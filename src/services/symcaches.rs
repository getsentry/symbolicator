use std::fs::File;
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::compat::Future01CompatExt;
use futures::future::{FutureExt, TryFutureExt};
use sentry::{configure_scope, Hub, SentryFutureExt};
use symbolic::common::{Arch, ByteView, DebugId};
use symbolic::debuginfo::bcsymbolmap::{BCSymbolMap, BCSymbolMapError};
use symbolic::debuginfo::plist::{PList, PListError};
use symbolic::debuginfo::Object;
use symbolic::symcache::{self, SymCache, SymCacheWriter};
use thiserror::Error;

use crate::cache::{Cache, CacheKey, CacheStatus};
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

    #[error("failed to handle auxiliary PList file")]
    PListError(#[from] PListError),

    #[error("failed to handle auxiliary BCSymbolMap file")]
    BCSymbolMapError(#[from] BCSymbolMapError),

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
    /// The external request, as passed into [`SymCacheActor::fetch`].
    request: FetchSymCache,

    /// The objects actor, used to fetch original DIF objects from.
    objects_actor: ObjectsActor,

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
        Some(ref debug_id) => fetch_bcsymbolmap(
            debug_id.clone(),
            object_meta.scope().clone(),
            sources.clone(),
            &objects_actor,
        )
        .await?
        .map(|bcsym| (debug_id.clone(), bcsym)),
        None => None,
    };
    let bcsymbolmap_handle = dbg!(bcsymbolmap_handle);

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

async fn fetch_bcsymbolmap(
    uuid: DebugId,
    scope: Scope,
    sources: Arc<[SourceConfig]>,
    objects_actor: &ObjectsActor,
) -> Result<Option<Arc<ObjectHandle>>, SymCacheError> {
    println!("fetch_bcsymbolmap");
    let plist_search = FindObject {
        filetypes: &[FileType::PList],
        purpose: ObjectPurpose::Debug,
        scope: scope.clone(),
        identifier: uuid.clone().into(),
        sources: sources.clone(),
    };
    println!("fetch_bcsymbolmap 2");
    let found = dbg!(objects_actor
        .find(plist_search)
        .await
        .map_err(SymCacheError::Fetching)?);
    println!("fetch_bcsymbolmap 3");
    let meta_handle = match found.meta {
        Some(handle) => handle,
        None => {
            return Ok(None);
        }
    };
    println!("fetch_bcsymbolmap 4");
    let handle = objects_actor
        .fetch(meta_handle)
        .await
        .map_err(SymCacheError::Fetching)?;
    println!("fetch_bcsymbolmap 5");
    if dbg!(handle.status()) != CacheStatus::Positive {
        return Ok(None);
    }
    println!("fetch_bcsymbolmap 6");
    let plist = PList::parse(uuid, &handle.data())?;
    println!("fetch_bcsymbolmap 7");
    let original_uuid = match plist.original_uuid() {
        Ok(Some(uuid)) => uuid,
        _ => {
            // This should not be possible, they only get written if this is valid.
            sentry::capture_message(
                "PList did not contain valid BCSymbolMap UUID mapping",
                sentry::Level::Error,
            );
            return Ok(None);
        }
    };
    println!("Ok, got the plist");

    let bcsymbolmap_id = ObjectId::from(original_uuid);
    let bcsymbolmap_search = FindObject {
        filetypes: &[FileType::BCSymbolMap],
        purpose: ObjectPurpose::Debug,
        scope,
        identifier: bcsymbolmap_id,
        sources,
    };
    let found = dbg!(objects_actor
        .find(bcsymbolmap_search)
        .await
        .map_err(SymCacheError::Fetching)?);
    let meta_handle = match found.meta {
        Some(handle) => handle,
        None => {
            return Ok(None);
        }
    };
    println!("fetching bcsymbolmap");
    objects_actor
        .fetch(meta_handle)
        .await
        .map(|handle| Some(handle))
        .map_err(SymCacheError::Fetching)
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
    bcsymbolmap_handle: Option<(DebugId, Arc<ObjectHandle>)>,
) -> Result<(), SymCacheError> {
    configure_scope(|scope| {
        scope.set_transaction(Some("compute_symcache"));
        object_handle.to_scope(scope);
    });

    let mut symbolic_object = object_handle
        .parse()
        .map_err(SymCacheError::ObjectParsing)?
        .unwrap();
    println!("write_symcache: {:?}", bcsymbolmap_handle);
    if let Object::MachO(ref mut macho) = symbolic_object {
        if let Some((code_id, symbolmap_handle)) = bcsymbolmap_handle {
            let bcsymbolmap = BCSymbolMap::parse(code_id, symbolmap_handle.data().as_slice())?;
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
