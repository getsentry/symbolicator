use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use futures::future;

use crate::service::objects::common::{
    prepare_download_paths, DownloadPath, DownloadedFile, ObjectError, ObjectErrorKind,
};
use crate::service::objects::FileId;
use crate::types::{FileType, FilesystemSourceConfig, ObjectId};
use crate::utils::futures::ResultFuture;

pub(super) fn prepare_downloads(
    source: &Arc<FilesystemSourceConfig>,
    filetypes: &'static [FileType],
    object_id: &ObjectId,
) -> ResultFuture<Vec<FileId>, ObjectError> {
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

pub(super) fn download_from_source(
    _download_dir: PathBuf,
    source: Arc<FilesystemSourceConfig>,
    download_path: &DownloadPath,
) -> ResultFuture<Option<DownloadedFile>, ObjectError> {
    let download_abspath = source.path.join(download_path);
    log::debug!("Fetching debug file from {:?}", download_abspath);

    let res = match File::open(download_abspath.clone()) {
        Ok(_) => DownloadedFile::local(download_abspath).map(Some),
        Err(e) => match e.kind() {
            io::ErrorKind::NotFound => Ok(None),
            _ => Err(ObjectError::from(ObjectErrorKind::Io)),
        },
    };

    Box::new(future::result(res))
}
