//! Exposes the command line application.
use std::sync::Arc;

use actix_web::App;
use failure::{Fail, ResultExt};

use crate::actors::{
    cficaches::CfiCacheActor, objects::ObjectsActor, symbolication::SymbolicationActor,
    symcaches::SymCacheActor,
};
use crate::cache::Caches;
use crate::config::Config;
use crate::utils::futures::{RemoteThread, ThreadPool};
use crate::utils::http;

/// Variants of [ServiceStateError].
#[derive(Clone, Copy, Debug, Fail)]
pub enum ServiceStateErrorKind {
    #[fail(display = "failed to create process pool")]
    Spawn,
}

symbolic::common::derive_failure!(
    ServiceStateError,
    ServiceStateErrorKind,
    doc = "Error constructing the service state."
);

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
    download_svc: Arc<crate::services::download::Downloader>,
}

impl ServiceState {
    pub fn create(config: Config) -> Result<Self, ServiceStateError> {
        let config = Arc::new(config);

        if !config.connect_to_reserved_ips {
            http::start_safe_connector();
        }

        let cpu_threadpool = ThreadPool::new();
        let io_threadpool = ThreadPool::new();
        let io_thread = RemoteThread::new();

        let download_svc = Arc::new(crate::services::download::Downloader::new(io_thread));

        let caches = Caches::new(&config);
        let objects = ObjectsActor::new(
            caches.object_meta,
            caches.objects,
            io_threadpool.clone(),
            download_svc.clone(),
        );
        let symcaches = SymCacheActor::new(
            caches.symcaches,
            objects.clone(),
            cpu_threadpool.clone(),
            download_svc.clone(),
        );
        let cficaches = CfiCacheActor::new(
            caches.cficaches,
            objects.clone(),
            cpu_threadpool.clone(),
            download_svc.clone(),
        );
        let spawnpool =
            procspawn::Pool::new(num_cpus::get()).context(ServiceStateErrorKind::Spawn)?;

        let symbolication = SymbolicationActor::new(
            objects.clone(),
            symcaches,
            cficaches,
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

    pub fn download_svc(&self) -> Arc<crate::services::download::Downloader> {
        self.download_svc.clone()
    }
}

/// Typedef for the application type.
pub type ServiceApp = App<ServiceState>;
