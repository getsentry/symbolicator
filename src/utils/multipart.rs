use actix_web::{dev::Payload, error, multipart, Error};
use bytes::{Bytes, BytesMut};
use futures01::{Future, Stream};

use crate::sources::SourceConfig;
use crate::types::RequestOptions;

use super::futures::ResponseFuture;

const MAX_JSON_SIZE: usize = 1_000_000;

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
) -> impl Future<Item = Bytes, Error = Error> {
    field
        .map_err(Error::from)
        .fold(BytesMut::new(), |mut bytes, chunk| {
            bytes.extend_from_slice(&chunk);
            Ok::<BytesMut, Error>(bytes)
        })
        .and_then(|bytes| Ok(bytes.freeze()))
}

pub fn read_multipart_sources(
    field: multipart::Field<Payload>,
) -> ResponseFuture<Vec<SourceConfig>, Error> {
    Box::new(
        read_multipart_data(field, MAX_JSON_SIZE)
            .and_then(|data| Ok(serde_json::from_slice(&data)?)),
    )
}

pub fn read_multipart_request_options(
    field: multipart::Field<Payload>,
) -> ResponseFuture<RequestOptions, Error> {
    let fut = read_multipart_data(field, MAX_JSON_SIZE)
        .and_then(|data| Ok(serde_json::from_slice(&data)?));
    Box::new(fut)
}
