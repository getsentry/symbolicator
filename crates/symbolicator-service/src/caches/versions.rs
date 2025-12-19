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
//! * add the `current` version to the `previous` versions.
//! * it is also possible to skip a version, in case a broken deploy needed to
//!   be reverted which left behind broken cache files.
//!
//! Some of the versioned caches are also tied to format versions defined in [`symbolic`].
//! For those cases, there are static assertions that are a reminder to also bump the cache version.

use std::fmt;

/// How to format cache keys into file paths.
#[derive(Clone, Copy, Debug)]
pub enum CachePathFormat {
    /// Format cache keys as `xx/xxxxxx/xxx…`.
    V1,
    /// Format cache keys as `xx/xx/xxx…`
    V2,
}

#[derive(Clone, Copy, Debug)]
pub struct CacheVersion {
    /// The version number.
    pub number: u32,
    /// The way in which cache keys should be formatted
    /// into file paths for this version.
    pub path_format: CachePathFormat,
}

impl CacheVersion {
    /// Creates a new `CacheVersion` with the given number and path format.
    pub const fn new(number: u32, path_format: CachePathFormat) -> Self {
        Self {
            number,
            path_format,
        }
    }
}

impl fmt::Display for CacheVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.number)
    }
}

impl PartialEq for CacheVersion {
    fn eq(&self, other: &Self) -> bool {
        self.number == other.number
    }
}

impl Eq for CacheVersion {}

/// Cache Version Configuration used during cache lookup and generation.
///
/// The `current` version is tried first, and written during cache generation.
/// The `fallback` versions are tried next, in first to last order. They are used only for cache
/// lookups, but never for writing.
///
/// The version `0` is special in the sense that it is not used as part of the resulting cache
/// file path, and generates the same paths as "legacy" unversioned cache files.
#[derive(Clone, Debug)]
pub struct CacheVersions {
    /// The current cache version that is being looked up, and used for writing
    pub current: CacheVersion,
    /// A list of fallback cache versions that are being tried on lookup,
    /// in descending order of priority.
    pub fallbacks: &'static [CacheVersion],
    /// A list of all previous cache versions.
    ///
    /// This list includes both fallback and incompatible
    /// versions.
    pub previous: &'static [CacheVersion],
}

/// CFI cache, with the following versions:
///
/// - `5`: Restructuring the cache directory format.
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
    current: CacheVersion::new(5, CachePathFormat::V2),
    fallbacks: &[CacheVersion::new(4, CachePathFormat::V1)],
    previous: &[
        CacheVersion::new(1, CachePathFormat::V1),
        CacheVersion::new(2, CachePathFormat::V1),
        CacheVersion::new(3, CachePathFormat::V1),
        CacheVersion::new(4, CachePathFormat::V1),
    ],
};
static_assert!(symbolic::cfi::CFICACHE_LATEST_VERSION == 2);

/// SymCache, with the following versions:
///
/// - `11`: Fixes symcache generations for DWARF files with unusual tombstone addresses (<https://github.com/getsentry/symbolic/pull/937>)
///
/// - `10`: Fixes symcache generation for functions with no lines (<https://github.com/getsentry/symbolic/pull/930>)
///
/// - `9`: Fixes symcache generation from symbol tables (<https://github.com/getsentry/symbolic/pull/915>)
///
/// - `8`: Restructuring the cache directory format.
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
///  - fixes problems with split inlinees and inlinees appearing twice in the call chain
///  - undecorate Windows C-decorated symbols in symcaches
///
/// - `2`: Tons of fixes/improvements in symcache generation:
///  - fixed problems with DWARF functions that have the
///    same line records for different inline hierarchy
///  - fixed problems with PDB where functions have line records that don't belong to them
///  - fixed problems with PDB/DWARF when parent functions don't have matching line records
///  - using a new TypeFormatter for PDB that can pretty-print function arguments
///
/// - `1`: New binary format based on instruction addr lookup.
///
/// - `0`: Initial version.
pub const SYMCACHE_VERSIONS: CacheVersions = CacheVersions {
    current: CacheVersion::new(11, CachePathFormat::V2),
    fallbacks: &[
        CacheVersion::new(10, CachePathFormat::V2),
        CacheVersion::new(9, CachePathFormat::V2),
        CacheVersion::new(8, CachePathFormat::V2),
        CacheVersion::new(7, CachePathFormat::V1),
        CacheVersion::new(6, CachePathFormat::V1),
    ],
    previous: &[
        CacheVersion::new(1, CachePathFormat::V1),
        CacheVersion::new(2, CachePathFormat::V1),
        CacheVersion::new(3, CachePathFormat::V1),
        CacheVersion::new(4, CachePathFormat::V1),
        CacheVersion::new(5, CachePathFormat::V1),
        CacheVersion::new(6, CachePathFormat::V1),
        CacheVersion::new(7, CachePathFormat::V1),
        CacheVersion::new(8, CachePathFormat::V2),
        CacheVersion::new(9, CachePathFormat::V2),
        CacheVersion::new(10, CachePathFormat::V2),
    ],
};
static_assert!(symbolic::symcache::SYMCACHE_VERSION == 8);

/// Data / Objects cache, with the following versions:
///
/// - `2`: Restructuring the cache directory format.
///
/// - `1`: Recomputation to use new `CacheKey` format.
///
/// - `0`: Initial version.
pub const OBJECTS_CACHE_VERSIONS: CacheVersions = CacheVersions {
    current: CacheVersion::new(2, CachePathFormat::V2),
    fallbacks: &[CacheVersion::new(1, CachePathFormat::V1)],
    previous: &[CacheVersion::new(1, CachePathFormat::V1)],
};

