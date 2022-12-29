use std::future::Future;
use std::path::Path;
use std::sync::Arc;

use symbolic::common::ByteView;
use tempfile::NamedTempFile;

use crate::cache::{Cache, CacheEntry, CacheError, ExpirationTime};

use super::cacher::{persist_tempfile, CacheKey, CacheVersions};
use super::shared_cache::{CacheStoreReason, SharedCacheKey, SharedCacheService};

pub struct Cacher2 {
    config: Cache,
    shared_cache_service: Arc<SharedCacheService>,
    versions: CacheVersions,
}

impl Cacher2 {
    pub async fn load_or_compute<T, LoadFn, ComputeFn, ComputeFut>(
        &self,
        key: &CacheKey,
        load_fn: LoadFn,
        compute_fn: ComputeFn,
    ) -> CacheEntry<T>
    where
        LoadFn: FnMut(ByteView<'static>, ExpirationTime) -> CacheEntry<T>,
        ComputeFn: FnOnce(NamedTempFile) -> ComputeFut,
        ComputeFut: Future<Output = CacheEntry<NamedTempFile>> + Send + 'static,
    {
        let mut request = CacheRequest {
            config: &self.config,
            shared_cache_service: self.shared_cache_service.as_ref(),
            versions: self.versions.clone(),

            key,
            load_fn,
            compute_fn: Some(compute_fn),
        };
        request.load_or_compute().await
    }
}

struct CacheRequest<'a, LoadFn, ComputeFn> {
    config: &'a Cache,
    key: &'a CacheKey,
    versions: CacheVersions,

    shared_cache_service: &'a SharedCacheService,
    load_fn: LoadFn,
    compute_fn: Option<ComputeFn>,
}

impl<'a, T, LoadFn, ComputeFn, ComputeFut> CacheRequest<'a, LoadFn, ComputeFn>
where
    LoadFn: FnMut(ByteView<'static>, ExpirationTime) -> CacheEntry<T>,
    ComputeFn: FnOnce(NamedTempFile) -> ComputeFut,
    ComputeFut: Future<Output = CacheEntry<NamedTempFile>> + Send + 'static,
{
    async fn load_or_compute(&mut self) -> CacheEntry<T> {
        let result = self.load_any_version_from_disk();

        let mut result = match result {
            Ok((entry, version)) if version != self.versions.current => {
                // this was a fallback -> spawn background computation
                Ok(entry)
            }
            Ok((entry, _version)) => Ok(entry), // this was the current version
            Err(err) => Err(err),
        };

        if result.is_err() {
            result = self.compute().await;
        }

        result
    }

    fn load_any_version_from_disk(&mut self) -> CacheEntry<(T, u32)> {
        // TODO: this should be done outside this function?
        let Some(cache_dir) = self.config.cache_dir() else {
            return Err(CacheError::NotFound);
        };
        let mut result = Err(CacheError::NotFound);

        let versions =
            std::iter::once(self.versions.current).chain(self.versions.fallbacks.iter().copied());
        for version in versions {
            let path = self.key.cache_path(cache_dir, version);
            result = self
                .load_one_version_from_disk(&path)
                .map(|entry| (entry, version));
            if result.is_ok() {
                break;
            }
        }

        result
    }

    fn load_one_version_from_disk(&mut self, path: &Path) -> CacheEntry<T> {
        let (entry, expiration) = self
            .config
            .open_cachefile(path)?
            .ok_or(CacheError::NotFound)?;

        entry.and_then(|byteview| (self.load_fn)(byteview, expiration))
    }

    async fn compute(&mut self) -> CacheEntry<T> {
        let temp_file = self.config.tempfile()?;
        let shared_cache_key = SharedCacheKey {
            name: self.config.name(),
            version: self.versions.current,
            local_key: self.key.clone(),
        };

        let temp_fd = tokio::fs::File::from_std(temp_file.reopen()?);
        let shared_cache_hit = self
            .shared_cache_service
            .fetch(&shared_cache_key, temp_fd)
            .await;

        let mut entry = Err(CacheError::NotFound);

        if shared_cache_hit {
            let byteview = ByteView::map_file_ref(temp_file.as_file())?;
            entry = self.load_fresh_entry(byteview);
            // TODO: log error / metric
        }

        let temp_file = match entry {
            // the cache came from the shared cache which wrote directly to `temp_file`
            Ok(_) => temp_file,
            Err(_) => {
                #[allow(clippy::unnecessary_lazy_evaluations)]
                let compute_fn = self.compute_fn.take().ok_or_else(|| {
                    // TODO: log error
                    CacheError::InternalError
                })?;

                match compute_fn(temp_file).await {
                    Ok(temp_file) => {
                        // Now we have written the data to the tempfile we can mmap it, persisting it later
                        // is fine as it does not move filesystem boundaries there.
                        let byteview = ByteView::map_file_ref(temp_file.as_file())?;
                        entry = self.load_fresh_entry(byteview);

                        temp_file
                    }
                    Err(err) => {
                        // FIXME(swatinem): We are creating a new tempfile in the hot err/not-found path
                        let temp_file = self.config.tempfile()?;
                        let mut temp_fd = tokio::fs::File::from_std(temp_file.reopen()?);
                        err.write(&mut temp_fd).await?;

                        entry = Err(err);

                        temp_file
                    }
                }
            }
        };

        let file = if let Some(cache_dir) = self.config.cache_dir() {
            // Cache is enabled, write it!
            let cache_path = self.key.cache_path(cache_dir, self.versions.current);

            // TODO: metrics and logging

            persist_tempfile(temp_file, cache_path).await?
        } else {
            temp_file.into_file()
        };

        if !shared_cache_hit && entry.is_ok() {
            self.shared_cache_service
                .store(
                    shared_cache_key,
                    tokio::fs::File::from_std(file),
                    CacheStoreReason::New,
                )
                .await;
        }

        entry
    }

    fn load_fresh_entry(&mut self, byteview: ByteView<'static>) -> CacheEntry<T> {
        let entry = Ok(byteview);
        let expiration = ExpirationTime::for_fresh_status(self.config, &entry);

        entry.and_then(|byteview| (self.load_fn)(byteview, expiration))
    }
}
