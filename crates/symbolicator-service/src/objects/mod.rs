use std::collections::BTreeSet;
use std::sync::Arc;

use futures::future;
use sentry::{Hub, SentryFutureExt};

use symbolic::common::ByteView;
use symbolicator_sources::{FileType, ObjectId, RemoteFile, RemoteFileUri, SourceConfig, SourceId};

use crate::caching::{Cache, CacheContents, CacheError, CacheKey, CacheKeyBuilder, Cacher, SharedCacheRef};
use crate::download::{self, DownloadService, SourceIndexService};
use crate::types::Scope;

mod cab_synth_cache;
mod candidates;
mod data_cache;
mod meta_cache;
mod raw_compressed_cache;

use cab_synth_cache::{CabSynthRequest, cab_synth_cache_key};
use data_cache::FetchFileDataRequest;
use meta_cache::FetchFileMetaRequest;
use raw_compressed_cache::{RawCompressedRequest, raw_compressed_cache_key};

pub use candidates::*;
pub use data_cache::ObjectHandle;
pub use meta_cache::ObjectMetaHandle;

/// Builds a [`CacheKeyBuilder`] scoped to `(scope, file_source)` with a trailing
/// `discriminator` line, used to namespace per-role caches that share an underlying
/// `RemoteFile` identity with the primary `objects` cache (raw-compressed mirror,
/// synthesized CAB envelopes, etc.).
fn role_scoped_cache_key(
    scope: &Scope,
    file_source: &RemoteFile,
    discriminator: &str,
) -> CacheKeyBuilder {
    use std::fmt::Write as _;
    let mut builder = CacheKey::scoped_builder(scope);
    builder.write_file_meta(file_source).unwrap();
    builder.write_str(discriminator).unwrap();
    builder
}

/// Wrapper around [`CacheError`] to also pass the file information along.
///
/// Because of the requirement of [`CacheItemRequest`] to impl `From<io::Error>` it can not
/// itself contain the [`SourceId`] and [`SourceLocation`].  However we need to carry this
/// along some errors, so we use this wrapper.
///
/// [`CacheItemRequest`]: crate::caching::CacheItemRequest
/// [`SourceLocation`]: crate::download::SourceLocation
#[derive(Clone, Debug)]
pub struct CacheLookupError {
    /// The object file which was attempted to be fetched.
    pub file_source: RemoteFile,
    /// The wrapped [`CacheError`] which occurred while fetching the object file.
    pub error: CacheError,
}

/// Fetch a Object from external sources or internal cache.
#[derive(Debug, Clone)]
pub struct FindObject {
    pub filetypes: &'static [FileType],
    pub purpose: ObjectPurpose,
    pub scope: Scope,
    pub identifier: ObjectId,
    pub sources: Arc<[SourceConfig]>,
}

#[derive(Debug, Copy, Clone)]
pub enum ObjectPurpose {
    Unwind,
    Debug,
    Source,
}

#[derive(Debug, Clone)]
pub struct FoundMeta {
    pub file_source: RemoteFile,
    pub handle: CacheContents<Arc<ObjectMetaHandle>>,
}

/// The response for [`ObjectsActor::find`].
///
/// The found object is in the `selected_meta` field with other DIFs considered
/// in the `candidates` field.
#[derive(Debug, Clone)]
pub struct FindResult {
    /// If a matching object (or an error as fallback) was found,
    /// its [`ObjectMetaHandle`] will be provided here, otherwise this will be `None`
    pub meta: Option<FoundMeta>,
    /// This is a list of some meta information on all objects which have been considered
    /// for this object.  It could be populated even if no matching object is found.
    pub candidates: AllObjectCandidates,
}

