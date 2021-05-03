use actix_web::{dev::Payload, error, multipart, Error};
use futures::{compat::Stream01CompatExt, StreamExt};

use crate::sources::SourceConfig;
use crate::types::RequestOptions;

const MAX_JSON_SIZE: usize = 1_000_000;

pub async fn read_multipart_data(
    field: multipart::Field<Payload>,
    max_size: usize,
) -> Result<Vec<u8>, Error> {
    let mut body = Vec::with_capacity(512);
    let mut stream = field.compat();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;

        if (body.len() + chunk.len()) > max_size {
            return Err(error::ErrorBadRequest("payload too large"));
        }

        body.extend_from_slice(&chunk);
    }

    Ok(body)
}

pub async fn read_multipart_file(field: multipart::Field<Payload>) -> Result<Vec<u8>, Error> {
    let mut body = Vec::with_capacity(512);
    let mut stream = field.compat();

    while let Some(chunk) = stream.next().await {
        body.extend_from_slice(&chunk?);
    }

    Ok(body)
}

pub async fn read_multipart_sources(
    field: multipart::Field<Payload>,
) -> Result<Vec<SourceConfig>, Error> {
    let data = read_multipart_data(field, MAX_JSON_SIZE).await?;
    Ok(serde_json::from_slice(&data)?)
}

pub async fn read_multipart_request_options(
    field: multipart::Field<Payload>,
) -> Result<RequestOptions, Error> {
    let data = read_multipart_data(field, MAX_JSON_SIZE).await?;
    Ok(serde_json::from_slice(&data)?)
}
