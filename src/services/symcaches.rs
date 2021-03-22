use std::fs::File;
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Error;
use futures::compat::Future01CompatExt;
use futures::future::{FutureExt, TryFutureExt};
use sentry::{configure_scope, Hub, SentryFutureExt};
use symbolic::common::{Arch, ByteView};
use symbolic::debuginfo::Object;
use symbolic::symcache::{self, SymCache, SymCacheWriter};
use thiserror::Error;

use crate::cache::{Cache, CacheKey, CacheStatus};
use crate::services::bitcode::{BCSymbolMapHandle, BitcodeService};
use crate::services::cacher::{CacheItemRequest, CachePath, Cacher};
use crate::services::objects::{
    FindObject, FoundObject, ObjectError, ObjectHandle, ObjectMetaHandle, ObjectPurpose,
    ObjectsActor,
};
use crate::sources::{FileType, SourceConfig};
use crate::types::{
    AllObjectCandidates, ObjectFeatures, ObjectId, ObjectType, ObjectUseInfo, Scope,
};
use crate::utils::futures::{BoxedFuture, ThreadPool};
use crate::utils::sentry::ConfigureScope;

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

    #[error("failed to handle auxiliary BCSymbolMap file")]
    BCSymbolMapError(#[source] Error),

    #[error("symcache building took too long")]
    Timeout,

    #[error("computation was canceled internally")]
    Canceled,
}

#[derive(Clone, Debug)]
pub struct SymCacheActor {
    symcaches: Arc<Cacher<FetchSymCacheInternal>>,
    objects: ObjectsActor,
    bitcode_svc: BitcodeService,
    threadpool: ThreadPool,
}

