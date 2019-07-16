use std::sync::Arc;

use tokio_threadpool::ThreadPool;

use crate::cache::Caches;
use crate::config::Config;
use crate::utils::http;

pub mod cache;
pub mod cficaches;
pub mod objects;
pub mod symbolication;
pub mod symcaches;

use self::cficaches::CfiCacheActor;
use self::objects::ObjectsActor;
use self::symbolication::SymbolicationActor;
use self::symcaches::SymCacheActor;

#[derive(Clone, Debug)]
pub struct Service {
    config: Arc<Config>,
    // cpu_pool: Arc<ThreadPool>,
    io_pool: Arc<ThreadPool>,
    symbolication: Arc<SymbolicationActor>,
    objects: Arc<ObjectsActor>,
}

impl Service {
    pub fn create(config: Config) -> Self {
        let config = Arc::new(config);

        http::allow_reserved_ips(config.connect_to_reserved_ips);

        let caches = Caches::new(&config);
        let cpu_pool = Arc::new(ThreadPool::new());
        let io_pool = Arc::new(ThreadPool::new());

        let objects = Arc::new(ObjectsActor::new(
            caches.object_meta,
            caches.objects,
            io_pool.clone(),
        ));

        let symcaches = Arc::new(SymCacheActor::new(
            caches.symcaches,
            objects.clone(),
            cpu_pool.clone(),
        ));

        let cficaches = Arc::new(CfiCacheActor::new(
            caches.cficaches,
            objects.clone(),
            cpu_pool.clone(),
        ));

        let symbolication = Arc::new(SymbolicationActor::new(
            objects.clone(),
            symcaches,
            cficaches,
            cpu_pool.clone(),
        ));

        Self {
            // cpu_pool,
            io_pool,
            symbolication,
            objects,
            config,
        }
    }

    pub fn config(&self) -> Arc<Config> {
        self.config.clone()
    }

    // TODO(ja): Keep or remove?
    // pub fn cpu_pool(&self) -> Arc<ThreadPool> {
    //     self.cpu_pool.clone()
    // }

    pub fn io_pool(&self) -> Arc<ThreadPool> {
        self.io_pool.clone()
    }

    pub fn symbolication(&self) -> Arc<SymbolicationActor> {
        self.symbolication.clone()
    }

    pub fn objects(&self) -> Arc<ObjectsActor> {
        self.objects.clone()
    }
}
