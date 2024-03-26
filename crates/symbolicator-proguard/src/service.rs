use std::sync::Arc;

use futures::future::BoxFuture;
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

    async fn find_proguard_file(
        &self,
        sources: &[SourceConfig],
        identifier: &ObjectId,
    ) -> Option<RemoteFile> {
        let file_ids = self
            .download_svc
            .list_files(sources, &[FileType::Proguard], identifier)
            .await;

        file_ids.into_iter().next()
    }

    /// Retrieves the given [`RemoteFile`] from cache, or fetches it and persists it according
    /// to the provided [`Scope`].
    /// It is possible to avoid using the shared cache using the `use_shared_cache` parameter.
    pub async fn fetch_file(&self, scope: &Scope, file: RemoteFile) -> CacheEntry<ProguardMapper> {
        let cache_key = CacheKey::from_scoped_file(scope, &file);

        let request = FetchProguard {
            file,
            download_svc: Arc::clone(&self.download_svc),
        };

        self.cache.compute_memoized(request, cache_key).await
    }

    pub async fn download_proguard_file(
        &self,
        sources: &[SourceConfig],
        scope: &Scope,
        debug_id: DebugId,
    ) -> CacheEntry<ProguardMapper> {
        let identifier = ObjectId {
            debug_id: Some(debug_id),
            ..Default::default()
        };

        let remote_file = self
            .find_proguard_file(sources, &identifier)
            .await
            .ok_or(CacheError::NotFound)?;

        self.fetch_file(scope, remote_file).await
    }

    /// Downloads a source bundle for the given scope and debug id.
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
}

struct ProguardInner<'a> {
    mapper: proguard::ProguardMapper<'a>,
}

impl<'slf, 'a: 'slf> AsSelf<'slf> for ProguardInner<'a> {
    type Ref = ProguardInner<'slf>;

    fn as_self(&'slf self) -> &Self::Ref {
        self
    }
}

#[derive(Clone)]
pub struct ProguardMapper {
    inner: Arc<SelfCell<ByteView<'static>, ProguardInner<'static>>>,
}

impl ProguardMapper {
    pub fn new(byteview: ByteView<'static>, use_param_mapping: bool) -> Self {
        let inner = SelfCell::new(byteview, |data| {
            let mapping = proguard::ProguardMapping::new(unsafe { &*data });
            let mapper =
                proguard::ProguardMapper::new_with_param_mapping(mapping, use_param_mapping);
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
    download_svc: Arc<DownloadService>,
}

impl CacheItemRequest for FetchProguard {
    type Item = ProguardMapper;

    const VERSIONS: CacheVersions = PROGUARD_CACHE_VERSIONS;

    fn compute<'a>(&'a self, temp_file: &'a mut NamedTempFile) -> BoxFuture<'a, CacheEntry> {
        let fut = async {
            fetch_file(self.download_svc.clone(), self.file.clone(), temp_file).await?;

            let view = ByteView::map_file_ref(temp_file.as_file())?;

            let mapping = proguard::ProguardMapping::new(&view);
            if !mapping.is_valid() {
                Err(CacheError::Malformed(
                    "The file is not a valid ProGuard file".into(),
                ))
            } else if !mapping.has_line_info() {
                Err(CacheError::Malformed(
                    "The ProGuard file doesn't contain any line mappings".into(),
                ))
            } else {
                Ok(())
            }
        };
        Box::pin(fut)
    }

    fn load(&self, byteview: ByteView<'static>) -> CacheEntry<Self::Item> {
        Ok(Self::Item::new(byteview, true))
    }

    fn use_shared_cache(&self) -> bool {
        false
    }
}
