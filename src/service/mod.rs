use std::sync::Arc;

use crate::cache::Caches;
use crate::config::Config;
use crate::utils::futures::ThreadPool;
use crate::utils::http;

pub mod cache;
pub mod cficaches;
pub mod download;
pub mod objects;
pub mod symbolication;
pub mod symcaches;

use self::cficaches::CfiCacheActor;
use self::download::Downloader;
use self::objects::ObjectsActor;
use self::symbolication::SymbolicationActor;
use self::symcaches::SymCacheActor;

#[derive(Clone, Debug)]
pub struct Service {
    config: Arc<Config>,
    symbolication: Arc<SymbolicationActor>,
    objects: Arc<ObjectsActor>,
}

impl Service {
    pub fn create(config: Config) -> Self {
        let config = Arc::new(config);

        http::allow_reserved_ips(config.connect_to_reserved_ips);

        let caches = Caches::new(&config);

        let cache_pool = ThreadPool::new();
        let symbolication_pool = ThreadPool::new();
        let downloader = Downloader::new();

        let objects = Arc::new(ObjectsActor::new(
            caches.object_meta,
            caches.objects,
            cache_pool.clone(),
            downloader,
        ));

        let symcaches = Arc::new(SymCacheActor::new(
            caches.symcaches,
            objects.clone(),
            cache_pool.clone(),
        ));

        let cficaches = Arc::new(CfiCacheActor::new(
            caches.cficaches,
            objects.clone(),
            cache_pool.clone(),
        ));

        let symbolication = Arc::new(SymbolicationActor::new(
            objects.clone(),
            symcaches,
            cficaches,
            symbolication_pool,
        ));

        Self {
            symbolication,
            objects,
            config,
        }
    }

    pub fn config(&self) -> Arc<Config> {
        self.config.clone()
    }

    pub fn symbolication(&self) -> Arc<SymbolicationActor> {
        self.symbolication.clone()
    }

    pub fn objects(&self) -> Arc<ObjectsActor> {
        self.objects.clone()
    }
}
