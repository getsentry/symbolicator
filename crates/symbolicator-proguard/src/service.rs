use std::sync::Arc;

use futures::future::BoxFuture;
use symbolic::common::{AsSelf, ByteView, SelfCell};
use symbolicator_service::caches::versions::PROGUARD_CACHE_VERSIONS;
use symbolicator_service::caching::{
    CacheEntry, CacheError, CacheItemRequest, CacheKey, CacheVersions, Cacher,
};
use symbolicator_service::download::{fetch_file, DownloadService};
use symbolicator_service::services::SharedServices;
use symbolicator_service::types::Scope;
use symbolicator_sources::{FileType, ObjectId, RemoteFile, SourceConfig};
use tempfile::NamedTempFile;

#[derive(Debug, Clone)]
pub struct ProguardService {
    pub(crate) download_svc: Arc<DownloadService>,
    pub(crate) cache: Arc<Cacher<FetchProguard>>,
}

impl ProguardService {
    pub fn new(services: &SharedServices) -> Self {
        let caches = &services.caches;
        let shared_cache = services.shared_cache.clone();
        let download_svc = services.download_svc.clone();

        let cache = Arc::new(Cacher::new(caches.proguard.clone(), shared_cache));

        Self {
            download_svc,
            cache,
        }
    }

    async fn find_proguard_file(
        &self,
        sources: &[SourceConfig],
        identifier: &ObjectId,
    ) -> Option<RemoteFile> {
        let file_ids = self
            .download_svc
            .list_files(&sources, &[FileType::Proguard], identifier)
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
}

struct ProguardInner<'a> {
    mapping: proguard::ProguardMapping<'a>,
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
            if mapping.is_valid() {
                Ok(())
            } else {
                Err(CacheError::Malformed(
                    "The file is not a valid ProGuard file".into(),
                ))
            }
        };
        Box::pin(fut)
    }

    fn load(&self, byteview: ByteView<'static>) -> CacheEntry<Self::Item> {
        let inner = SelfCell::new(byteview, |data| {
            let mapping = proguard::ProguardMapping::new(unsafe { &*data });
            let mapper = proguard::ProguardMapper::new(mapping.clone());
            ProguardInner { mapping, mapper }
        });

        Ok(ProguardMapper {
            inner: Arc::new(inner),
        })
    }

    fn use_shared_cache(&self) -> bool {
        false
    }
}
