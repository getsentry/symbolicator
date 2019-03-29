use std::sync::Arc;

use actix::Addr;
use failure::Fail;
use futures::{future, Future, IntoFuture, Stream};
use rusoto_s3::S3;
use tokio_threadpool::ThreadPool;

use crate::actors::cache::{CacheActor, ComputeMemoized};
use crate::actors::objects::http::get_directory_path;
use crate::actors::objects::{
    DownloadStream, FetchFile, FileId, ObjectError, ObjectErrorKind, PrioritizedDownloads,
};
use crate::types::{ArcFail, FileType, ObjectId, S3SourceConfig, Scope, SourceConfig};

pub fn prepare_downloads(
    source: &S3SourceConfig,
    scope: Scope,
    filetypes: &'static [FileType],
    object_id: &ObjectId,
    threadpool: Arc<ThreadPool>,
    cache: Addr<CacheActor<FetchFile>>,
) -> Box<Future<Item = PrioritizedDownloads, Error = ObjectError>> {
    let mut requests = vec![];

    for &filetype in filetypes {
        requests.push(FetchFile {
            source: SourceConfig::S3(source.clone()),
            scope: scope.clone(),
            file_id: FileId::External {
                filetype,
                object_id: object_id.clone(),
            },
            threadpool: threadpool.clone(),
        });
    }

    Box::new(future::join_all(requests.into_iter().map(move |request| {
        cache
            .send(ComputeMemoized(request))
            .map_err(|e| e.context(ObjectErrorKind::Mailbox).into())
            .and_then(move |response| {
                Ok(response.map_err(|e| ArcFail(e).context(ObjectErrorKind::Caching).into()))
            })
    })))
}

pub fn download_from_source(
    source: &S3SourceConfig,
    file_id: &FileId,
) -> Box<Future<Item = Option<DownloadStream>, Error = ObjectError>> {
    let (object_id, filetype) = match file_id {
        FileId::External {
            object_id,
            filetype,
        } => (object_id, *filetype),
        _ => unreachable!(), // XXX(markus): fugly
    };

    if !source.filetypes.contains(&filetype) {
        return Box::new(Ok(None).into_future());
    }

    // XXX: Probably should send an error if the URL turns out to be invalid
    let key = match get_directory_path(source.layout, filetype, object_id) {
        Some(x) => format!("{}/{}", source.prefix.trim_end_matches(&['/'][..]), x),
        None => return Box::new(Ok(None).into_future()),
    };

    let provider = rusoto_credential::StaticProvider::new_minimal(
        source.access_key.clone(),
        source.secret_key.clone(),
    );
    let s3 = rusoto_s3::S3Client::new_with(
        rusoto_core::HttpClient::new().expect("failed to create request dispatcher"),
        provider,
        source.region.clone(),
    );

    let bucket = source.bucket.clone();
    let response = s3
        .get_object(rusoto_s3::GetObjectRequest {
            key: key.clone(),
            bucket: bucket.clone(),
            ..Default::default()
        })
        .then(move |result| match result {
            Ok(mut result) => {
                let body_read = match result.body.take() {
                    Some(body) => body.into_async_read(),
                    None => {
                        log::warn!("Empty response from s3:{}{}", bucket, key);
                        return Ok(None);
                    }
                };
                Ok(Some(Box::new(
                    tokio::codec::FramedRead::new(body_read, tokio::codec::BytesCodec::new())
                        .map(|value| value.freeze())
                        .map_err(|_err| ObjectError::from(ObjectErrorKind::Io)),
                )
                    as Box<dyn Stream<Item = _, Error = _>>))
            }
            Err(err) => {
                log::warn!("Skipping response from s3:{}{}: {}", bucket, key, err);
                Ok(None)
            }
        });

    Box::new(response)
}
