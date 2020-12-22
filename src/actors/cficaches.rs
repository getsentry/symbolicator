use std::fs::File;
use std::io::{self, BufWriter};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures::compat::Future01CompatExt;
use futures::prelude::*;
use sentry::{configure_scope, Hub, SentryFutureExt};
use symbolic::{
    common::ByteView,
    minidump::cfi::{self, CfiCache},
};
use thiserror::Error;

use crate::actors::common::cache::{CacheItemRequest, CachePath, Cacher};
use crate::actors::objects::{
    FindObject, ObjectError, ObjectHandle, ObjectMetaHandle, ObjectPurpose, ObjectsActor,
};
use crate::cache::{Cache, CacheKey, CacheStatus};
use crate::sources::{FileType, SourceConfig};
use crate::types::{
    AllObjectCandidates, ObjectFeatures, ObjectId, ObjectType, ObjectUseInfo, Scope,
};
use crate::utils::futures::{BoxedFuture, ThreadPool};
use crate::utils::sentry::WriteSentryScope;

/// Errors happening while generating a cficache
#[derive(Debug, Error)]
pub enum CfiCacheError {
    #[error("failed to download")]
    Io(#[from] io::Error),

    #[error("failed to download object")]
    Fetching(#[source] ObjectError),

    #[error("failed to parse cficache")]
    Parsing(#[from] cfi::CfiError),

    #[error("failed to parse object")]
    ObjectParsing(#[source] ObjectError),

    #[error("cficache building took too long")]
    Timeout,

    #[error("computation was canceled internally")]
    Canceled,
}

#[derive(Clone, Debug)]
pub struct CfiCacheActor {
    cficaches: Arc<Cacher<FetchCfiCacheInternal>>,
    objects: ObjectsActor,
    threadpool: ThreadPool,
}

impl CfiCacheActor {
    pub fn new(cache: Cache, objects: ObjectsActor, threadpool: ThreadPool) -> Self {
        CfiCacheActor {
            cficaches: Arc::new(Cacher::new(cache)),
            objects,
            threadpool,
        }
    }
}

#[derive(Debug)]
pub struct CfiCacheFile {
    object_type: ObjectType,
    identifier: ObjectId,
    scope: Scope,
    data: ByteView<'static>,
    features: ObjectFeatures,
    status: CacheStatus,
    path: CachePath,
    candidates: AllObjectCandidates,
}

impl CfiCacheFile {
    /// Returns the status of this cache file.
    pub fn status(&self) -> CacheStatus {
        self.status
    }

    /// Returns the features of the object file this symcache was constructed from.
    pub fn features(&self) -> ObjectFeatures {
        self.features
    }

    /// Returns the path at which this cache file is stored.
    pub fn path(&self) -> &Path {
        self.path.as_ref()
    }

    /// Returns all the DIF object candidates.
    pub fn candidates(&self) -> &AllObjectCandidates {
        &self.candidates
    }
}

#[derive(Clone, Debug)]
struct FetchCfiCacheInternal {
    request: FetchCfiCache,
    objects_actor: ObjectsActor,
    meta_handle: Arc<ObjectMetaHandle>,
    candidates: AllObjectCandidates,
    threadpool: ThreadPool,
}

impl CacheItemRequest for FetchCfiCacheInternal {
    type Item = CfiCacheFile;
    type Error = CfiCacheError;

    fn get_cache_key(&self) -> CacheKey {
        self.meta_handle.cache_key()
    }

    /// Extracts the Call Frame Information (CFI) from an object file.
    ///
    /// The extracted CFI is written to `path` in symbolic's
    /// [`CfiCache`](symbolic::minidump::cfi::CfiCache) format.
    fn compute(&self, path: &Path) -> BoxedFuture<Result<CacheStatus, Self::Error>> {
        let objects_actor = self.objects_actor.clone();
        let meta_handle = self.meta_handle.clone();
        let threadpool = self.threadpool.clone();
        let path = path.to_owned();

        let result = async move {
            let object_handle = objects_actor
                .fetch(meta_handle.clone())
                .await
                .map_err(CfiCacheError::Fetching)?;

            let future = async move {
                let status = match object_handle.status() {
                    CacheStatus::Positive => match write_cficache(&path, &*object_handle) {
                        Ok(()) => CacheStatus::Positive,
                        Err(err) => {
                            log::warn!("Could not write cficache: {}", err);
                            sentry::capture_error(&err);
                            CacheStatus::Malformed
                        }
                    },
                    cache_status => cache_status,
                };
                Ok(status)
            };

            match threadpool
                .spawn_handle(future.bind_hub(Hub::current()))
                .await
            {
                Ok(result) => result,
                Err(_) => Err(CfiCacheError::Canceled),
            }
        }
        .boxed_local();

        // TODO(flub): Implement future_metrics! in futures 0.3.
        let num_sources = self.request.sources.len();
        Box::pin(
            future_metrics!(
                "cficaches",
                Some((Duration::from_secs(1200), CfiCacheError::Timeout)),
                result.compat(),
                "num_sources" => &num_sources.to_string()
            )
            .compat(),
        )
    }

    fn should_load(&self, data: &[u8]) -> bool {
        CfiCache::from_bytes(ByteView::from_slice(data))
            .map(|cficache| cficache.is_latest())
            .unwrap_or(false)
    }

    fn load(
        &self,
        scope: Scope,
        status: CacheStatus,
        data: ByteView<'static>,
        path: CachePath,
    ) -> Self::Item {
        let unwind = match status {
            CacheStatus::Positive => ObjectUseInfo::Ok,
            CacheStatus::Negative => {
                if self.meta_handle.status() == CacheStatus::Positive {
                    ObjectUseInfo::Error {
                        details: String::from("Object file no longer available"),
                    }
                } else {
                    // No need to pretend that we were going to use this cficache if the
                    // original object file was already not there, that status is already
                    // reported.
                    ObjectUseInfo::None
                }
            }
            CacheStatus::Malformed => ObjectUseInfo::Malformed,
        };
        let mut candidates = self.candidates.clone();
        candidates.set_unwind(
            self.meta_handle.source().clone(),
            self.meta_handle.location(),
            unwind,
        );

        CfiCacheFile {
            object_type: self.request.object_type,
            identifier: self.request.identifier.clone(),
            scope,
            data,
            features: self.meta_handle.features(),
            status,
            path,
            candidates,
        }
    }
}

/// Information for fetching the symbols for this cficache
#[derive(Debug, Clone)]
pub struct FetchCfiCache {
    pub object_type: ObjectType,
    pub identifier: ObjectId,
    pub sources: Arc<[SourceConfig]>,
    pub scope: Scope,
}

impl CfiCacheActor {
    /// Fetches the CFI cache file for a given code module.
    ///
    /// The code object can be identified by a combination of the code-id, debug-id and
    /// debug filename (the basename).  To do this it looks in the existing cache with the
    /// given scope and if it does not yet exist in cached form will fetch the required DIFs
    /// and compute the required CFI cache file.
    pub async fn fetch(
        &self,
        request: FetchCfiCache,
    ) -> Result<Arc<CfiCacheFile>, Arc<CfiCacheError>> {
        let found_result = self
            .objects
            .clone()
            .find(FindObject {
                filetypes: FileType::from_object_type(request.object_type),
                identifier: request.identifier.clone(),
                sources: request.sources.clone(),
                scope: request.scope.clone(),
                purpose: ObjectPurpose::Unwind,
            })
            .map_err(|e| Arc::new(CfiCacheError::Fetching(e)))
            .await?;

        match found_result.meta {
            Some(meta_handle) => {
                self.cficaches
                    .compute_memoized(FetchCfiCacheInternal {
                        request,
                        objects_actor: self.objects.clone(),
                        meta_handle,
                        threadpool: self.threadpool.clone(),
                        candidates: found_result.candidates,
                    })
                    .await
            }
            None => Ok(Arc::new(CfiCacheFile {
                object_type: request.object_type,
                identifier: request.identifier,
                scope: request.scope,
                data: ByteView::from_slice(b""),
                features: ObjectFeatures::default(),
                status: CacheStatus::Negative,
                path: CachePath::new(),
                candidates: found_result.candidates,
            })),
        }
    }
}

/// Extracts the CFI from an object file, writing it to a CFI file.
///
/// The source file is probably an executable or so, the resulting file is in the format of
/// [symbolic::minidump::cfi::CfiCache].
fn write_cficache(path: &Path, object_file: &ObjectHandle) -> Result<(), CfiCacheError> {
    configure_scope(|scope| {
        scope.set_transaction(Some("compute_cficache"));
        object_file.write_sentry_scope(scope);
    });

    let object = object_file
        .parse()
        .map_err(CfiCacheError::ObjectParsing)?
        .unwrap();

    let file = File::create(&path)?;
    let writer = BufWriter::new(file);

    log::debug!("Converting cficache for {}", object_file.cache_key());

    CfiCache::from_object(&object)?.write_to(writer)?;

    Ok(())
}