impl SymCacheActor {
    pub fn new(
        cache: Cache,
        objects: ObjectsActor,
        bitcode_svc: BitcodeService,
        threadpool: ThreadPool,
    ) -> Self {
        SymCacheActor {
            symcaches: Arc::new(Cacher::new(cache)),
            objects,
            bitcode_svc,
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
    /// The external request, as passed into [`SymCacheActor::fetch`].
    request: FetchSymCache,

    /// The objects actor, used to fetch original DIF objects from.
    objects_actor: ObjectsActor,

    /// The bitcode service, use to fetch [`BCSymbolMap`].
    bitcode_svc: BitcodeService,

    /// ObjectMeta handle of the original DIF object to fetch.
    object_meta: Arc<ObjectMetaHandle>,

    /// Thread pool on which to spawn the symcache computation.
    threadpool: ThreadPool,

    /// The object candidates from which [`FetchSymCacheInternal::object_meta`] was chosen.
    ///
    /// This needs to be returned back with the symcache result and is only being passed
    /// through here as callers to the SymCacheActer want to have this info.
    candidates: AllObjectCandidates,
}

/// Fetches the needed DIF objects and spawns symcache computation.
///
/// Required DIF objects are fetched from the objects actor in the current executor, once
/// DIFs have been retrieved it spawns the symcache computation onto the provided
/// threadpool.
///
/// This is the actual implementation of [`CacheItemRequest::compute`] for
/// [`FetchSymCacheInternal`] but outside of the trait so it can be written as async/await
/// code.
async fn fetch_difs_and_compute_symcache(
    path: PathBuf,
    object_meta: Arc<ObjectMetaHandle>,
    sources: Arc<[SourceConfig]>,
    objects_actor: ObjectsActor,
    bitcode_svc: BitcodeService,
    threadpool: ThreadPool,
) -> Result<CacheStatus, SymCacheError> {
    let object_handle = objects_actor
        .fetch(object_meta.clone())
        .await
        .map_err(SymCacheError::Fetching)?;

    if object_handle.status() != CacheStatus::Positive {
        return Ok(object_handle.status());
    }

    let bcsymbolmap_handle = match object_meta.object_id().debug_id {
        Some(debug_id) => bitcode_svc
            .fetch_bcsymbolmap(debug_id, object_meta.scope().clone(), sources.clone())
            .await
            .map_err(SymCacheError::BCSymbolMapError)?,
        None => None,
    };

    let compute_future = async move {
        let status = match write_symcache(&path, &*object_handle, bcsymbolmap_handle) {
            Ok(_) => CacheStatus::Positive,
            Err(err) => {
                log::warn!("Failed to write symcache: {}", err);
                sentry::capture_error(&err);
                CacheStatus::Malformed
            }
        };
        Ok(status)
    };

    threadpool
        .spawn_handle(compute_future.bind_hub(Hub::current()))
        .await
        .unwrap_or(Err(SymCacheError::Canceled))
}

impl CacheItemRequest for FetchSymCacheInternal {
    type Item = SymCacheFile;
    type Error = SymCacheError;

    fn get_cache_key(&self) -> CacheKey {
        self.object_meta.cache_key()
    }

    fn compute(&self, path: &Path) -> BoxedFuture<Result<CacheStatus, Self::Error>> {
        let future = fetch_difs_and_compute_symcache(
            path.to_owned(),
            self.object_meta.clone(),
            self.request.sources.clone(),
            self.objects_actor.clone(),
            self.bitcode_svc.clone(),
            self.threadpool.clone(),
        );

        let num_sources = self.request.sources.len();

        Box::pin(
            future_metrics!(
                "symcaches",
                Some((Duration::from_secs(1200), SymCacheError::Timeout)),
                future.boxed_local().compat(),
                "num_sources" => &num_sources.to_string()
            )
            .compat(),
        )
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

        let mut candidates = self.candidates.clone(); // yuk!
        candidates.set_debug(
            self.object_meta.source_id().clone(),
            &self.object_meta.uri(),
            ObjectUseInfo::from_derived_status(status, self.object_meta.status()),
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
    pub async fn fetch(
        &self,
        request: FetchSymCache,
    ) -> Result<Arc<SymCacheFile>, Arc<SymCacheError>> {
        let FoundObject { meta, candidates } = self
            .objects
            .find(FindObject {
                filetypes: FileType::from_object_type(request.object_type),
                identifier: request.identifier.clone(),
                sources: request.sources.clone(),
                scope: request.scope.clone(),
                purpose: ObjectPurpose::Debug,
            })
            .await
            .map_err(|e| Arc::new(SymCacheError::Fetching(e)))?;

        match meta {
            Some(handle) => {
                self.symcaches
                    .compute_memoized(FetchSymCacheInternal {
                        request,
                        objects_actor: self.objects.clone(),
                        bitcode_svc: self.bitcode_svc.clone(),
                        object_meta: handle,
                        threadpool: self.threadpool.clone(),
                        candidates,
                    })
                    .await
            }
            None => Ok(Arc::new(SymCacheFile {
                object_type: request.object_type,
                identifier: request.identifier,
                scope: request.scope,
                data: ByteView::from_slice(b""),
                features: ObjectFeatures::default(),
                status: CacheStatus::Negative,
                arch: Arch::Unknown,
                candidates,
            })),
        }
    }
}

/// Computes and writes the symcache.
///
/// It is assumed both the `object_handle` and `bcsymbolmap_handle` contain positive caches.
fn write_symcache(
    path: &Path,
    object_handle: &ObjectHandle,
    bcsymbolmap_handle: Option<BCSymbolMapHandle>,
) -> Result<(), SymCacheError> {
    configure_scope(|scope| {
        scope.set_transaction(Some("compute_symcache"));
        object_handle.to_scope(scope);
    });

    let mut symbolic_object = object_handle
        .parse()
        .map_err(SymCacheError::ObjectParsing)?
        .unwrap();
    if let Object::MachO(ref mut macho) = symbolic_object {
        if let Some(handle) = bcsymbolmap_handle {
            let bcsymbolmap = handle
                .bc_symbol_map()
                .map_err(SymCacheError::BCSymbolMapError)?;
            log::debug!(
                "Adding BCSymbolMap {} to dSYM {}",
                handle.uuid,
                object_handle
            );
            macho.load_symbolmap(bcsymbolmap);
        }
    }

    let file = File::create(&path)?;
    let mut writer = BufWriter::new(file);

    log::debug!("Converting symcache for {}", object_handle.cache_key());

    SymCacheWriter::write_object(&symbolic_object, &mut writer).map_err(SymCacheError::Writing)?;

    let file = writer.into_inner().map_err(io::Error::from)?;
    file.sync_all()?;

    Ok(())
}
