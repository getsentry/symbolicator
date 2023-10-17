use std::path::PathBuf;
use std::sync::OnceLock;

static CONFIG: OnceLock<RunConfig> = OnceLock::new();

#[derive(PartialEq, Eq, PartialOrd, Ord, Debug, Default, Clone)]
pub struct RunConfig {
    /// Output location for this task.
    pub output: PathBuf,

    /// Ignore broken archives.
    pub ignore_errors: bool,

    /// If enabled output will be suppressed
    pub quiet: bool,
}

impl RunConfig {
    pub fn get() -> &'static RunConfig {
        CONFIG.get_or_init(RunConfig::default)
    }

    pub fn configure<F: FnOnce(&mut Self) -> R, R>(f: F) -> R {
        let mut config = RunConfig::default();
        let rv = f(&mut config);
        CONFIG.set(config).unwrap();
        rv
    }
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct SortConfig {
    /// The bundle ID of this task.
    pub bundle_id: Option<String>,

    /// If enable the system will attempt to create source bundles
    pub with_sources: bool,

    /// If enabled debug symbols will be zstd compressed
    /// (repeat to increase compression)
    pub compression_level: u8,
}
