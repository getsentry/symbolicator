use std::fmt::Write;
use std::ops::Range;
use std::sync::Arc;

use futures::future::{self, BoxFuture};
use symbolic::common::{AsSelf, ByteView, DebugId, SelfCell};
use symbolicator_service::caches::versions::PROGUARD_CACHE_VERSIONS;
use symbolicator_service::caching::{
    CacheEntry, CacheError, CacheItemRequest, CacheKey, CacheVersions, Cacher,
};
use symbolicator_service::download::{fetch_file, DownloadService};
use symbolicator_service::objects::{
    FindObject, FindResult, ObjectHandle, ObjectPurpose, ObjectsActor,
};
use symbolicator_service::services::SharedServices;
use symbolicator_service::types::Scope;
use symbolicator_sources::{FileType, ObjectId, RemoteFile, SourceConfig};
use tempfile::NamedTempFile;

use crate::interface::{ProguardError, ProguardErrorKind};

#[derive(Debug, Clone)]
pub struct ProguardService {
    pub(crate) download_svc: Arc<DownloadService>,
    pub(crate) cache: Arc<Cacher<FetchProguard>>,
    pub(crate) objects: ObjectsActor,
}

impl ProguardService {
    pub fn new(services: &SharedServices) -> Self {
        let caches = &services.caches;
        let shared_cache = services.shared_cache.clone();
        let download_svc = services.download_svc.clone();
        let objects = services.objects.clone();

        let cache = Arc::new(Cacher::new(caches.proguard.clone(), shared_cache));

        Self {
            download_svc,
            cache,
            objects,
        }
    }

    /// Downloads a source bundle for the given scope and debug id.
    #[tracing::instrument(skip_all)]
    pub async fn download_source_bundle(
        &self,
        sources: Arc<[SourceConfig]>,
        scope: &Scope,
        debug_id: DebugId,
    ) -> CacheEntry<Arc<ObjectHandle>> {
        let identifier = ObjectId {
            debug_id: Some(debug_id),
            ..Default::default()
        };
        let find_request = FindObject {
            filetypes: &[FileType::SourceBundle],
            purpose: ObjectPurpose::Source,
            identifier,
            sources: sources.clone(),
            scope: scope.clone(),
        };

        let FindResult { meta, .. } = self.objects.find(find_request).await;
        match meta {
            Some(meta) => match meta.handle {
                Ok(handle) => self.objects.fetch(handle).await,
                Err(err) => Err(err),
            },
            None => Err(CacheError::NotFound),
        }
    }

    /// Downloads the proguard file with the given debug ID and extracts a mapper for
    /// the given class name from it.
    ///
    /// This returns:
    /// * `Ok(Some(mapper))` if the file exists, is valid, and contains a
    ///   mapping information for `class_name`;
    /// * `Ok(None)` if the file exists and is valid, but doesn't contain
    ///   mapping informationfor `class_name`;
    /// * `Err(_)` if the file wasn't found or is invalid.
    #[tracing::instrument(skip_all)]
    pub async fn download_proguard_file(
        &self,
        sources: Arc<[SourceConfig]>,
        scope: &Scope,
        debug_id: DebugId,
        class_name: Arc<str>,
    ) -> Result<Option<ProguardMapper>, ProguardError> {
        let identifier = ObjectId {
            debug_id: Some(debug_id),
            ..Default::default()
        };

        let file = self
            .download_svc
            .list_files(sources.as_ref(), &[FileType::Proguard], &identifier)
            .await
            .into_iter()
            .next()
            .ok_or(ProguardError {
                uuid: debug_id,
                kind: ProguardErrorKind::Missing,
            })?;

        let file_cache_key = CacheKey::from_scoped_file(scope, &file);

        let memory_cache_key = {
            let mut builder = CacheKey::scoped_builder(scope);
            builder.write_file_meta(&file).unwrap();
            writeln!(&mut builder, "class:").unwrap();
            writeln!(&mut builder, "{class_name}").unwrap();
            builder.build()
        };

        let request = FetchProguard {
            file,
            class_name,
            download_svc: Arc::clone(&self.download_svc),
        };

        match self
            .cache
            .compute_memoized(request, memory_cache_key, file_cache_key)
            .await
        {
            Ok(maybe_mapper) => Ok(maybe_mapper.map(|p| p.1)),
            Err(e) => {
                if !matches!(e, CacheError::NotFound) {
                    tracing::error!(%debug_id, "Error reading Proguard file: {e}");
                }
                let kind = match e {
                    CacheError::Malformed(msg) => match msg.as_str() {
                        "The file is not a valid ProGuard file" => ProguardErrorKind::Invalid,
                        "The ProGuard file doesn't contain any line mappings" => {
                            ProguardErrorKind::NoLineInfo
                        }
                        _ => unreachable!(),
                    },
                    _ => ProguardErrorKind::Missing,
                };
                Err(ProguardError {
                    uuid: debug_id,
                    kind,
                })
            }
        }
    }

