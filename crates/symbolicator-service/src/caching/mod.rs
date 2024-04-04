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
//! We collect a couple of metrics, each of those is tagged with a `cache` field that corresponds to
//! the cache item type. Here is a list of metrics that are collected:
//!
//! - `caches.access`: All accesses.
//! - `caches.memory.hit`: Accesses served by the in-memory layer.
//! - `caches.file.hit`: Accesses served by the file-system layer.
//! - `services.shared_cache.fetch(hit:true)`: Accesses served by the shared-cache layer.
//! - `caches.computation`: Actual computations being run, and not served by any of the caching layers.
//!
//! NOTE: The sum of shared-cache hits and computations can exceed the number of cache misses of
//! previous layers in case of lazy cache recomputation.
//!
//! Various other metrics are being collected as well, including:
//! - `caches.file.size`: A histogram for the size (in bytes) of the successfully loaded / written cache files.
//! - `caches.file.write`: The number of caches being written to disk.
//!   This should match `caches.computation` if the file-system layer is enabled.
//! - TODO: list all the other metrics that are missing here :-)
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
//! The [`CacheKey`] is used both as the key for the in-memory cache, as well as the path of the
//! file-system cache. It contains some human-readable (but not necessarily machine-readable)
//! metadata. This metadata encodes the information what this cache contains, and where it came from.
//! For cache artifacts that contain data from more than one source, it should contain all the
//! information from all the sources that contributed to the cached file.
//!
//! The [`CacheKeyBuilder`] provides a [`std::fmt::Write`] interface with other helper methods to
//! construct the human-readable metadata. This metadata is then SHA256-hashed to form the filename
//! for the file-system cache.
//!
//! **NOTE**: Care must be taken to make sure that this metadata is stable, as it would otherwise
//! lead to bad cache reuse.
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
//! Creating a new Cache involves quite some moving parts.
//! The primary entry point is the [`CacheItemRequest`] trait. The [`CacheItemRequest::compute`]
//! function is used to asynchronously compute / write the item into a [`NamedTempFile`](tempfile::NamedTempFile).
//! This file is a `&mut` reference, and one might use [`std::mem::swap`] to replace it with a
//! newly created temporary file.
//! Once the file is written, it is considered to be immutable, and loadable synchronously via the
//! [`CacheItemRequest::load`] method. This function returns a [`CacheEntry`] and is theoretically
//! fallible. However, failing to load a previously written cache file in most cases should be
//! considered a [`CacheError::InternalError`], and is unexpected to occur.
//!
//! A [`CacheItemRequest`] also needs to specify [`CacheVersions`] which are used for cache fallback
//! as explained in detail above. Newly added caches should start with version `1`, and all the
//! cache versions and their versioning history should be recorded in [`caches::versions`](crate::caches::versions).
//!
//! A new cache item also needs a new [`CacheName`] and [`Cache`] configuration. This should be added
//! to [`Caches`] down below as well, and to [`Caches::cleanup`] to properly clean up cache files.
//!
//! Last but not least, it might be worth adding a simplified wrapper function around
//! [`Cacher::compute_memoized`] to hide all the details of the [`CacheItemRequest`] struct and how
//! the cache item itself is being computed / loaded.

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
pub use cache_key::{CacheKey, CacheKeyBuilder};
pub use cleanup::cleanup;
pub use config::CacheName;
pub use fs::{Cache, ExpirationStrategy, ExpirationTime};
pub use memory::{CacheItemRequest, CacheVersions, Cacher};
pub use shared_cache::{CacheStoreReason, SharedCacheConfig, SharedCacheRef, SharedCacheService};

pub struct Caches {
    /// Caches for object files.
    pub objects: Cache,
    /// Caches for object metadata.
    pub object_meta: Cache,
    /// Caches for auxiliary DIF files.
    pub auxdifs: Cache,
    /// Caches for il2cpp line mapping files.
    pub il2cpp: Cache,
    /// Caches for [`symbolic::symcache::SymCache`], used by.
    pub symcaches: Cache,
    /// Caches for breakpad CFI info.
    pub cficaches: Cache,
    /// PortablePDB files.
    pub ppdb_caches: Cache,
    /// `SourceMapCache` files.
    pub sourcemap_caches: Cache,
    /// Source files.
    pub sourcefiles: Cache,
    /// Store for minidump data symbolicator failed to process, for diagnostics purposes
    pub diagnostics: Cache,
    /// Proguard mapping files.
    pub proguard: Cache,
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

        // NOTE: A small cache item with all its structures is around ~100 bytes, which gives us an
        // approximate upper bound of items in the cache.
        // Most items are a lot larger in reality, but giving concrete numbers here is hard to do.
        // Items that use `mmap` under the hood need special attention as well, as those keep a
        // file descriptor open, and use up *virtual memory* instead of actual memory.
        let default_cap = 100 * 1024;
        let in_memory = &config.caches.in_memory;

        Ok(Self {
            objects: Cache::from_config(
                CacheName::Objects,
                config,
                config.caches.downloaded.into(),
                max_lazy_redownloads.clone(),
                default_cap,
            )?,
            object_meta: Cache::from_config(
                CacheName::ObjectMeta,
                config,
                config.caches.derived.into(),
                max_lazy_recomputations.clone(),
                in_memory.object_meta_capacity,
            )?,
            auxdifs: Cache::from_config(
                CacheName::Auxdifs,
                config,
                config.caches.downloaded.into(),
                max_lazy_redownloads.clone(),
                default_cap,
            )?,
            il2cpp: Cache::from_config(
                CacheName::Il2cpp,
                config,
                config.caches.downloaded.into(),
                max_lazy_redownloads.clone(),
                default_cap,
            )?,
            symcaches: Cache::from_config(
                CacheName::Symcaches,
                config,
                config.caches.derived.into(),
                max_lazy_recomputations.clone(),
                default_cap,
            )?,
            cficaches: Cache::from_config(
                CacheName::Cficaches,
                config,
                config.caches.derived.into(),
                max_lazy_recomputations.clone(),
                in_memory.cficaches_capacity,
            )?,
            ppdb_caches: Cache::from_config(
                CacheName::PpdbCaches,
                config,
                config.caches.derived.into(),
                max_lazy_recomputations.clone(),
                default_cap,
            )?,
            sourcemap_caches: Cache::from_config(
                CacheName::SourceMapCaches,
                config,
                config.caches.derived.into(),
                max_lazy_recomputations,
                default_cap,
            )?,
            sourcefiles: Cache::from_config(
                CacheName::SourceFiles,
                config,
                config.caches.downloaded.into(),
                max_lazy_redownloads.clone(),
                default_cap,
            )?,
            diagnostics: Cache::from_config(
                CacheName::Diagnostics,
                config,
                config.caches.diagnostics.into(),
                Default::default(),
                default_cap,
            )?,
            proguard: Cache::from_config(
                CacheName::Proguard,
                config,
                config.caches.downloaded.into(),
                max_lazy_redownloads,
                in_memory.proguard_capacity,
            )?,
        })
    }
}
