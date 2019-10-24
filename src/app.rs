//! Exposes the command line application.
use std::sync::Arc;

use actix_web::App;

use crate::actors::{
    cficaches::CfiCacheActor, objects::ObjectsActor, symbolication::SymbolicationActor,
    symcaches::SymCacheActor,
};
use crate::cache::Caches;
use crate::config::Config;
use crate::utils::futures::ThreadPool;
use crate::utils::http;

/// The shared state for the service.
#[derive(Clone, Debug)]
pub struct ServiceState {
    /// Thread pool instance reserved for CPU-intensive tasks.
    pub cpu_threadpool: ThreadPool,
    /// Thread pool instance reserved for IO-intensive tasks.
    pub io_threadpool: ThreadPool,
    /// Actor for minidump and stacktrace processing
    pub symbolication: Arc<SymbolicationActor>,
    /// Actor for downloading and caching objects (no symcaches or cficaches)
    pub objects: Arc<ObjectsActor>,
    /// The config object.
    pub config: Arc<Config>,
}

impl ServiceState {
    pub fn create(config: Config) -> Self {
        let config = Arc::new(config);

        if !config.connect_to_reserved_ips {
            http::start_safe_connector();
        }

        let caches = Caches::new(&config);

        let cpu_threadpool = ThreadPool::new();
        let io_threadpool = ThreadPool::new();

        let objects = Arc::new(ObjectsActor::new(
            caches.object_meta,
            caches.objects,
            io_threadpool.clone(),
        ));

        let symcaches = Arc::new(SymCacheActor::new(
            caches.symcaches,
            objects.clone(),
            cpu_threadpool.clone(),
        ));

        let cficaches = Arc::new(CfiCacheActor::new(
            caches.cficaches,
            objects.clone(),
            cpu_threadpool.clone(),
        ));

        let symbolication = Arc::new(SymbolicationActor::new(
            objects.clone(),
            symcaches,
            cficaches,
            cpu_threadpool.clone(),
        ));

        Self {
            cpu_threadpool,
            io_threadpool,
            symbolication,
            objects,
            config: config.clone(),
        }
    }
}

/// Typedef for the application type.
pub type ServiceApp = App<ServiceState>;