    /// Downloads the proguard files with the given debug IDs and extracts mappers for
    /// the given class names from them.
    pub async fn download_proguard_files(
        &self,
        sources: Arc<[SourceConfig]>,
        scope: &Scope,
        debug_ids: Arc<[DebugId]>,
        class_names: &[Arc<str>],
    ) -> (Vec<ProguardMapper>, Vec<ProguardError>) {
        let outer = future::join_all(debug_ids.iter().cloned().map(|debug_id| {
            let sources = Arc::clone(&sources);
            async move {
                let inner = future::join_all(class_names.iter().cloned().map(|class| {
                    let sources = Arc::clone(&sources);
                    async move {
                        self.download_proguard_file(Arc::clone(&sources), scope, debug_id, class)
                            .await
                    }
                }));

                inner.await
            }
        }));

        let (mut mappers, mut errors) = (Vec::new(), Vec::new());
        let outer_results = outer.await;
        for res in outer_results
            .into_iter()
            .flat_map(|inner_result| inner_result.into_iter())
        {
            match res {
                Ok(Some(mapper)) => mappers.push(mapper),
                Ok(None) => {}
                Err(e) => errors.push(e),
            }
        }

        errors.sort_unstable_by_key(|e| e.uuid);
        errors.dedup_by_key(|e| e.uuid);

        (mappers, errors)
    }
}

#[derive(Debug, Clone)]
struct ProguardInner<'a> {
    mapper: proguard::ProguardMapper<'a>,
}

impl<'slf, 'a: 'slf> AsSelf<'slf> for ProguardInner<'a> {
    type Ref = ProguardInner<'slf>;

    fn as_self(&'slf self) -> &Self::Ref {
        self
    }
}

#[derive(Clone, Debug)]
pub struct ProguardMapper {
    inner: Arc<SelfCell<ByteView<'static>, ProguardInner<'static>>>,
}

impl ProguardMapper {
    #[tracing::instrument(name = "ProguardMapper::new", skip_all, fields(size = byteview.len()))]
    pub fn new(byteview: ByteView<'static>, range: Range<usize>) -> Self {
        let inner = SelfCell::new(byteview, |data| {
            let mapping = proguard::ProguardMapping::new(unsafe { &*data });
            let mapping = mapping.section(range.start, range.end);
            let mapper = proguard::ProguardMapper::new_with_param_mapping(mapping, true);
            ProguardInner { mapper }
        });

        Self {
            inner: Arc::new(inner),
        }
    }

    pub fn get(&self) -> &proguard::ProguardMapper {
        &self.inner.get().mapper
    }
}

#[derive(Clone, Debug)]
pub struct FetchProguard {
    file: RemoteFile,
    class_name: Arc<str>,
    download_svc: Arc<DownloadService>,
}

impl CacheItemRequest for FetchProguard {
    /// The first component is the estimated memory footprint of the mapper,
    /// computed as 2x the size of the relevant section of the mapping ifle in bytes.
    type Item = Option<(u32, ProguardMapper)>;

    const VERSIONS: CacheVersions = PROGUARD_CACHE_VERSIONS;

    fn compute<'a>(&'a self, temp_file: &'a mut NamedTempFile) -> BoxFuture<'a, CacheEntry> {
        let fut = async {
            fetch_file(self.download_svc.clone(), self.file.clone(), temp_file).await?;

            let view = ByteView::map_file_ref(temp_file.as_file())?;

            let mapping = proguard::ProguardMapping::new(&view);
            if !mapping.is_valid() {
                return Err(CacheError::Malformed(
                    "The file is not a valid ProGuard file".into(),
                ));
            }

            if !mapping.has_line_info() {
                return Err(CacheError::Malformed(
                    "The ProGuard file doesn't contain any line mappings".into(),
                ));
            }

            Ok(())
        };
        Box::pin(fut)
    }

    fn load(&self, byteview: ByteView<'static>) -> CacheEntry<Self::Item> {
        let mapping = proguard::ProguardMapping::new(&byteview);
        // TODO(sebastian): Creating the index every time we want to load a class from a file is obviously
        // wasteful, we should cache this.
        let index = mapping.create_class_index();
        let Some(range) = index.get(self.class_name.as_ref()).cloned() else {
            return Ok(None);
        };

        // NOTE: In an extremely unscientific test, the proguard mapper was slightly less
        // than twice as big in memory as the file on disk.
        let weight = ((range.end - range.start) as u32).saturating_mul(2);

        Ok(Some((weight, ProguardMapper::new(byteview, range))))
    }

    fn use_shared_cache(&self) -> bool {
        false
    }

    fn weight(item: &Self::Item) -> u32 {
        item.as_ref()
            .map_or(0, |it| it.0)
            .max(std::mem::size_of::<Self::Item>() as u32)
    }
}
