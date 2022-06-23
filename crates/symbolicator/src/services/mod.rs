//! Provides the symbolicator [`Service`] and internal services.
//!
//! Symbolicator operates a number of independent services defined in this module for downloading,
//! cache management, and symbolication. They are created by the main [`Service`] and can be
//! accessed via that.
//!
//! In general, services are created once in the [`crate::services::Service`] and accessed via this
//! state.
//!
//! The internal services require two separate asynchronous runtimes.
//! (There is a third runtime dedicated to serving http requests)
//! For regular scheduling and I/O-intensive work, services will use the `io_pool`.
//! For CPU intensive workloads, services will use the `cpu_pool`.
//!
//! When a request comes in on the web pool, it is handed off to the `cpu_pool` for processing, which
//! is primarily synchronous work in the best case (everything is cached).
//! When file fetching is needed, that fetching will happen on the `io_pool`.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::cache::Caches;
use crate::config::Config;
use crate::metrics::record_task_metrics;

pub mod bitcode;
pub mod cacher;
pub mod cficaches;
pub mod download;
pub mod il2cpp;
mod minidump;
pub mod objects;
pub mod shared_cache;
pub mod symbolication;
pub mod symcaches;

use self::bitcode::BitcodeService;
use self::cficaches::CfiCacheActor;
use self::download::DownloadService;
use self::il2cpp::Il2cppService;
use self::objects::ObjectsActor;
use self::shared_cache::SharedCacheService;
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
}

impl Service {
    pub async fn create(
        config: Config,
        io_pool: tokio::runtime::Handle,
        cpu_pool: tokio::runtime::Handle,
    ) -> Result<Self> {
        let config = Arc::new(config);

        let downloader = DownloadService::new(&config);
        let shared_cache =
            SharedCacheService::new(config.shared_cache.clone(), io_pool.clone()).await;
        let shared_cache = Arc::new(shared_cache);
        let caches = Caches::from_config(&config).context("failed to create local caches")?;
        caches
            .clear_tmp(&config)
            .context("failed to clear tmp caches")?;
        let objects = ObjectsActor::new(
            caches.object_meta,
            caches.objects,
            shared_cache.clone(),
            downloader.clone(),
            io_pool.clone(),
        );
        let bitcode = BitcodeService::new(
            caches.auxdifs,
            shared_cache.clone(),
            downloader.clone(),
            io_pool.clone(),
        );
        let il2cpp = Il2cppService::new(
            caches.il2cpp,
            shared_cache.clone(),
            downloader,
            io_pool.clone(),
        );
        let symcaches = SymCacheActor::new(
            caches.symcaches,
            shared_cache.clone(),
            objects.clone(),
            bitcode,
            il2cpp,
            cpu_pool.clone(),
        );
        let cficaches = CfiCacheActor::new(
            caches.cficaches,
            shared_cache,
            objects.clone(),
            cpu_pool.clone(),
        );

        let symbolication = SymbolicationActor::new(
            objects.clone(),
            symcaches,
            cficaches,
            caches.diagnostics,
            cpu_pool,
            config.max_concurrent_requests,
        );
        let symbolication_taskmon = symbolication.symbolication_task_monitor();
        io_pool.spawn(async move {
            for interval in symbolication_taskmon.intervals() {
                record_task_metrics("symbolication", &interval);
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });

        Ok(Self {
            symbolication,
            objects,
            config,
        })
    }

    pub fn symbolication(&self) -> SymbolicationActor {
        self.symbolication.clone()
    }

    pub fn objects(&self) -> &ObjectsActor {
        &self.objects
    }

    pub fn config(&self) -> Arc<Config> {
        self.config.clone()
    }
}
