//! Helper utilities to handle HTTP multipart bodies.

use axum::extract::multipart::Field;
use axum::http::StatusCode;
use futures::prelude::*;
use tokio::fs::File;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

use super::ResponseError;

/// Stream a multipart body to a file.
///
/// The file's cursor will be set to the beginning of the file again.
pub async fn stream_multipart_file(
    mut field: Field<'_>,
    file: &mut File,
) -> Result<(), ResponseError> {
    // NOTE: we manually deal with chunks here for, as that should give us better errors
    while let Some(mut chunk) = field.chunk().await? {
        file.write_all_buf(&mut chunk).await?;
    }

    file.flush().await?;
    file.rewind().await?;
    Ok(())
}

/// Read a multipart body into memory.
///
/// This respects a maximum size.
pub async fn read_multipart_data(
    mut field: Field<'_>,
    max_size: usize,
) -> Result<Vec<u8>, ResponseError> {
    let mut data = Vec::with_capacity(512);

    while let Some(chunk) = field.next().await {
        let chunk = chunk?;
        if (data.len() + chunk.len()) > max_size {
            let err = anyhow::anyhow!(
                "Field {} larger than {} bytes",
                field.name().unwrap_or_default(),
                max_size
            );
            return Err((StatusCode::PAYLOAD_TOO_LARGE, err).into());
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}