#[derive(Clone, Debug)]
pub struct ObjectsActor {
    // FIXME(swatinem): Having a fully fledged filesystem and shared cache for these tiny file meta
    // items is heavy handed and wasteful. However, we *do* want to have this in shared cache, as
    // it is the primary thing that makes cold starts fast, as we do not need to fetch the whole
    // objects, but just the derived caches. Some lighter weight solution, like Redis might be more
    // appropriate at some point.
    meta_cache: Arc<Cacher<FetchFileMetaRequest>>,
    data_cache: Arc<Cacher<FetchFileDataRequest>>,
    /// Mirror of the upstream-compressed payload tee'd during downloads. Populated as a side
    /// effect of `data_cache` compute when `compressed_proxy` is on; used by the proxy to
    /// serve `.pd_`/`.dl_`/`.ex_` byte-identically.
    raw_compressed_cache: Arc<Cacher<RawCompressedRequest>>,
    /// On-demand CAB (MSZIP) envelopes built from the decompressed object. Fallback for the
    /// compressed-proxy mode when no upstream raw copy is available.
    cab_synth_cache: Arc<Cacher<CabSynthRequest>>,
    /// Whether the compressed-proxy feature is enabled. Gates raw-bytes tee'ing during
    /// downloads and the proxy's compressed lookup path.
    compressed_proxy: bool,
    download_svc: Arc<DownloadService>,
    source_index_svc: Arc<SourceIndexService>,
}

