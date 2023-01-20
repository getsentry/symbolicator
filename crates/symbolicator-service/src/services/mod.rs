//! Provides the internal Symbolicator services and a way to initialize them.
//!
//! Symbolicator operates a number of independent services defined in this module for downloading,
//! cache management, and symbolication.
//! The main [`create_service`] fn creates all these internal services according to the provided
//! [`Config`] and returns a [`SymbolicationActor`] as the main Symbolicator interface, and an
//! [`ObjectsActor`] which abstracts object access.
//!
//! The internal services require a separate asynchronous runtimes dedicated for I/O-intensive work,
//! such as downloads and access to the shared cache.

use anyhow::{Context, Result};

use crate::cache::Caches;
use crate::config::Config;

pub mod bitcode;
pub mod cacher;
pub mod cficaches;
pub mod derived;
pub mod download;
mod fetch_file;
pub mod il2cpp;
mod minidump;
pub mod objects;
pub mod ppdb_caches;
pub mod shared_cache;
pub mod symbolication;
pub mod symcaches;

use self::bitcode::BitcodeService;
use self::cficaches::CfiCacheActor;
use self::download::DownloadService;
use self::il2cpp::Il2cppService;
use self::objects::ObjectsActor;
use self::ppdb_caches::PortablePdbCacheActor;
use self::shared_cache::SharedCacheService;
use self::symbolication::SymbolicationActor;
use self::symcaches::SymCacheActor;
pub use fetch_file::fetch_file;

pub fn create_service(
    config: &Config,
    io_pool: tokio::runtime::Handle,
) -> Result<(SymbolicationActor, ObjectsActor)> {
    let caches = Caches::from_config(config).context("failed to create local caches")?;
    caches
        .clear_tmp(config)
        .context("failed to clear tmp caches")?;

    let downloader = DownloadService::new(config, io_pool.clone());

    let shared_cache = SharedCacheService::new(config.shared_cache.clone(), io_pool);

    let objects = ObjectsActor::new(
        caches.object_meta,
        caches.objects,
        shared_cache.clone(),
        downloader.clone(),
    );

    let bitcode = BitcodeService::new(caches.auxdifs, shared_cache.clone(), downloader.clone());

    let il2cpp = Il2cppService::new(caches.il2cpp, shared_cache.clone(), downloader);

    let symcaches = SymCacheActor::new(
        caches.symcaches,
        shared_cache.clone(),
        objects.clone(),
        bitcode,
        il2cpp,
    );

    let cficaches = CfiCacheActor::new(caches.cficaches, shared_cache.clone(), objects.clone());

    let ppdb_caches = PortablePdbCacheActor::new(caches.ppdb_caches, shared_cache, objects.clone());

    let symbolication = SymbolicationActor::new(
        objects.clone(),
        symcaches,
        cficaches,
        ppdb_caches,
        caches.diagnostics,
    );

    Ok((symbolication, objects))
}
