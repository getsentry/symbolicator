use std::io;
use std::path::Path;

use tempfile::NamedTempFile;

/// Creates a new [`NamedTempFile`] in the same directory as `file`.
pub fn tempfile_in_parent(file: &NamedTempFile) -> io::Result<NamedTempFile> {
    let dir = file
        .path()
        .parent()
        .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;

    tempfile(Some(dir))
}

/// Creates a new [`NamedTempFile`] in `tmp_dir`.
pub fn tempfile(tmp_dir: Option<&Path>) -> io::Result<NamedTempFile> {
    let Some(tmp_dir) = tmp_dir else {
        return NamedTempFile::new();
    };

    // The `cleanup` process could potentially remove the parent directories we are
    // operating in, so be defensive here and retry the fs operations.
    const MAX_RETRIES: usize = 2;
    let mut retries = 0;
    loop {
        retries += 1;

        if let Err(e) = std::fs::create_dir_all(tmp_dir) {
            sentry::with_scope(
                |scope| scope.set_extra("tmp_dir", tmp_dir.display().to_string().into()),
                || tracing::error!("Failed to create cache directory: {:?}", e),
            );
            if retries > MAX_RETRIES {
                return Err(e);
            }
            continue;
        }

        match tempfile::Builder::new().prefix("tmp").tempfile_in(tmp_dir) {
            Ok(temp_file) => return Ok(temp_file),
            Err(e) => {
                sentry::with_scope(
                    |scope| scope.set_extra("tmp_dir", tmp_dir.display().to_string().into()),
                    || tracing::error!("Failed to create cache file: {:?}", e),
                );
                if retries > MAX_RETRIES {
                    return Err(e);
                }
                continue;
            }
        }
    }
}