impl ObjectsActor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        meta_cache: Cache,
        data_cache: Cache,
        raw_compressed_cache: Cache,
        cab_synth_cache: Cache,
        compressed_proxy: bool,
        shared_cache: SharedCacheRef,
        download_svc: Arc<DownloadService>,
        source_index_svc: Arc<SourceIndexService>,
    ) -> Self {
        ObjectsActor {
            meta_cache: Arc::new(Cacher::new(meta_cache, Arc::clone(&shared_cache))),
            data_cache: Arc::new(Cacher::new(data_cache, Arc::clone(&shared_cache))),
            raw_compressed_cache: Arc::new(Cacher::new(
                raw_compressed_cache,
                Arc::clone(&shared_cache),
            )),
            cab_synth_cache: Arc::new(Cacher::new(cab_synth_cache, Arc::clone(&shared_cache))),
            compressed_proxy,
            download_svc,
            source_index_svc,
        }
    }

    /// Returns the requested object file.
    ///
    /// This fetches the requested object, re-downloading it from the source if it is no
    /// longer in the cache.
    #[tracing::instrument(skip_all, fields(file_source = ?file_handle.file_source))]
    pub async fn fetch(
        &self,
        file_handle: Arc<ObjectMetaHandle>,
    ) -> CacheContents<Arc<ObjectHandle>> {
        let cache_key = CacheKey::from_scoped_file(&file_handle.scope, &file_handle.file_source);
        let request = FetchFileDataRequest(self.make_meta_request(
            file_handle.scope.clone(),
            file_handle.file_source.clone(),
            file_handle.object_id.clone(),
        ));

        self.data_cache
            .compute_memoized(request, cache_key)
            .await
            .into_contents()
    }

    /// Returns a compressed (CAB-wrapped) form of the requested object file.
    ///
    /// Resolution order:
    ///
    /// 1. If [`Self::compressed_proxy`] is `false`, returns [`CacheError::NotFound`].
    /// 2. Otherwise, ensures the underlying object has been downloaded (which populates the
    ///    `raw_compressed` cache as a side effect when the upstream payload was compressed).
    /// 3. Looks up the `raw_compressed` mirror; on hit, returns those bytes byte-identically.
    /// 4. On miss, falls back to synthesizing a CAB envelope around the cached decompressed
    ///    object, caching it under `cab_synth` for subsequent requests.
    ///
    /// `inner_filename` is what tools extracting the CAB will see (must be the
    /// uncompressed name like `foo.pdb`).
    #[tracing::instrument(skip_all, fields(file_source = ?file_handle.file_source, inner_filename))]
    pub async fn fetch_compressed(
        &self,
        file_handle: Arc<ObjectMetaHandle>,
        inner_filename: &str,
    ) -> CacheContents<ByteView<'static>> {
        if !self.compressed_proxy {
            return Err(CacheError::NotFound);
        }

        // Ensure the underlying object has been fetched so that, if the source delivered a
        // compressed payload, it has been tee'd into the raw_compressed cache. Errors here
        // propagate so callers see the same NotFound the regular proxy path would return.
        self.fetch(file_handle.clone()).await?;

        // Try the upstream-bytes mirror first (cheap + byte-identical).
        let raw_key = raw_compressed_cache_key(&file_handle.scope, &file_handle.file_source);
        match self
            .raw_compressed_cache
            .compute_memoized(RawCompressedRequest, raw_key)
            .await
            .into_contents()
        {
            Ok(bytes) => return Ok(bytes),
            Err(CacheError::NotFound) => {
                // fall through to CAB synthesis
            }
            Err(other) => return Err(other),
        }

        // Fallback: synthesize a CAB envelope from the decompressed object.
        let synth_key = cab_synth_cache_key(
            &file_handle.scope,
            &file_handle.file_source,
            inner_filename,
        );
        let synth_request = CabSynthRequest {
            scope: file_handle.scope.clone(),
            file_source: file_handle.file_source.clone(),
            object_id: file_handle.object_id.clone(),
            inner_filename: inner_filename.to_owned(),
            data_cache: self.data_cache.clone(),
            download_svc: self.download_svc.clone(),
        };
        self.cab_synth_cache
            .compute_memoized(synth_request, synth_key)
            .await
            .into_contents()
    }

    /// Builds a [`FetchFileMetaRequest`] with this actor's shared state plumbed in.
    fn make_meta_request(
        &self,
        scope: Scope,
        file_source: RemoteFile,
        object_id: ObjectId,
    ) -> FetchFileMetaRequest {
        let raw_compressed_cache = if self.compressed_proxy {
            Some(self.raw_compressed_cache.clone())
        } else {
            None
        };
        FetchFileMetaRequest {
            scope,
            file_source,
            object_id,
            data_cache: self.data_cache.clone(),
            download_svc: self.download_svc.clone(),
            raw_compressed_cache,
        }
    }

    /// Fetches matching objects and returns the metadata of the most suitable object.
    ///
    /// This requests the available matching objects from the sources and then looks up the
    /// object metadata of each matching object in the metadata cache.  These are then
    /// ranked and the best matching object metadata is returned.
    ///
    /// Asking for the objects metadata from the data cache also triggers a download of each
    /// object, which will then be cached in the data cache.  The metadata itself is cached
    /// in the metadata cache which usually lives longer.
    pub async fn find(&self, request: FindObject) -> FindResult {
        let FindObject {
            filetypes,
            scope,
            identifier,
            sources,
            purpose,
        } = request;
        let file_ids = download::list_files(
            &self.download_svc,
            &self.source_index_svc,
            &sources,
            filetypes,
            &identifier,
        )
        .await;

        let file_metas = self.fetch_file_metas(file_ids, &identifier, scope).await;

        let candidates = create_candidates(&sources, &file_metas);
        let meta = select_meta(file_metas, purpose);

        FindResult { meta, candidates }
    }

    /// Fetch all [`ObjectMetaHandle`]s for the files.
    ///
    /// This concurrently looks up the file IDs in the meta-cache and returns all results.
    /// A custom [`CacheLookupError`] is returned to allow us to keep track of the source ID
    /// and source location in case of an error.  [`select_meta`] uses this to build the
    /// [`ObjectCandidate`] list.
    async fn fetch_file_metas(
        &self,
        file_sources: Vec<RemoteFile>,
        identifier: &ObjectId,
        scope: Scope,
    ) -> Vec<FoundMeta> {
        let queries = file_sources.into_iter().map(|file_source| {
            let scope = if file_source.is_public() {
                Scope::Global
            } else {
                scope.clone()
            };
            let cache_key = CacheKey::from_scoped_file(&scope, &file_source);
            let request = self.make_meta_request(scope, file_source.clone(), identifier.clone());

            async move {
                let handle = self
                    .meta_cache
                    .compute_memoized(request, cache_key)
                    .await
                    .into_contents();
                FoundMeta {
                    file_source,
                    handle,
                }
            }
            .bind_hub(Hub::new_from_top(Hub::current()))
        });

        future::join_all(queries).await
    }
}

