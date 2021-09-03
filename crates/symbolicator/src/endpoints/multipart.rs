//! Helper utilities to handle HTTP multipart bodies.

use std::io::SeekFrom;

use axum::extract::multipart::Field;
use axum::http::StatusCode;
use bytes::Bytes;
use futures::prelude::*;
use tokio::fs::File;
use tokio::io::AsyncSeekExt;
use tokio_util::io::StreamReader;

use super::ResponseError;

/// Newtype around axum's multipart [`Field`].
///
/// This is required because we need to have a [`Stream`] impl which has an `Item =
/// Result<T, std::io::Error>` in order to be able to use it with tokio-util's
/// [`StreamReader`].  The original [`Field`]'s [`Stream`] impl does have the wrong error
/// type so we wrap it here.
struct MultipartField<'a> {
    inner: axum::extract::multipart::Field<'a>,
}

impl<'a> MultipartField<'a> {
    fn new(inner: axum::extract::multipart::Field<'a>) -> Self {
        Self { inner }
    }
}

impl<'a> Stream for MultipartField<'a> {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = &mut *self;
        let inner = std::pin::Pin::new(&mut this.inner);
        inner
            .poll_next(cx)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err))
    }
}

/// Stream a multipart body to a file.
///
/// The file's cursor will be set to the beginning of the file again.
pub async fn stream_multipart_file(field: Field<'_>, file: &mut File) -> Result<(), ResponseError> {
    let mut field_reader = StreamReader::new(MultipartField::new(field));
    tokio::io::copy(&mut field_reader, file).await?;
    file.seek(SeekFrom::Start(0)).await?;
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
