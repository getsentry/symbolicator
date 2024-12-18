//! This module defines the various [`CacheVersions`] of the various caches that Symbolicator uses.
//!
//! # How to version
//!
//! The initial "unversioned" version is `0`.
//! Whenever we want to increase the version in order to re-generate stale/broken cache files,
//! we need to:
//!
//! * increase the `current` version.
//! * prepend the `current` version to the `fallbacks`.
//! * it is also possible to skip a version, in case a broken deploy needed to
//!   be reverted which left behind broken cache files.
//!
//! Some of the versioned caches are also tied to format versions defined in [`symbolic`].
//! For those cases, there are static assertions that are a reminder to also bump the cache version.

use crate::caching::CacheVersions;

/// CFI cache, with the following versions:
///
/// - `4`: Recomputation to use new `CacheKey` format.
///
/// - `3`: Proactive bump, as a bug in shared cache could have potentially
///   uploaded `v1` cache files as `v2` erroneously.
///
/// - `2`: Allow underflow in Win-x64 CFI which allows loading registers from outside the stack frame.
///
/// - `1`: Generate higher fidelity CFI for Win-x64 binaries.
///
/// - `0`: Initial version.
pub const CFICACHE_VERSIONS: CacheVersions = CacheVersions {
    current: 4,
    fallbacks: &[],
};
static_assert!(symbolic::cfi::CFICACHE_LATEST_VERSION == 2);

/// SymCache, with the following versions:
///
/// - `7`: Fixes inlinee lookup. (<https://github.com/getsentry/symbolic/pull/883>)
///
/// - `6`: Recomputation to use new `CacheKey` format.
///
/// - `5`: Proactive bump, as a bug in shared cache could have potentially
///   uploaded `v2` cache files as `v3` (and later `v4`) erroneously.
///
/// - `4`: An updated symbolic symcache that uses a LEB128 prefixed string table.
///
/// - `3`: Another round of fixes in symcache generation:
///        - fixes problems with split inlinees and inlinees appearing twice in the call chain
///        - undecorate Windows C-decorated symbols in symcaches
///
/// - `2`: Tons of fixes/improvements in symcache generation:
///        - fixed problems with DWARF functions that have the
///          same line records for different inline hierarchy
///        - fixed problems with PDB where functions have line records that don't belong to them
///        - fixed problems with PDB/DWARF when parent functions don't have matching line records
///        - using a new TypeFormatter for PDB that can pretty-print function arguments
///
/// - `1`: New binary format based on instruction addr lookup.
///
/// - `0`: Initial version.
pub const SYMCACHE_VERSIONS: CacheVersions = CacheVersions {
    current: 7,
    fallbacks: &[6],
};
static_assert!(symbolic::symcache::SYMCACHE_VERSION == 8);

/// Data / Objects cache, with the following versions:
///
/// - `1`: Recomputation to use new `CacheKey` format.
///
/// - `0`: Initial version.
pub const OBJECTS_CACHE_VERSIONS: CacheVersions = CacheVersions {
    current: 1,
    fallbacks: &[],
};

/// Objects Meta cache, with the following versions:
///
/// - `1`: Recomputation to use new `CacheKey` format.
///
/// - `0`: Initial version.
pub const META_CACHE_VERSIONS: CacheVersions = CacheVersions {
    current: 1,
    fallbacks: &[],
};

/// Portable PDB cache, with the following versions:
///
/// - `3`: Skips hidden SequencePoints, and thus avoids outputting `lineno: 0`.
///
/// - `2`: Recomputation to use new `CacheKey` format.
///
/// - `1`: Initial version.
pub const PPDB_CACHE_VERSIONS: CacheVersions = CacheVersions {
    current: 3,
    fallbacks: &[2],
};

/// SourceMapCache, with the following versions:
///
/// - `1`: Initial version.
pub const SOURCEMAP_CACHE_VERSIONS: CacheVersions = CacheVersions {
    current: 1,
    fallbacks: &[],
};

/// Il2cpp cache, with the following versions:
///
/// - `1`: Recomputation to use new `CacheKey` format.
///
/// - `0`: Initial version.
pub const IL2CPP_CACHE_VERSIONS: CacheVersions = CacheVersions {
    current: 1,
    fallbacks: &[],
};

/// Bitcode / Auxdif (plist / bcsymbolmap) cache, with the following versions:
///
/// - `1`: Recomputation to use new `CacheKey` format.
///
/// - `0`: Initial version.
pub const BITCODE_CACHE_VERSIONS: CacheVersions = CacheVersions {
    current: 1,
    fallbacks: &[],
};

/// Source Files Cache, with the following versions:
///
/// - `1`: Initial version.
pub const SOURCEFILES_CACHE_VERSIONS: CacheVersions = CacheVersions {
    current: 1,
    fallbacks: &[],
};

/// Bundle Index Cache, with the following versions:
///
/// - `1`: Initial version.
pub const BUNDLE_INDEX_CACHE_VERSIONS: CacheVersions = CacheVersions {
    current: 1,
    fallbacks: &[],
};

/// Proguard Cache, with the following versions:
///
/// - `1`: Initial version.
/// - `2`: Use proguard cache format (<https://github.com/getsentry/symbolicator/pull/1491>).
pub const PROGUARD_CACHE_VERSIONS: CacheVersions = CacheVersions {
    current: 2,
    fallbacks: &[],
};
