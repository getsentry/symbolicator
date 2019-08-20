use std::fs::File;
use std::io::{Seek, SeekFrom, Write};

use actix_multipart::Field;
use actix_web::{error, Error};
use futures::{future, Future, Stream};

use crate::types::SourceConfig;

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

pub fn read_multipart_file(field: Field) -> impl Future<Item = File, Error = Error> {
    future::result(tempfile::tempfile())
        .map_err(Error::from)
        .and_then(|file| {
            field
                .map_err(Error::from)
                .fold(file, |mut file, chunk| -> Result<File, Error> {
                    file.write_all(&chunk)?;
                    Ok(file)
                })
        })
        .and_then(|mut file| {
            file.sync_all()?;
            file.seek(SeekFrom::Start(0))?;
            Ok(file)
        })
}

pub fn read_multipart_sources(
    field: Field,
) -> impl Future<Item = Vec<SourceConfig>, Error = Error> {
    read_multipart_data(field, MAX_SOURCES_SIZE).and_then(|data| Ok(serde_json::from_slice(&data)?))
}
