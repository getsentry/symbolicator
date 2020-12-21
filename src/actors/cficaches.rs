use std::fs::File;
use std::io::{self, BufWriter};
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::compat::Future01CompatExt;
use futures::future::{self, Either};
use futures::{Future, FutureExt, TryFutureExt};
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
use crate::types::{ObjectFeatures, ObjectId, ObjectType, Scope};
use crate::utils::futures::ThreadPool;
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
}

#[derive(Clone, Debug)]
struct FetchCfiCacheInternal {
    request: FetchCfiCache,
    objects_actor: ObjectsActor,
    object_meta: Arc<ObjectMetaHandle>,
    threadpool: ThreadPool,
}

impl CacheItemRequest for FetchCfiCacheInternal {
    type Item = CfiCacheFile;
    type Error = CfiCacheError;

    fn get_cache_key(&self) -> CacheKey {
        self.object_meta.cache_key()
    }

    /// Extracts the Call Frame Information (CFI) from an object file.
    ///
    /// The extracted CFI is written to `path` in symbolic's
    /// [`CfiCache`](symbolic::minidump::cfi::CfiCache) format.
    fn compute(
        &self,
        path: &Path,
    ) -> Pin<Box<dyn Future<Output = Result<CacheStatus, Self::Error>>>> {
        let path = path.to_owned();
        let object = self
            .objects_actor
            .fetch(self.object_meta.clone())
            .map_err(CfiCacheError::Fetching);

        let threadpool = self.threadpool.clone();
        let result = object.and_then(move |object| {
            let future = future::lazy(move |_| {
                if object.status() != CacheStatus::Positive {
                    return Ok(object.status());
                }

                let status = if let Err(e) = write_cficache(&path, &*object) {
                    log::warn!("Could not write cficache: {}", e);
                    sentry::capture_error(&e);

                    CacheStatus::Malformed
                } else {
                    CacheStatus::Positive
                };

                Ok(status)
            });

            threadpool
                .spawn_handle(future.bind_hub(Hub::current()))
                .unwrap_or_else(|_| Err(CfiCacheError::Canceled))
        });

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
        CfiCacheFile {
            object_type: self.request.object_type,
            identifier: self.request.identifier.clone(),
            scope,
            data,
            features: self.object_meta.features(),
            status,
            path,
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
    pub fn fetch(
        &self,
        request: FetchCfiCache,
    ) -> Pin<Box<dyn Future<Output = Result<Arc<CfiCacheFile>, Arc<CfiCacheError>>>>> {
        let object = self
            .objects
            .clone()
            .find(FindObject {
                filetypes: FileType::from_object_type(request.object_type),
                identifier: request.identifier.clone(),
                sources: request.sources.clone(),
                scope: request.scope.clone(),
                purpose: ObjectPurpose::Unwind,
            })
            .map_err(|e| Arc::new(CfiCacheError::Fetching(e)));

        let cficaches = self.cficaches.clone();
        let threadpool = self.threadpool.clone();
        let objects = self.objects.clone();

        let object_type = request.object_type;
        let identifier = request.identifier.clone();
        let scope = request.scope.clone();

        object
            .and_then(move |object| {
                object
                    .meta
                    .map(move |object_meta| {
                        Either::Left(cficaches.compute_memoized(FetchCfiCacheInternal {
                            request,
                            objects_actor: objects,
                            object_meta,
                            threadpool,
                        }))
                    })
                    .unwrap_or_else(move || {
                        Either::Right(future::ok(Arc::new(CfiCacheFile {
                            object_type,
                            identifier,
                            scope,
                            data: ByteView::from_slice(b""),
                            features: ObjectFeatures::default(),
                            status: CacheStatus::Negative,
                            path: CachePath::new(),
                        })))
                    })
            })
            .boxed_local()
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