/// Select the best [ObjectMetaHandle`] out of all lookups from the meta-cache.
///
/// The lookups are expected to be in order or preference, so if two files are equally good
/// the first one will be chosen.  If the file list is emtpy, `None` is returned in the
/// result, if there were no suitable files and only lookup errors one of the lookup errors
/// is propagated.  If there were no suitlable files and no errors `None` is also returned
/// in the result.
fn select_meta(all_lookups: Vec<FoundMeta>, purpose: ObjectPurpose) -> Option<FoundMeta> {
    let mut selected_meta = None;
    let mut selected_quality = u8::MAX;

    for meta_lookup in all_lookups {
        // Skip objects which and not suitable for what we're asked to provide.  Keep errors
        // though, if we don't find any object we need to return an error.
        if let Ok(ref meta_handle) = meta_lookup.handle
            && !object_has_features(meta_handle, purpose)
        {
            continue;
        }

        // We iterate in order of preferred sources, so only select a later object if the
        // quality is better.
        let quality = object_quality(&meta_lookup, purpose);
        if quality < selected_quality {
            selected_meta = Some(meta_lookup);
            selected_quality = quality;
        }
    }

    selected_meta
}

/// Returns a sortable quality measure of this object for the given purpose.
///
/// Lower quality number is better.
fn object_quality(meta_lookup: &FoundMeta, purpose: ObjectPurpose) -> u8 {
    match &meta_lookup.handle {
        Ok(object_meta) => match purpose {
            ObjectPurpose::Unwind if object_meta.features.has_unwind_info => 0,
            ObjectPurpose::Debug if object_meta.features.has_debug_info => 0,
            ObjectPurpose::Debug if object_meta.features.has_symbols => 1,
            ObjectPurpose::Source if object_meta.features.has_sources => 0,
            _ => 2,
        },
        Err(_) => 3,
    }
}

/// Whether the object provides the required features for the given purpose.
fn object_has_features(meta_handle: &ObjectMetaHandle, purpose: ObjectPurpose) -> bool {
    match purpose {
        ObjectPurpose::Unwind => meta_handle.features.has_unwind_info,
        ObjectPurpose::Debug => {
            meta_handle.features.has_debug_info || meta_handle.features.has_symbols
        }
        ObjectPurpose::Source => meta_handle.features.has_sources,
    }
}

/// Creates collection of all the DIF object candidates used in the metadata lookups.
///
/// If there were any sources which did not return any [`download::list_files`]
/// results they will get a [`ObjectDownloadInfo::NotFound`] entry with a location of `*`.
/// In practice this will only affect the `sentry` source for now as all other sources
/// always return [`download::list_files`] results.
fn create_candidates(sources: &[SourceConfig], lookups: &[FoundMeta]) -> AllObjectCandidates {
    let mut source_ids: BTreeSet<SourceId> =
        sources.iter().map(|source| source.id()).cloned().collect();
    let mut candidates: Vec<ObjectCandidate> = Vec::with_capacity(lookups.len() + source_ids.len());

    for meta_lookup in lookups.iter() {
        let source_id = meta_lookup.file_source.source_id();
        source_ids.take(source_id);
        candidates.push(create_candidate_info(meta_lookup));
    }

    // Create a NotFound entry for each source from which we did not try and fetch anything.
    for source_id in source_ids {
        let info = ObjectCandidate {
            source: source_id,
            location: RemoteFileUri::new("No object files listed on this source"),
            download: ObjectDownloadInfo::NotFound,
            unwind: Default::default(),
            debug: Default::default(),
        };
        candidates.push(info);
    }

    // NOTE: This `into()` (or rather the `From` impl) does `sort` and `dedupe` these candidates.
    candidates.into()
}

/// Build the [`ObjectCandidate`] info for the provided meta lookup result.
fn create_candidate_info(meta_lookup: &FoundMeta) -> ObjectCandidate {
    let source = meta_lookup.file_source.source_id().clone();
    let location = meta_lookup.file_source.uri();
    let download = match &meta_lookup.handle {
        Ok(handle) => ObjectDownloadInfo::Ok {
            features: handle.features(),
        },
        Err(error) => match error {
            CacheError::NotFound => ObjectDownloadInfo::NotFound,
            CacheError::PermissionDenied(msg) => ObjectDownloadInfo::NoPerm {
                details: msg.clone(),
            },
            CacheError::Malformed(_) => ObjectDownloadInfo::Malformed,
            err => ObjectDownloadInfo::Error {
                details: err.to_string(),
            },
        },
    };

    ObjectCandidate {
        source,
        location,
        download,
        unwind: Default::default(),
        debug: Default::default(),
    }
}
