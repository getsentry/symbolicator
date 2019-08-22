use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use futures::future;

use crate::service::download::common::{
    prepare_download_paths, DownloadError, DownloadErrorKind, DownloadPath, DownloadedFile,
};
use crate::service::download::FileId;
use crate::types::{FileType, FilesystemSourceConfig, ObjectId};
use crate::utils::futures::SendFuture;

pub(super) struct FilesystemDownloader;

impl FilesystemDownloader {
    pub fn new() -> Self {
        Self
    }

    pub fn list_files(
        &self,
        source: Arc<FilesystemSourceConfig>,
        filetypes: &[FileType],
        object_id: &ObjectId,
    ) -> SendFuture<Vec<FileId>, DownloadError> {
        let ids = prepare_download_paths(
            object_id,
            filetypes,
            &source.files.filters,
            source.files.layout,
        )
        .map(|download_path| FileId::Filesystem(source.clone(), download_path))
        .collect();

        Box::new(future::ok(ids))
    }

    pub fn download(
        &self,
        source: Arc<FilesystemSourceConfig>,
        download_path: DownloadPath,
        _temp_dir: PathBuf,
    ) -> SendFuture<Option<DownloadedFile>, DownloadError> {
        let download_abspath = source.path.join(download_path);
        log::debug!("Fetching debug file from {:?}", download_abspath);

        let res = match File::open(download_abspath.clone()) {
            Ok(_) => DownloadedFile::local(download_abspath).map(Some),
            Err(e) => match e.kind() {
                io::ErrorKind::NotFound => Ok(None),
                _ => Err(DownloadError::from(DownloadErrorKind::Io)),
            },
        };

        Box::new(future::result(res))
    }
}
