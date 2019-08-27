use actix_multipart::Field;
use actix_web::web::{Bytes, BytesMut};
use actix_web::{error, Error};
use futures::{Future, Stream};

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

pub fn read_multipart_file(field: Field) -> impl Future<Item = Bytes, Error = Error> {
    field
        .map_err(Error::from)
        .fold(
            BytesMut::new(),
            |mut bytes, chunk| -> Result<BytesMut, Error> {
                bytes.extend_from_slice(&chunk);
                Ok(bytes)
            },
        )
        .and_then(|bytes| Ok(bytes.freeze()))
}

pub fn read_multipart_sources(
    field: Field,
) -> impl Future<Item = Vec<SourceConfig>, Error = Error> {
    read_multipart_data(field, MAX_SOURCES_SIZE).and_then(|data| Ok(serde_json::from_slice(&data)?))
}
