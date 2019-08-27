use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use crate::service::download::common::{
    prepare_download_paths, DownloadError, DownloadErrorKind, DownloadPath, DownloadedFile,
    ObjectDownloader,
};
use crate::types::{FileType, FilesystemSourceConfig, ObjectId};

pub struct FilesystemDownloader;

impl FilesystemDownloader {
    pub fn new() -> Self {
        Self
    }
}

impl ObjectDownloader for FilesystemDownloader {
    type Config = Arc<FilesystemSourceConfig>;
    type ListResponse = Result<Vec<DownloadPath>, DownloadError>;
    type DownloadResponse = Result<Option<DownloadedFile>, DownloadError>;

    fn list_files(
        &self,
        source: Self::Config,
        filetypes: &[FileType],
        object_id: &ObjectId,
    ) -> Self::ListResponse {
        let paths = prepare_download_paths(
            object_id,
            filetypes,
            &source.files.filters,
            source.files.layout,
        );

        Ok(paths.collect())
    }

    fn download(
        &self,
        source: Self::Config,
        download_path: DownloadPath,
        _temp_dir: PathBuf,
    ) -> Self::DownloadResponse {
        let download_abspath = source.path.join(download_path);
        log::debug!("Fetching debug file from {:?}", download_abspath);

        match File::open(download_abspath.clone()) {
            Ok(_) => DownloadedFile::local(download_abspath).map(Some),
            Err(e) => match e.kind() {
                io::ErrorKind::NotFound => Ok(None),
                _ => Err(DownloadError::from(DownloadErrorKind::Io)),
            },
        }
    }
}
