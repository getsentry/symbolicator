//! Exposes the command line application.
use std::sync::Arc;

use actix_web::App;
use anyhow::{Context, Result};

use crate::actors::{
    cficaches::CfiCacheActor, objects::ObjectsActor, symbolication::SymbolicationActor,
    symcaches::SymCacheActor,
};
use crate::cache::Caches;
use crate::config::Config;
use crate::services::download::DownloadService;
use crate::utils::futures::ThreadPool;

/// The shared state for the service.
#[derive(Clone, Debug)]
pub struct ServiceState {
    /// Thread pool instance reserved for CPU-intensive tasks.
    cpu_threadpool: ThreadPool,
    /// Thread pool instance reserved for IO-intensive tasks.
    io_threadpool: ThreadPool,
    /// Actor for minidump and stacktrace processing
    symbolication: SymbolicationActor,
    /// Actor for downloading and caching objects (no symcaches or cficaches)
    objects: ObjectsActor,
    /// The config object.
    config: Arc<Config>,
    /// The download service.
    download_svc: Arc<DownloadService>,
}

impl ServiceState {
    pub fn create(config: Config) -> Result<Self> {
        let config = Arc::new(config);

        let cpu_threadpool = ThreadPool::new();
        let io_threadpool = ThreadPool::new();

        let download_svc = DownloadService::new(config.clone());

        let caches = Caches::from_config(&config).context("failed to create local caches")?;
        caches
            .clear_tmp(&config)
            .context("failed to clear tmp caches")?;
        let objects = ObjectsActor::new(
            caches.object_meta,
            caches.objects,
            io_threadpool.clone(),
            download_svc.clone(),
        );
        let symcaches =
            SymCacheActor::new(caches.symcaches, objects.clone(), cpu_threadpool.clone());
        let cficaches =
            CfiCacheActor::new(caches.cficaches, objects.clone(), cpu_threadpool.clone());
        let spawnpool = procspawn::Pool::new(config.processing_pool_size)
            .context("failed to create process pool")?;

        let symbolication = SymbolicationActor::new(
            objects.clone(),
            symcaches,
            cficaches,
            caches.diagnostics,
            cpu_threadpool.clone(),
            spawnpool,
        );

        Ok(Self {
            cpu_threadpool,
            io_threadpool,
            symbolication,
            objects,
            config,
            download_svc,
        })
    }

    pub fn io_pool(&self) -> ThreadPool {
        self.io_threadpool.clone()
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

/// Typedef for the application type.
pub type ServiceApp = App<ServiceState>;
