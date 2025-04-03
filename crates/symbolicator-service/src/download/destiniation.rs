use bytes::Bytes;
use std::{convert::Infallible, io, mem::ManuallyDrop};
use tokio::io::{AsyncWrite, AsyncWriteExt};

/// A simplified version of [`AsyncWrite`] which only supports
/// [`write_all`](AsyncWriteExt::write_all).
///
/// The interface here could be replaced with [`AsyncWrite`],
/// but currently Symbolicator only needs this minimized interface,
/// the full implementation of [`AsyncWrite`] is significantly harder
/// to implement and get right.
pub trait WriteStream {
    /// Attempts to write an entire buffer into this writer.
    ///
    /// See also: [`AsyncWriteExt::write_buf`].
    async fn write_buf(&mut self, buf: Bytes) -> io::Result<()>;

    async fn flush(&mut self) -> io::Result<()>;
}

impl<T> WriteStream for T
where
    T: AsyncWrite + Unpin,
{
    async fn write_buf(&mut self, mut buf: Bytes) -> io::Result<()> {
        AsyncWriteExt::write_buf(self, &mut buf).await.map(|_| ())
    }

    async fn flush(&mut self) -> io::Result<()> {
        AsyncWriteExt::flush(self).await
    }
}

/// A generic destination for downloads.
///
/// The destination can be simply converted into a [`AsyncWrite`],
/// but some destinations can choose to implement [`Destination::try_into_streams`],
/// to support concurrent partial downloads.
pub trait Destination: Sized {
    /// Attempts to convert this `Destination` into a destination which
    /// supports parallelized downloads through the [`MultiStreamDestination`] interface.
    ///
    /// Destinations which do not support parallelized downloads will return `Err` here.
    ///
    /// The specified `size` is the size of the fully downloaded file.
    /// Some underlying storages first need to resize to support concurrent writes.
    fn try_into_streams(self, size: u64) -> Result<impl MultiStreamDestination, Self>;

    /// Converts the destination into a generic [`AsyncWrite`].
    fn into_write(self) -> impl AsyncWrite;
}

/// A destination which supports multiple concurrent streams writing to it.
pub trait MultiStreamDestination {
    /// Type returned from [`MultiStreamDestination::stream`].
    type Stream: WriteStream;

    /// Creates a new stream starting at `offset`.
    ///
    /// Writing to the stream will write to the underlying destination with the specified offset.
    ///
    /// Multiple streams can be opened at once. The returned streams can be overlapping
    /// but the caller must take care not to write overlapping data.
    ///
    /// The `offset` specified here may not be validated against the `size` this multi-destination
    /// was opened with, requesting an offset past the maximum size of the destination may result
    /// in data corruption.
    ///
    /// The amount of bytes written to that stream must be specified accurately.
    /// An implementation is free to ignore these constraints and writing past the end of the
    /// returned stream may lead to data corruption.
    fn stream(&self, offset: u64, size: u64) -> Self::Stream;
}

impl MultiStreamDestination for Infallible {
    type Stream = Vec<u8>;

    fn stream(&self, _offset: u64, _len: u64) -> Self::Stream {
        match *self {}
    }
}

impl<'a> Destination for &'a mut tokio::fs::File {
    #[cfg(unix)]
    fn try_into_streams(self, _size: u64) -> Result<impl MultiStreamDestination, Self> {
        Ok(TokioFileMultiStreamDestination(&*self))
    }

    #[cfg(not(unix))]
    fn try_into_streams(self, _size: u64) -> Result<impl MultiStreamDestination, Self> {
        Err(self)
    }

    fn into_write(self) -> impl AsyncWrite {
        self
    }
}

/// Implements [`TokioFileMultiStreamDestination`] for a [`tokio::fs::File`].
#[cfg(unix)]
pub struct TokioFileMultiStreamDestination<'a>(&'a tokio::fs::File);

#[cfg(unix)]
impl<'a> MultiStreamDestination for TokioFileMultiStreamDestination<'a> {
    type Stream = OffsetFileWriteStream<'a>;

    fn stream(&self, offset: u64, _size: u64) -> Self::Stream {
        OffsetFileWriteStream {
            file: self.0,
            offset,
        }
    }
}

#[cfg(unix)]
pub struct OffsetFileWriteStream<'a> {
    file: &'a tokio::fs::File,
    offset: u64,
}

#[cfg(unix)]
impl WriteStream for OffsetFileWriteStream<'_> {
    async fn write_buf(&mut self, buf: Bytes) -> io::Result<()>
    where
        Self: Unpin,
    {
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::fs::FileExt;

        let offset = self.offset;
        let length = buf.len();

        // SAFETY:
        //
        // According to IO safety:
        // > To uphold I/O safety, it is crucial that no code acts on file descriptors it does not own or borrow,
        // > and no code closes file descriptors it does not own.
        //
        // The created file is immediately used and not used outside of this function. Since we do
        // own a reference to the original file it cannot be dropped (closed).
        //
        // Dropping the created `File`, would close the underlying file, the usage of `ManuallyDrop`.
        // satisfies part 2 of IO safety.
        //
        // IO safety: <https://doc.rust-lang.org/std/io/index.html#io-safety>
        let file = ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(self.file.as_raw_fd()) });

        tokio::task::spawn_blocking(move || file.write_all_at(&buf, offset)).await??;

        // Update the offset for the next write.
        //
        // There are no concurrent writes, because we have an exclusive reference here.
        self.offset += length as u64;

        Ok(())
    }

    async fn flush(&mut self) -> io::Result<()> {
        // Noop, it's just a partial stream.
        Ok(())
    }
}

/// Turns a [`AsyncWrite`] into a [`Destination`].
///
/// The resulting destination will not support concurrent streaming writes.
pub struct AsyncWriteDestination<T>(pub T);

#[expect(refining_impl_trait, reason = "type safety, access to inner type")]
impl<T> Destination for AsyncWriteDestination<T>
where
    T: AsyncWrite,
{
    fn try_into_streams(self, _size: u64) -> Result<Infallible, Self> {
        Err(self)
    }

    fn into_write(self) -> T {
        self.0
    }
}

impl<T> From<T> for AsyncWriteDestination<T> {
    fn from(value: T) -> Self {
        Self(value)
    }
}
