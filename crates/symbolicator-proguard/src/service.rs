use std::io::BufWriter;
use std::sync::Arc;

use futures::future::BoxFuture;
use proguard::ProguardCache;
use symbolic::common::{AsSelf, ByteView, DebugId, SelfCell};
use symbolicator_service::caches::versions::PROGUARD_CACHE_VERSIONS;
use symbolicator_service::caching::{
    CacheEntry, CacheError, CacheItemRequest, CacheKey, CacheVersions, Cacher,
};
use symbolicator_service::download::{fetch_file, tempfile_in_parent, DownloadService};
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

    /// Downloads a proguard file for the given scope and debug id and converts it into a
    /// `ProguardMapper`.
    #[tracing::instrument(skip_all)]
    pub async fn download_proguard_file(
        &self,
        sources: &[SourceConfig],
        scope: &Scope,
        debug_id: DebugId,
    ) -> CacheEntry<OwnedProguardCache> {
        let identifier = ObjectId {
            debug_id: Some(debug_id),
            ..Default::default()
        };

        let file = self
            .download_svc
            .list_files(sources, &[FileType::Proguard], &identifier)
            .await
            .into_iter()
            .next()
            .ok_or(CacheError::NotFound)?;

        let cache_key = CacheKey::from_scoped_file(scope, &file);

        let request = FetchProguard {
            file,
            download_svc: Arc::clone(&self.download_svc),
        };

        self.cache.compute_memoized(request, cache_key).await
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
}

struct ProguardInner<'a> {
    cache: proguard::ProguardCache<'a>,
}

impl<'slf, 'a: 'slf> AsSelf<'slf> for ProguardInner<'a> {
    type Ref = ProguardInner<'slf>;

    fn as_self(&'slf self) -> &'slf Self::Ref {
        self
    }
}

#[derive(Clone)]
pub struct OwnedProguardCache {
    inner: Arc<SelfCell<ByteView<'static>, ProguardInner<'static>>>,
}

impl OwnedProguardCache {
    #[tracing::instrument(name = "OwnedProguardCache::new", skip_all, fields(size = byteview.len()))]
    pub fn new(byteview: ByteView<'static>) -> Result<Self, CacheError> {
        let inner = SelfCell::try_new::<CacheError, _>(byteview, |data| {
            let cache = ProguardCache::parse(unsafe { &*data }).map_err(|e| {
                tracing::error!(error = %e);
                CacheError::InternalError
            })?;
            Ok(ProguardInner { cache })
        })?;

        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    pub fn get(&self) -> &proguard::ProguardCache {
        &self.inner.get().cache
    }
}

#[derive(Clone, Debug)]
pub struct FetchProguard {
    file: RemoteFile,
    download_svc: Arc<DownloadService>,
}

impl CacheItemRequest for FetchProguard {
    type Item = OwnedProguardCache;

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
                let cache_temp_file = tempfile_in_parent(temp_file)?;
                let mut writer = BufWriter::new(cache_temp_file);
                ProguardCache::write(&mapping, &mut writer)?;
                let mut cache_temp_file =
                    writer.into_inner().map_err(|_| CacheError::InternalError)?;
                std::mem::swap(temp_file, &mut cache_temp_file);
                Ok(())
            }
        };
        Box::pin(fut)
    }

    fn load(&self, byteview: ByteView<'static>) -> CacheEntry<Self::Item> {
        OwnedProguardCache::new(byteview)
    }

    fn use_shared_cache(&self) -> bool {
        true
    }
}
