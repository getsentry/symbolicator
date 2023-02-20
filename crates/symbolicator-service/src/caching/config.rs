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
    ArtifactCaches,
    SourceMapCaches,
    Diagnostics,
}

impl AsRef<str> for CacheName {
    fn as_ref(&self) -> &str {
        match self {
            Self::Objects => "objects",
            Self::ObjectMeta => "object_meta",
            Self::Auxdifs => "auxdifs",
            Self::Il2cpp => "il2cpp",
            Self::Symcaches => "symcaches",
            Self::Cficaches => "cficaches",
            Self::PpdbCaches => "ppdb_caches",
            Self::ArtifactCaches => "artifact_caches",
            Self::SourceMapCaches => "sourcesmap_caches",
            Self::Diagnostics => "diagnostics",
        }
    }
}

impl fmt::Display for CacheName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_ref())
    }
}
