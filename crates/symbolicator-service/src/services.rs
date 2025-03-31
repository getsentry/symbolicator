//! Provides the internal shared Symbolicator services and a way to initialize them.
//!
//! Symbolicator operates a number of independent services defined in this module for downloading,
//! cache management, and file access.
//! [`SharedServices`] initializes all these internal services according to the provided [`Config`].
//!
//! The internal services require a separate asynchronous runtimes dedicated for I/O-intensive work,
//! such as downloads and access to the shared cache.

use std::sync::Arc;

use anyhow::{Context, Result};

use crate::caching::{Caches, SharedCacheRef, SharedCacheService};
use crate::config::Config;

use crate::caches::SourceFilesCache;
use crate::download::{DownloadService, SymstoreIndexService};
use crate::objects::ObjectsActor;

pub struct SharedServices {
    pub config: Config,
    pub caches: Caches,
    pub download_svc: Arc<DownloadService>,
    pub symstore_index_svc: Arc<SymstoreIndexService>,
    pub shared_cache: SharedCacheRef,
    pub objects: ObjectsActor,
    pub sourcefiles_cache: Arc<SourceFilesCache>,
}

impl SharedServices {
    pub fn new(config: Config, io_pool: tokio::runtime::Handle) -> Result<Self> {
        let caches = Caches::from_config(&config).context("failed to create local caches")?;
        caches
            .clear_tmp(&config)
            .context("failed to clear tmp caches")?;

        let shared_cache = SharedCacheService::new(config.shared_cache.clone(), io_pool.clone());
        let download_svc = DownloadService::new(&config, io_pool.clone());
        let symstore_index_svc = Arc::new(SymstoreIndexService::new(
            caches.symstore_index.clone(),
            shared_cache.clone(),
            download_svc.clone(),
        ));

        let sourcefiles_cache = Arc::new(SourceFilesCache::new(
            caches.sourcefiles.clone(),
            shared_cache.clone(),
            download_svc.clone(),
        ));

        let objects = ObjectsActor::new(
            caches.object_meta.clone(),
            caches.objects.clone(),
            shared_cache.clone(),
            download_svc.clone(),
            symstore_index_svc.clone(),
        );

        Ok(Self {
            config,
            caches,
            download_svc,
            symstore_index_svc,
            shared_cache,
            objects,
            sourcefiles_cache,
        })
    }
}
