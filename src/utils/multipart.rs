use std::fs::File;
use std::io::{Seek, SeekFrom, Write};

use actix::ResponseFuture;
use actix_web::{dev::Payload, error, multipart, Error};
use futures::{future, Future, IntoFuture, Stream};

use crate::types::SourceConfig;
use crate::utils::futures::ThreadPool;

const MAX_SOURCES_SIZE: usize = 1_000_000;

pub fn read_multipart_data(
    field: multipart::Field<Payload>,
    max_size: usize,
) -> ResponseFuture<Vec<u8>, Error> {
    let future =
        field
            .map_err(Error::from)
            .fold(Vec::with_capacity(512), move |mut body, chunk| {
                if (body.len() + chunk.len()) > max_size {
                    Err(error::ErrorBadRequest("payload too large"))
                } else {
                    body.extend_from_slice(&chunk);
                    Ok(body)
                }
            });

    Box::new(future)
}

pub fn read_multipart_file(
    field: multipart::Field<Payload>,
    threadpool: ThreadPool,
) -> ResponseFuture<File, Error> {
    let future = tempfile::tempfile()
        .into_future()
        .map_err(Error::from)
        .and_then(clone!(threadpool, |file| {
            field
                .map_err(Error::from)
                .fold(file, move |mut file, chunk| {
                    threadpool
                        .spawn_handle(future::lazy(move || file.write_all(&chunk).map(|_| file)))
                        .map_err(|e| Error::from(e.map_canceled(|| std::io::ErrorKind::Other)))
                })
        }))
        .and_then(clone!(threadpool, |mut file| {
            threadpool
                .spawn_handle(future::lazy(move || {
                    file.sync_all()?;
                    file.seek(SeekFrom::Start(0)).map(|_| file)
                }))
                .map_err(|e| Error::from(e.map_canceled(|| std::io::ErrorKind::Other)))
        }));

    Box::new(future)
}

pub fn read_multipart_sources(
    field: multipart::Field<Payload>,
) -> ResponseFuture<Vec<SourceConfig>, Error> {
    Box::new(
        read_multipart_data(field, MAX_SOURCES_SIZE)
            .and_then(|data| Ok(serde_json::from_slice(&data)?)),
    )
}
