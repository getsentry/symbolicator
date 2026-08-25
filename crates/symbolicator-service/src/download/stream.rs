use std::io::{self, Seek as _};
use std::pin::Pin;
use std::task::{Context, Poll};

use tempfile::NamedTempFile;
use tokio::io::{AsyncRead, ReadBuf};

use crate::{
    caching::{CacheContents, CacheError},
    download::{DownloadLimits, compression::Compression},
};

/// A stream returned from [`crate::download::DownloadService::download`].
///
/// The stream can be backed by various different types depending on the file being downloaded.
pub struct Download {
    inner: NamedTempFile,
    compression: Compression,
    limits: DownloadLimits,
}

impl Download {
    /// Creates a new [`Self`].
    ///
    /// The passed temp file must contain the download contents and be rewound to the beginning of
    /// the file.
    ///
    /// On access `compression` is used to transparently decompress the contents for the caller,
    /// `limits` are enforced during the decompression.
    pub fn new(mut f: NamedTempFile, compression: Compression, limits: DownloadLimits) -> Self {
        // Just to make sure the file is rewound to position `0`, so we can safely read from it
        // and return it to callers.
        debug_assert_eq!(f.stream_position().unwrap_or(0), 0);

        Self {
            inner: f,
            compression,
            limits,
        }
    }

    /// Materializes the download into the provided `file`.
    ///
    /// The passed mutable reference to the file might be swapped with a different file.
    pub async fn materialize_into(mut self, file: &mut NamedTempFile) -> CacheContents {
        // If the file here has no compression applied, we can just swap the temp file.
        if self.compression == Compression::Identity {
            std::mem::swap(&mut self.inner, file);
            return Ok(());
        }

        let mut destination = tokio::fs::File::from_std(file.reopen()?);
        // The file is at position zero, through the re-open, but it is possible that the file
        // already contains some leftover data from a previous download. We truncate the file
        // to make sure only these new contents are stored in it.
        destination.set_len(0).await?;
        let mut source = self.into_read();

        tokio::io::copy(&mut source, &mut destination).await?;

        Ok(())
    }

    /// Materializes the download into a [`NamedTempFile`].
    ///
    /// This is very similar to [`Self::materialize_into`], but slightly more convenient
    /// and efficient as not always a new temp file must be created.
    pub async fn materialize(self) -> CacheContents<NamedTempFile> {
        if self.compression == Compression::Identity {
            return Ok(self.inner);
        }

        let mut file = NamedTempFile::new()?;
        self.materialize_into(&mut file).await?;
        Ok(file)
    }

    /// Returns the downloaded contents as an [`AsyncRead`].
    pub fn into_read(self) -> impl AsyncRead + Unpin {
        let source = tokio::fs::File::from_std(self.inner.into_file());
        let source = tokio::io::BufReader::new(source);
        let source = self.compression.decompress(source);
        let source = LimitRead::new(source, self.limits.max_download_size);
        MapError::new(source, |error| {
            // Wrap all errors in a download error, so that later conversions from io error to cache
            // error pick up the inner error as a download error instead of internal error.
            io::Error::other(CacheError::download_error(&error))
        })
    }
}

/// A reader which enforces a maximum for amount of bytes read.
struct LimitRead<R> {
    /// The inner `AsyncRead`.
    inner: R,
    /// The maximum amount of bytes allowed to read.
    limit: u64,
    /// The amount of bytes read so far.
    bytes_read: u64,
}

impl<R> LimitRead<R> {
    fn new(inner: R, limit: Option<u64>) -> Self {
        Self {
            inner,
            limit: limit.unwrap_or(u64::MAX),
            bytes_read: 0,
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for LimitRead<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        if this.bytes_read > this.limit {
            return Poll::Ready(Err(io::Error::other(CacheError::size_exceeded())));
        }
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }

        let start = buf.filled().len();
        std::task::ready!(Pin::new(&mut this.inner).poll_read(cx, buf))?;

        let n = buf.filled().len() - start;
        this.bytes_read += n as u64;

        if this.bytes_read > this.limit {
            // For our use-case this isn't technically necessary, since we only want to bound compression
            // we could safely overfill the buffer a bit and then fail on the next read.
            //
            // But for the sake of testability and correctness, let's handle the case where the last
            // read went over the limit.
            //
            // The problem is, we can't just error here, as `AsyncRead`'s contract states no bytes
            // must be read on error.

            // Amount of bytes, which were read too much.
            let over = this.bytes_read - this.limit;
            // Rewind the buffer to the actual max which is allowed.
            buf.set_filled(buf.filled().len() - over as usize);

            // We rewound back all bytes that were just read, we can error as we filled no bytes.
            if buf.filled().len() == start {
                return Poll::Ready(Err(io::Error::other(CacheError::size_exceeded())));
            }

            // Now we did write some bytes, `bytes_read > this.limit` is still true, which means the
            // next read will fail.
        }

        Poll::Ready(Ok(()))
    }
}

