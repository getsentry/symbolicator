use std::fs::File;
use std::io::{self, BufWriter};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures::{compat::Future01CompatExt, FutureExt, TryFutureExt};
use futures01::future::{Either, Future, IntoFuture};
use sentry::configure_scope;
use symbolic::common::{Arch, ByteView};
use symbolic::symcache::{self, SymCache, SymCacheWriter};
use thiserror::Error;

use crate::actors::common::cache::{CacheItemRequest, CachePath, Cacher};
use crate::actors::objects::{
    FindObject, FoundObject, ObjectError, ObjectHandle, ObjectMetaHandle, ObjectPurpose,
    ObjectsActor,
};
use crate::cache::{Cache, CacheKey, CacheStatus};
use crate::sources::{FileType, SourceConfig};
use crate::types::{
    AllObjectCandidates, ObjectFeatures, ObjectId, ObjectType, ObjectUseInfo, Scope,
};
use crate::utils::futures::ThreadPool;
use crate::utils::sentry::{SentryFutureExt, WriteSentryScope};

/// Errors happening while generating a symcache.
#[derive(Debug, Error)]
pub enum SymCacheError {
    #[error("failed to write symcache")]
    Io(#[from] io::Error),

    #[error("failed to download object")]
    Fetching(#[source] ObjectError),

    #[error("failed to parse symcache")]
    Parsing(#[source] symcache::SymCacheError),

    #[error("failed to write symcache")]
    Writing(#[source] symcache::SymCacheError),

    #[error("malformed symcache file")]
    Malformed,

    #[error("failed to parse object")]
    ObjectParsing(#[source] ObjectError),

    #[error("symcache building took too long")]
    Timeout,

    #[error("computation was canceled internally")]
    Canceled,
}

#[derive(Clone, Debug)]
pub struct SymCacheActor {
    symcaches: Arc<Cacher<FetchSymCacheInternal>>,
    objects: ObjectsActor,
    threadpool: ThreadPool,
}

impl SymCacheActor {
    pub fn new(cache: Cache, objects: ObjectsActor, threadpool: ThreadPool) -> Self {
        SymCacheActor {
            symcaches: Arc::new(Cacher::new(cache)),
            objects,
            threadpool,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SymCacheFile {
    object_type: ObjectType,
    identifier: ObjectId,
    scope: Scope,
    data: ByteView<'static>,
    features: ObjectFeatures,
    status: CacheStatus,
    arch: Arch,
    candidates: AllObjectCandidates,
}

impl SymCacheFile {
    pub fn parse(&self) -> Result<Option<SymCache<'_>>, SymCacheError> {
        match self.status {
            CacheStatus::Positive => Ok(Some(
                SymCache::parse(&self.data).map_err(SymCacheError::Parsing)?,
            )),
            CacheStatus::Negative => Ok(None),
            CacheStatus::Malformed => Err(SymCacheError::Malformed),
        }
    }

    /// Returns the architecture of this symcache.
    pub fn arch(&self) -> Arch {
        self.arch
    }

    /// Returns the features of the object file this symcache was constructed from.
    pub fn features(&self) -> ObjectFeatures {
        self.features
    }

    /// Returns the list of DIFs which were searched for this symcache.
    pub fn candidates(&self) -> AllObjectCandidates {
        self.candidates.clone()
    }
}

#[derive(Clone, Debug)]
struct FetchSymCacheInternal {
    request: FetchSymCache,
    objects_actor: ObjectsActor,
    object_meta: Arc<ObjectMetaHandle>,
    threadpool: ThreadPool,
    candidates: AllObjectCandidates,
}

impl CacheItemRequest for FetchSymCacheInternal {
    type Item = SymCacheFile;
    type Error = SymCacheError;

    fn get_cache_key(&self) -> CacheKey {
        self.object_meta.cache_key()
    }

    fn compute(&self, path: &Path) -> Box<dyn Future<Item = CacheStatus, Error = Self::Error>> {
        let path = path.to_owned();
        let object = self
            .objects_actor
            .fetch(self.object_meta.clone())
            .compat()
            .map_err(SymCacheError::Fetching);

        let threadpool = self.threadpool.clone();
        let result = object.and_then(move |object| {
            let future = futures01::lazy(move || {
                if object.status() != CacheStatus::Positive {
                    return Ok(object.status());
                }

                let status = if let Err(e) = write_symcache(&path, &*object) {
                    log::warn!("Failed to write symcache: {}", e);
                    sentry::capture_error(&e);

                    CacheStatus::Malformed
                } else {
                    CacheStatus::Positive
                };

                Ok(status)
            });

            threadpool
                .spawn_handle(future.sentry_hub_current().compat())
                .boxed_local()
                .compat()
                .map_err(|_| SymCacheError::Canceled)
                .flatten()
        });

        let num_sources = self.request.sources.len();

        Box::new(future_metrics!(
            "symcaches",
            Some((Duration::from_secs(1200), SymCacheError::Timeout)),
            result,
            "num_sources" => &num_sources.to_string()
        ))
    }

    fn should_load(&self, data: &[u8]) -> bool {
        SymCache::parse(data)
            .map(|symcache| symcache.is_latest())
            .unwrap_or(false)
    }

    fn load(
        &self,
        scope: Scope,
        status: CacheStatus,
        data: ByteView<'static>,
        _: CachePath,
    ) -> Self::Item {
        // TODO: Figure out if this double-parsing could be avoided
        let arch = SymCache::parse(&data)
            .map(|cache| cache.arch())
            .unwrap_or_default();

        // If self.object_meta.status() was != Positive than that status got passed straight
        // through to our own `status` argument.
        let debug = match status {
            CacheStatus::Positive => ObjectUseInfo::Ok,
            CacheStatus::Negative => {
                if self.object_meta.status() == CacheStatus::Positive {
                    ObjectUseInfo::Error {
                        details: String::from("Object file no longer available"),
                    }
                } else {
                    // No need to pretend that we were going to use this symcache if the
                    // original object file was already not there, that status is already
                    // reported.
                    ObjectUseInfo::None
                }
            }
            CacheStatus::Malformed => ObjectUseInfo::Malformed,
        };
        let mut candidates = self.candidates.clone(); // yuk!
        candidates.set_debug(
            self.object_meta.source().clone(),
            self.object_meta.location(),
            debug,
        );

        SymCacheFile {
            object_type: self.request.object_type,
            identifier: self.request.identifier.clone(),
            scope,
            data,
            features: self.object_meta.features(),
            status,
            arch,
            candidates,
        }
    }
}

/// Information for fetching the symbols for this symcache
#[derive(Debug, Clone)]
pub struct FetchSymCache {
    pub object_type: ObjectType,
    pub identifier: ObjectId,
    pub sources: Arc<[SourceConfig]>,
    pub scope: Scope,
}

impl SymCacheActor {
    pub fn fetch(
        &self,
        request: FetchSymCache,
    ) -> impl Future<Item = Arc<SymCacheFile>, Error = Arc<SymCacheError>> {
        let find_result_future = self
            .objects
            .clone()
            .find(FindObject {
                filetypes: FileType::from_object_type(request.object_type),
                identifier: request.identifier.clone(),
                sources: request.sources.clone(),
                scope: request.scope.clone(),
                purpose: ObjectPurpose::Debug,
            })
            .boxed_local()
            .compat()
            .map_err(|e| Arc::new(SymCacheError::Fetching(e)));

        let symcaches = self.symcaches.clone();
        let threadpool = self.threadpool.clone();
        let objects = self.objects.clone();

        let object_type = request.object_type;
        let identifier = request.identifier.clone();
        let scope = request.scope.clone();

        find_result_future.and_then(move |find_result: FoundObject| {
            let FoundObject { meta, candidates } = find_result;
            meta.map(clone!(candidates, |object_meta| {
                Either::A(symcaches.compute_memoized(FetchSymCacheInternal {
                    request,
                    objects_actor: objects,
                    object_meta,
                    threadpool,
                    candidates,
                }))
            }))
            .unwrap_or_else(move || {
                Either::B(
                    Ok(Arc::new(SymCacheFile {
                        object_type,
                        identifier,
                        scope,
                        data: ByteView::from_slice(b""),
                        features: ObjectFeatures::default(),
                        status: CacheStatus::Negative,
                        arch: Arch::Unknown,
                        candidates,
                    }))
                    .into_future(),
                )
            })
        })
    }
}

fn write_symcache(path: &Path, object: &ObjectHandle) -> Result<(), SymCacheError> {
    configure_scope(|scope| {
        scope.set_transaction(Some("compute_symcache"));
        object.write_sentry_scope(scope);
    });

    let symbolic_object = object
        .parse()
        .map_err(SymCacheError::ObjectParsing)?
        .unwrap();

    let file = File::create(&path)?;
    let mut writer = BufWriter::new(file);

    log::debug!("Converting symcache for {}", object.cache_key());

    SymCacheWriter::write_object(&symbolic_object, &mut writer).map_err(SymCacheError::Writing)?;

    let file = writer.into_inner().map_err(io::Error::from)?;
    file.sync_all()?;

    Ok(())
}
