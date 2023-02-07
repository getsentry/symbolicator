//! # Symbolicator Caching infrastructure
//!
//! Caching is front and center in Symbolicator. To guarantee smooth operation, Symbolicator heavily
//! caches slow downloads and CPU intensive computations.
//! This module includes all the code that deals with the different layers of caching, our central
//! [`CacheError`] type, and contains an explanation of how all this works and why it exists.
//!
//! ## Cache Layers
//!
//! Symbolicator has a multi-layered caching architecture, consisting of the following layers:
//!
//! - An in-memory caching layer which is currently used for request coalescing
//!   (deduplicating concurrent accesses).
//! - A file-system layer that persists the results of downloads and computations to the file system,
//!   and also persists errors happening during those.
//! - A shared cache layer which is backed by a shared GCS bucket, to more evenly distribute
//!   the load to multiple Symbolicator instances, and to help the fresh startup path without any
//!   file-system caches available. The shared-cache layer does not persist errors, but only
//!   "immutable" cache results.
//!
//! A cache request goes through the following steps:
//! - First, it goes through the in-memory layer.
//! - On miss, it will try to load the cache item from the file-system, if enabled
//! - On miss, it will try the shared cache next, if enabled.
//! - On miss, it will finally generate a fresh item, by downloading or doing a computation,
//!   possibly requesting another cached item.
//! - The freshly computed item will be stored on the file-system and uploaded to the shared cache
//!   in case both caches are enabled.
//!
//! ### Metrics
//!
//! TODO
//!
//! ### Configuration
//!
//! The request-coalescing part of the in-memory caching layer is always active. Additional
//! configuration for the in-memory cache may be added in the future.
//!
//! The rest of the caching infrastructure is gated by the [`Config::cache_dir`] option. If no
//! `cache_dir` is configured, requests are computed directly after the in-memory cache.
//!
//! [`Config::caches`] is responsible for the configuration of the file-system caches.
//! It is divided into the categories "downloaded" and "derived". Both categories have settings
//! related to cache expiration. Additionally, they allow configuring a limit on concurrent lazy
//! re-downloads and re-computation. The limit applies to the whole category at once.
//! See the section on [`CacheVersions`] for more details.
//!
//! The `retry_X_after` options specify a time-to-live after which the cache expires and will be
//! re-computed. The `max_unused_for` option is rather a time-to-idle value, after which the item
//! will be evicted. File-system `mtime` is used to check for these. Cache items that are in use
//! will have their `mtime` updated once an hour to keep them from expiring.
//!
//! The "downloaded" category defaults to keeping entries alive for up to 24 hours, will retry
//! "missing" items every hour, and "malformed" items every 24 hours.
//! The "derived" category will keep entries alive for up to 7 days, and will also retry "missing"
//! entries every hour, and "malformed" entries every 24 hours.
//!
//! A "successful" entry is considered immutable and it will be reused indefinitely as long as it
//! is being actively used.
//! FIXME: One current exception to this is the `should_load` functionality that is used in combination
//! with SymCache entries that are "mutable" depending on availability of secondary mapping files.
//! This is subject to change: <https://github.com/getsentry/symbolicator/issues/983>
//!
//! The [`SharedCacheConfig`] is optional, and no shared cache will be used when it is absent. The
//! configuration is done by providing a GCS bucket and `service_account_path`. A file-system based
//! shared cache implementation exists for testing purposes.
//!
//! ## [`CacheEntry`] / [`CacheError`]
//!
//! The caching layer primarily deals with [`CacheEntry`]s, which are just an alias for a [`Result`]
//! around a [`CacheError`].
//!
//! [`CacheError`] encodes opaque errors, most of which happen during downloading of files. These
//! errors will be exposed to the end user in one form or other. The most important is
//! [`CacheError::NotFound`].
//!
//! Other than that, [`CacheError::Malformed`] signals a malformed source file, or a problem on
//! our end processing that file. This variant is logged internally to be able to fix these
//! problems which are indeed fixable.
//!
//! Lastly, the [`CacheError::InternalError`] is a catch-all for unexpected errors that might happen.
//! This includes filesystem access errors, or errors loading file formats that were already validated.
//! Internal errors should ideally never happen, and they are logged internally if they do.
//!
//! ## [`CacheKey`]
//!
//! TODO
//!
//! ## Cache Fallback and [`CacheVersions`]
//!
//! Each type of cache defines both a current cache version and a list of fallback versions. Different
//! versions correspond to separate directories on the file system. When an item is looked up in a file
//! system cache, the current version will be tried first, followed by each fallback version in order. Then:
//!
//! 1. If an entry for the current version was found, we just use it.
//! 2. If an entry for a fallback version was found, we use it and schedule a redownload/recomputation for the current version.
//! 3. If no entry was found at all, we compute the item and store it under the current version.
//!
//! This procedure ensures that when we update a cache's format to a new version, we don't immediately throw away
//! all old cache entries if they're still usable, but rather migrate to the new version over time.
//!
//! The number of simultaneous redownloads/recomputations of outdated cache items can be configured via the options
//! `max_lazy_redownloads` (default: 50) for "downloaded" caches and `max_lazy_recomputations` (default: 20) for
//! "derived" caches, respectively.
//!
//! ## Using the Cache / Creating a cached item
//!
//! TODO
//!

