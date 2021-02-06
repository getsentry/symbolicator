//! Provides the symbolicator [`Service`] and internal services.
//!
//! Symbolicator operates a number of independent services defined in this module for downloading,
//! cache management, and symbolication. They are created by the main [`Service`] and can be
//! accessed via that.
//!
//! In general, services are created once in the [`crate::services::Service`] and accessed via this
//! state.
//!
//! The internal services require an asynchronous runtime. For regular scheduling and I/O-intensive
//! work, services use the common runtime. For CPU intensive workloads, services delegate to a
//! [`ThreadPool`]. It is common for threadpools to be shared by multiple services and the
//! application wants to generally separate services with CPU-intensive workloads from those with
//! IO-heavy workloads.
//!
//! # Tokio 0.1 vs Tokio 1
//!
//! While symbolicator is transitioning, two runtimes are required. The [`Service`] to run in a
//! `tokio 0.1` runtime, but from within a `tokio 1` context. To achieve this, either run from
//! within a `tokio::main` or `tokio::test` context, or use `Runtime::enter` to register the Tokio
//! runtime.
//!
//! The current division of runtimes is:
//!
//!  - The HTTP server uses `tokio 0.1`.
//!  - Services use the HTTP server's runtime.
//!  - The downloader uses the `tokio 1` runtime internally.

use std::sync::Arc;

use anyhow::{Context, Result};

use crate::cache::Caches;
use crate::config::Config;
use crate::utils::futures::ThreadPool;

pub mod cacher;
pub mod cficaches;
pub mod download;
pub mod objects;
pub mod symbolication;
pub mod symcaches;

use self::cficaches::CfiCacheActor;
use self::download::DownloadService;
use self::objects::ObjectsActor;
use self::symbolication::SymbolicationActor;
use self::symcaches::SymCacheActor;

/// The shared state for the service.
#[derive(Clone, Debug)]
pub struct Service {
    /// Actor for minidump and stacktrace processing
    symbolication: SymbolicationActor,
    /// Actor for downloading and caching objects (no symcaches or cficaches)
    objects: ObjectsActor,
    /// The config object.
    config: Arc<Config>,
    /// The download service.
    downloader: Arc<DownloadService>,
}

impl Service {
    pub fn create(config: Config) -> Result<Self> {
        let config = Arc::new(config);

        let cpu_pool = ThreadPool::new();
        let spawnpool = procspawn::Pool::new(config.processing_pool_size)
            .context("failed to create process pool")?;

        let downloader = DownloadService::new(config.clone());
        let caches = Caches::from_config(&config).context("failed to create local caches")?;
        caches
            .clear_tmp(&config)
            .context("failed to clear tmp caches")?;
        let objects = ObjectsActor::new(caches.object_meta, caches.objects, downloader.clone());
        let symcaches = SymCacheActor::new(caches.symcaches, objects.clone(), cpu_pool.clone());
        let cficaches = CfiCacheActor::new(caches.cficaches, objects.clone(), cpu_pool.clone());

        let symbolication = SymbolicationActor::new(
            objects.clone(),
            symcaches,
            cficaches,
            caches.diagnostics,
            cpu_pool,
            spawnpool,
        );

        Ok(Self {
            symbolication,
            objects,
            config,
            downloader,
        })
    }

    pub fn symbolication(&self) -> SymbolicationActor {
        self.symbolication.clone()
    }

    pub fn objects(&self) -> ObjectsActor {
        self.objects.clone()
    }

    pub fn config(&self) -> Arc<Config> {
        self.config.clone()
    }
}
