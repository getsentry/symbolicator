use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::sync::Arc;

use actix_multipart::Field;
use actix_web::{error, Error};
use futures::{future, Future, IntoFuture, Stream};
use sentry::Hub;
use tokio_threadpool::ThreadPool;

use crate::types::SourceConfig;
use crate::utils::sentry::SentryFutureExt;

const MAX_SOURCES_SIZE: usize = 1_000_000;

pub fn read_multipart_data(
    field: Field,
    max_size: usize,
) -> impl Future<Item = Vec<u8>, Error = Error> {
    field
        .map_err(Error::from)
        .fold(Vec::with_capacity(512), move |mut body, chunk| {
            if (body.len() + chunk.len()) > max_size {
                Err(error::ErrorBadRequest("payload too large"))
            } else {
                body.extend_from_slice(&chunk);
                Ok(body)
            }
        })
}

pub fn read_multipart_file(
    field: Field,
    threadpool: Arc<ThreadPool>,
) -> impl Future<Item = File, Error = Error> {
    tempfile::tempfile()
        .into_future()
        .map_err(Error::from)
        .and_then(clone!(threadpool, |file| {
            field
                .map_err(Error::from)
                .fold(file, move |mut file, chunk| {
                    threadpool
                        .spawn_handle(
                            future::lazy(move || -> std::io::Result<File> {
                                file.write_all(&chunk)?;
                                Ok(file)
                            })
                            .bind_hub(Hub::current()),
                        )
                        .map_err(Error::from)
                })
        }))
        .and_then(move |mut file| {
            threadpool
                .spawn_handle(
                    future::lazy(move || -> std::io::Result<File> {
                        file.sync_all()?;
                        file.seek(SeekFrom::Start(0))?;
                        Ok(file)
                    })
                    .bind_hub(Hub::current()),
                )
                .map_err(Error::from)
        })
}

pub fn read_multipart_sources(
    field: Field,
) -> impl Future<Item = Vec<SourceConfig>, Error = Error> {
    read_multipart_data(field, MAX_SOURCES_SIZE).and_then(|data| Ok(serde_json::from_slice(&data)?))
}