use std::io;
use std::sync::atomic::AtomicIsize;
use std::sync::Arc;

use crate::config::Config;

mod cache_error;
mod cache_key;
mod cleanup;
mod config;
mod fs;
mod memory;
mod shared_cache;
#[cfg(test)]
mod tests;

pub use cache_error::{CacheEntry, CacheError};
pub use cache_key::CacheKey;
pub use cleanup::cleanup;
pub use config::CacheName;
pub use fs::{Cache, ExpirationStrategy, ExpirationTime};
pub use memory::{CacheItemRequest, CacheVersions, Cacher};
pub use shared_cache::{CacheStoreReason, SharedCacheConfig, SharedCacheRef, SharedCacheService};

pub struct Caches {
    /// Caches for object files, used by [`crate::services::objects::ObjectsActor`].
    pub objects: Cache,
    /// Caches for object metadata, used by [`crate::services::objects::ObjectsActor`].
    pub object_meta: Cache,
    /// Caches for auxiliary DIF files, used by [`crate::services::bitcode::BitcodeService`].
    pub auxdifs: Cache,
    /// Caches for il2cpp line mapping files, used by [`crate::services::il2cpp::Il2cppService`].
    pub il2cpp: Cache,
    /// Caches for [`symbolic::symcache::SymCache`], used by
    /// [`crate::services::symcaches::SymCacheActor`].
    pub symcaches: Cache,
    /// Caches for breakpad CFI info, used by [`crate::services::cficaches::CfiCacheActor`].
    pub cficaches: Cache,
    pub ppdb_caches: Cache,
    pub sourcemap_caches: Cache,
    /// Store for diagnostics data symbolicator failed to process, used by
    /// [`crate::services::symbolication::SymbolicationActor`].
    pub diagnostics: Cache,
}

impl Caches {
    pub fn from_config(config: &Config) -> io::Result<Self> {
        // The minimum value here is clamped to 1, as it would otherwise completely disable lazy
        // re-generation. We might as well decide to hard `panic!` on startup if users have
        // misconfigured this instead of silently correcting it to a value that actually makes sense.
        let max_lazy_redownloads = Arc::new(AtomicIsize::new(
            config.caches.downloaded.max_lazy_redownloads.max(1),
        ));
        let max_lazy_recomputations = Arc::new(AtomicIsize::new(
            config.caches.derived.max_lazy_recomputations.max(1),
        ));

        let tmp_dir = config.cache_dir("tmp");
        Ok(Self {
            objects: {
                let path = config.cache_dir("objects");
                Cache::from_config(
                    CacheName::Objects,
                    path,
                    tmp_dir.clone(),
                    config.caches.downloaded.into(),
                    max_lazy_redownloads.clone(),
                )?
            },
            object_meta: {
                let path = config.cache_dir("object_meta");
                Cache::from_config(
                    CacheName::ObjectMeta,
                    path,
                    tmp_dir.clone(),
                    config.caches.derived.into(),
                    max_lazy_recomputations.clone(),
                )?
            },
            auxdifs: {
                let path = config.cache_dir("auxdifs");
                Cache::from_config(
                    CacheName::Auxdifs,
                    path,
                    tmp_dir.clone(),
                    config.caches.downloaded.into(),
                    max_lazy_redownloads.clone(),
                )?
            },
            il2cpp: {
                let path = config.cache_dir("il2cpp");
                Cache::from_config(
                    CacheName::Il2cpp,
                    path,
                    tmp_dir.clone(),
                    config.caches.downloaded.into(),
                    max_lazy_redownloads,
                )?
            },
            symcaches: {
                let path = config.cache_dir("symcaches");
                Cache::from_config(
                    CacheName::Symcaches,
                    path,
                    tmp_dir.clone(),
                    config.caches.derived.into(),
                    max_lazy_recomputations.clone(),
                )?
            },
            cficaches: {
                let path = config.cache_dir("cficaches");
                Cache::from_config(
                    CacheName::Cficaches,
                    path,
                    tmp_dir.clone(),
                    config.caches.derived.into(),
                    max_lazy_recomputations.clone(),
                )?
            },
            ppdb_caches: {
                let path = config.cache_dir("ppdb_caches");
                Cache::from_config(
                    CacheName::PpdbCaches,
                    path,
                    tmp_dir.clone(),
                    config.caches.derived.into(),
                    max_lazy_recomputations.clone(),
                )?
            },
            sourcemap_caches: {
                let path = config.cache_dir("sourcemap_caches");
                Cache::from_config(
                    CacheName::SourceMapCaches,
                    path,
                    tmp_dir.clone(),
                    config.caches.derived.into(),
                    max_lazy_recomputations,
                )?
            },
            diagnostics: {
                let path = config.cache_dir("diagnostics");
                Cache::from_config(
                    CacheName::Diagnostics,
                    path,
                    tmp_dir,
                    config.caches.diagnostics.into(),
                    Default::default(),
                )?
            },
        })
    }
}
