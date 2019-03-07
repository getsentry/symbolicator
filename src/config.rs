use std::{
    fs::File,
    io,
    path::{Path, PathBuf},
};

use failure::Fail;

use serde_yaml;

#[derive(Fail, Debug, derive_more::From)]
pub enum ConfigError {
    #[fail(display = "Failed to open file: {}", _0)]
    Io(#[fail(cause)] io::Error),

    #[fail(display = "Failed to parse YAML: {}", _0)]
    Parsing(#[fail(cause)] serde_yaml::Error),
}

#[derive(serde::Deserialize)]
pub struct Config {
    pub cache_dir: Option<PathBuf>,
    pub bind: Option<String>,
}

pub fn get_config(path: Option<PathBuf>) -> Result<Config, ConfigError> {
    let path_ref: &Path = path
        .as_ref()
        .map(|x| x.as_path())
        .unwrap_or_else(|| "config".as_ref());
    let file = File::open(path_ref)?;
    Ok(serde_yaml::from_reader(file)?)
}
