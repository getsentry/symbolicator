use lazy_static::lazy_static;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

lazy_static! {
    static ref CONFIG: Mutex<Arc<RunConfig>> = Mutex::new(Arc::new(Default::default()));
}

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
    pub fn get() -> Arc<RunConfig> {
        CONFIG.lock().unwrap().clone()
    }

    pub fn configure<F: FnOnce(&mut Self) -> R, R>(f: F) -> R {
        let mut config = RunConfig::get();
        let rv = {
            let mutable_config = Arc::make_mut(&mut config);
            f(mutable_config)
        };
        *CONFIG.lock().unwrap() = config;
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
    pub compression_level: usize,
}
