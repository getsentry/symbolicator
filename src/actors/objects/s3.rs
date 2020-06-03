use std::sync::Arc;

use futures01::{future::IntoFuture, Future};

use crate::actors::objects::common::prepare_download_paths;
use crate::actors::objects::ObjectError;
use crate::sources::{FileType, S3SourceConfig, SourceFileId};
use crate::types::ObjectId;

pub(super) fn prepare_downloads(
    source: &Arc<S3SourceConfig>,
    filetypes: &'static [FileType],
    object_id: &ObjectId,
) -> Box<dyn Future<Item = Vec<SourceFileId>, Error = ObjectError>> {
    let ids = prepare_download_paths(
        object_id,
        filetypes,
        &source.files.filters,
        source.files.layout,
    )
    .map(|download_path| SourceFileId::S3(source.clone(), download_path))
    .collect();
    Box::new(Ok(ids).into_future())
}
