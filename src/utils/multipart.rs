use actix_web::{dev::Payload, error, multipart::Field, Error};
use bytes::{Bytes, BytesMut};
use futures::{compat::Stream01CompatExt, StreamExt};

use crate::types::SourceConfig;

const MAX_SOURCES_SIZE: usize = 1_000_000;

pub async fn read_multipart_data(field: Field<Payload>, max_size: usize) -> Result<Vec<u8>, Error> {
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

pub async fn read_multipart_file(field: Field<Payload>) -> Result<Bytes, Error> {
    let mut bytes = BytesMut::new();
    let mut stream = field.compat();

    while let Some(chunk) = stream.next().await {
        bytes.extend_from_slice(&chunk?);
    }

    Ok(bytes.freeze())
}

pub async fn read_multipart_sources(field: Field<Payload>) -> Result<Vec<SourceConfig>, Error> {
    let data = read_multipart_data(field, MAX_SOURCES_SIZE).await?;
    let sources = serde_json::from_slice(&data)?;
    Ok(sources)
}
