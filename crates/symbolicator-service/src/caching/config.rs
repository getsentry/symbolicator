use std::fmt;

/// All known cache names.
#[derive(Debug, Clone, Copy)]
pub enum CacheName {
    Objects,
    ObjectMeta,
    Auxdifs,
    Il2cpp,
    Symcaches,
    Cficaches,
    PpdbCaches,
    SourceMapCaches,
    SourceFiles,
    Diagnostics,
    Proguard,
    SourceIndex,
    /// Mirror of upstream-compressed object bytes (CAB/gzip/zstd/...), tee'd during download
    /// so the `/proxy` endpoint can serve `.pd_`/`.dl_`/`.ex_` byte-identically when the
    /// upstream source delivered a compressed payload.
    RawCompressed,
    /// CAB (MSZIP) envelopes synthesized from cached decompressed objects, used as the
    /// fallback for compressed-proxy responses when no upstream raw copy is available.
    CabSynth,
}

impl CacheName {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Objects => "objects",
            Self::ObjectMeta => "object_meta",
            Self::Auxdifs => "auxdifs",
            Self::Il2cpp => "il2cpp",
            Self::Symcaches => "symcaches",
            Self::Cficaches => "cficaches",
            Self::PpdbCaches => "ppdb_caches",
            Self::SourceMapCaches => "sourcemap_caches",
            Self::SourceFiles => "sourcefiles",
            Self::Diagnostics => "diagnostics",
            Self::Proguard => "proguard",
            Self::SourceIndex => "source_index",
            Self::RawCompressed => "raw_compressed",
            Self::CabSynth => "cab_synth",
        }
    }
}

impl fmt::Display for CacheName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}