pin_project_lite::pin_project! {
    struct MapError<S, F> {
        #[pin]
        inner: S,
        map: F,
    }
}

impl<S, F> MapError<S, F> {
    fn new(inner: S, map: F) -> Self {
        Self { inner, map }
    }
}

impl<S, F> AsyncRead for MapError<S, F>
where
    S: AsyncRead,
    F: Fn(io::Error) -> io::Error,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.project();
        match this.inner.poll_read(cx, buf) {
            Poll::Ready(res) => Poll::Ready(res.map_err(|err| (this.map)(err))),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use flate2::write::GzEncoder;
    use tokio::io::AsyncReadExt;

    use super::*;

    #[tokio::test]
    async fn test_limit_read_without_limit() {
        let mut reader = LimitRead::new(&b"hello world"[..], None);
        let mut output = Vec::new();

        reader.read_to_end(&mut output).await.unwrap();

        assert_eq!(output, b"hello world");
    }

    #[tokio::test]
    async fn test_limit_read_within_limit() {
        for (input, limit) in [(&b""[..], 0), (&b"hello"[..], 5), (&b"hello"[..], 6)] {
            let mut reader = LimitRead::new(input, Some(limit));
            let mut output = Vec::new();

            reader.read_to_end(&mut output).await.unwrap();

            assert_eq!(output, input);
        }
    }

    #[tokio::test]
    async fn test_limit_read_exceeded() {
        let mut reader = LimitRead::new(&b"hello world"[..], Some(5));
        let mut output = Vec::new();
        let mut buffer = [0; 3];

        let bytes_read = reader.read(&mut buffer).await.unwrap();
        output.extend_from_slice(&buffer[..bytes_read]);

        let bytes_read = reader.read(&mut buffer).await.unwrap();
        output.extend_from_slice(&buffer[..bytes_read]);

        let error = reader.read(&mut buffer).await.unwrap_err();

        assert_eq!(output, b"hello");
        assert_eq!(
            CacheError::from(error),
            CacheError::Malformed("Maximum file size exceeded".to_owned())
        );
    }

    #[tokio::test]
    async fn test_limit_read_zero_limit() {
        let mut reader = LimitRead::new(&b"hello"[..], Some(0));
        let mut output = Vec::new();

        let error = reader.read_to_end(&mut output).await.unwrap_err();

        assert!(output.is_empty());
        assert_eq!(
            CacheError::from(error),
            CacheError::Malformed("Maximum file size exceeded".to_owned())
        );
    }

    #[tokio::test]
    async fn test_materialize_into_truncates_destination() {
        let expected = b"hello";

        let mut compressed = NamedTempFile::new().unwrap();
        let mut encoder = GzEncoder::new(compressed.as_file_mut(), flate2::Compression::default());
        encoder.write_all(expected).unwrap();
        encoder.finish().unwrap();
        compressed.as_file_mut().rewind().unwrap();

        let download = Download::new(compressed, Compression::Gzip, DownloadLimits::default());
        let mut destination = NamedTempFile::new().unwrap();
        destination.write_all(b"stale contents").unwrap();

        download.materialize_into(&mut destination).await.unwrap();

        assert_eq!(std::fs::read(destination.path()).unwrap(), expected);
    }

    #[tokio::test]
    async fn test_materialize_enforces_size_limit() {
        let input = b"hello world";

        let mut compressed = NamedTempFile::new().unwrap();
        let mut encoder = GzEncoder::new(compressed.as_file_mut(), flate2::Compression::default());
        encoder.write_all(input).unwrap();
        encoder.finish().unwrap();
        compressed.as_file_mut().rewind().unwrap();

        let download = Download::new(
            compressed,
            Compression::Gzip,
            DownloadLimits {
                max_download_size: Some(5),
                ..DownloadLimits::default()
            },
        );

        let error = download.materialize().await.unwrap_err();
        assert_eq!(
            error,
            CacheError::Malformed("Maximum file size exceeded".to_owned())
        );
    }
}
