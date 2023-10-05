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

pub mod bitcode;
pub mod caches;
pub mod cficaches;
pub mod derived;
pub mod download;
mod fetch_file;
pub mod il2cpp;
mod minidump;
mod module_lookup;
pub mod objects;
pub mod ppdb_caches;
pub mod symbolication;
pub mod symcaches;

use self::caches::SourceFilesCache;
use self::download::DownloadService;
use self::objects::ObjectsActor;

pub use self::symbolication::ScrapingConfig;
pub use fetch_file::fetch_file;

pub struct SharedServices {
    pub config: Config,
    pub caches: Caches,
    pub download_svc: Arc<DownloadService>,
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

        let download_svc = DownloadService::new(&config, io_pool.clone());

        let shared_cache = SharedCacheService::new(config.shared_cache.clone(), io_pool);

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
        );

        Ok(Self {
            config,
            caches,
            download_svc,
            shared_cache,
            objects,
            sourcefiles_cache,
        })
    }
}