/// Objects Meta cache, with the following versions:
///
/// - `2`: Restructuring the cache directory format.
///
/// - `1`: Recomputation to use new `CacheKey` format.
///
/// - `0`: Initial version.
pub const META_CACHE_VERSIONS: CacheVersions = CacheVersions {
    current: CacheVersion::new(2, CachePathFormat::V2),
    fallbacks: &[CacheVersion::new(1, CachePathFormat::V1)],
    previous: &[CacheVersion::new(1, CachePathFormat::V1)],
};

/// Portable PDB cache, with the following versions:
///
/// - `4`: Restructuring the cache directory format.
///
/// - `3`: Skips hidden SequencePoints, and thus avoids outputting `lineno: 0`.
///
/// - `2`: Recomputation to use new `CacheKey` format.
///
/// - `1`: Initial version.
pub const PPDB_CACHE_VERSIONS: CacheVersions = CacheVersions {
    current: CacheVersion::new(4, CachePathFormat::V2),
    fallbacks: &[
        CacheVersion::new(3, CachePathFormat::V1),
        CacheVersion::new(2, CachePathFormat::V1),
    ],
    previous: &[
        CacheVersion::new(1, CachePathFormat::V1),
        CacheVersion::new(2, CachePathFormat::V1),
        CacheVersion::new(3, CachePathFormat::V1),
    ],
};

/// SourceMapCache, with the following versions:
///
/// - `2`: Restructuring the cache directory format.
///
/// - `1`: Initial version.
pub const SOURCEMAP_CACHE_VERSIONS: CacheVersions = CacheVersions {
    current: CacheVersion::new(2, CachePathFormat::V2),
    fallbacks: &[CacheVersion::new(1, CachePathFormat::V1)],
    previous: &[CacheVersion::new(1, CachePathFormat::V1)],
};

/// Il2cpp cache, with the following versions:
///
/// - `2`: Restructuring the cache directory format.
///
/// - `1`: Recomputation to use new `CacheKey` format.
///
/// - `0`: Initial version.
pub const IL2CPP_CACHE_VERSIONS: CacheVersions = CacheVersions {
    current: CacheVersion::new(2, CachePathFormat::V2),
    fallbacks: &[CacheVersion::new(1, CachePathFormat::V1)],
    previous: &[CacheVersion::new(1, CachePathFormat::V1)],
};

/// Bitcode / Auxdif (plist / bcsymbolmap) cache, with the following versions:
///
/// - `2`: Restructuring the cache directory format.
///
/// - `1`: Recomputation to use new `CacheKey` format.
///
/// - `0`: Initial version.
pub const BITCODE_CACHE_VERSIONS: CacheVersions = CacheVersions {
    current: CacheVersion::new(2, CachePathFormat::V2),
    fallbacks: &[CacheVersion::new(1, CachePathFormat::V1)],
    previous: &[CacheVersion::new(1, CachePathFormat::V1)],
};

/// Source Files Cache, with the following versions:
///
/// - `2`: Restructuring the cache directory format.
///
/// - `1`: Initial version.
pub const SOURCEFILES_CACHE_VERSIONS: CacheVersions = CacheVersions {
    current: CacheVersion::new(2, CachePathFormat::V2),
    fallbacks: &[CacheVersion::new(1, CachePathFormat::V1)],
    previous: &[CacheVersion::new(1, CachePathFormat::V1)],
};

/// Bundle Index Cache, with the following versions:
///
/// - `2`: Restructuring the cache directory format.
///
/// - `1`: Initial version.
pub const BUNDLE_INDEX_CACHE_VERSIONS: CacheVersions = CacheVersions {
    current: CacheVersion::new(2, CachePathFormat::V2),
    fallbacks: &[CacheVersion::new(1, CachePathFormat::V1)],
    previous: &[CacheVersion::new(1, CachePathFormat::V1)],
};

/// Proguard Cache, with the following versions:
///
/// - `6`: Information about whether a method has rewrite rules is now part
///   of the cache format.
///
/// - `5`: Information about whether a method is an outline/outlineCallsite is now part
///   of the cache format.
///
/// - `4`: Information about classes/methods being synthesized is now part
///   of the cache format.
///
/// - `3`: Restructuring the cache directory format.
///
/// - `2`: Use proguard cache format (<https://github.com/getsentry/symbolicator/pull/1491>).
///
/// - `1`: Initial version.
pub const PROGUARD_CACHE_VERSIONS: CacheVersions = CacheVersions {
    current: CacheVersion::new(6, CachePathFormat::V2),
    fallbacks: &[],
    previous: &[
        CacheVersion::new(1, CachePathFormat::V1),
        CacheVersion::new(2, CachePathFormat::V1),
        CacheVersion::new(3, CachePathFormat::V2),
        CacheVersion::new(4, CachePathFormat::V2),
        CacheVersion::new(5, CachePathFormat::V2),
    ],
};
static_assert!(proguard::PRGCACHE_VERSION == 4);

/// Symstore index cache, with the following versions:
///
/// - `1`: Initial version.
pub const SYMSTORE_INDEX_VERSIONS: CacheVersions = CacheVersions {
    current: CacheVersion::new(1, CachePathFormat::V2),
    fallbacks: &[],
    previous: &[],
};
