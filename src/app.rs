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
    /// Actor for minidump and stacktrace processing
    symbolication: SymbolicationActor,
    /// Actor for downloading and caching objects (no symcaches or cficaches)
    objects: ObjectsActor,
    /// The config object.
    config: Arc<Config>,
}

impl ServiceState {
    pub fn create(config: Config) -> Self {
        let config = Arc::new(config);

        if !config.connect_to_reserved_ips {
            http::start_safe_connector();
        }

        let cpu_pool = ThreadPool::new();
        let io_pool = ThreadPool::new();

        let caches = Caches::new(&config);
        let objects = ObjectsActor::new(caches.object_meta, caches.objects, io_pool);
        let symcaches = SymCacheActor::new(caches.symcaches, objects.clone(), cpu_pool.clone());
        let cficaches = CfiCacheActor::new(caches.cficaches, objects.clone(), cpu_pool.clone());

        let symbolication =
            SymbolicationActor::new(objects.clone(), symcaches, cficaches, cpu_pool);

        Self {
            symbolication,
            objects,
            config,
        }
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
