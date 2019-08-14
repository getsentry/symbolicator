use std::fs::File;
use std::io;
use std::sync::Arc;

use futures::{future, Future};

use crate::service::objects::common::{prepare_download_paths, DownloadPath};
use crate::service::objects::{DownloadStream, FileId, ObjectError, ObjectErrorKind};
use crate::types::{FileType, FilesystemSourceConfig, ObjectId};

pub(super) fn prepare_downloads(
    source: &Arc<FilesystemSourceConfig>,
    filetypes: &'static [FileType],
    object_id: &ObjectId,
) -> Box<dyn Future<Item = Vec<FileId>, Error = ObjectError>> {
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
    source: Arc<FilesystemSourceConfig>,
    download_path: &DownloadPath,
) -> Box<dyn Future<Item = Option<DownloadStream>, Error = ObjectError>> {
    let download_abspath = source.path.join(download_path);
    log::debug!("Fetching debug file from {:?}", download_abspath);

    let res = match File::open(download_abspath.clone()) {
        Ok(_) => Ok(Some(DownloadStream::File(download_abspath))),
        Err(e) => match e.kind() {
            io::ErrorKind::NotFound => Ok(None),
            _ => Err(ObjectError::from(ObjectErrorKind::Io)),
        },
    };

    Box::new(future::result(res))
}
