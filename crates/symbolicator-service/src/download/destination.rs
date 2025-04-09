use bytes::Bytes;
use std::{
    convert::Infallible,
    future::Future,
    io::{self, Write},
    sync::Arc,
};
use tokio::io::{AsyncWrite, AsyncWriteExt};

/// A simplified version of [`AsyncWrite`] which only supports
/// [`write_all`](AsyncWriteExt::write_all).
///
/// The interface here could be replaced with [`AsyncWrite`],
/// but currently Symbolicator only needs this minimized interface,
/// the full implementation of [`AsyncWrite`] is significantly harder
/// to implement and get right.
pub trait WriteStream: Send {
    /// Attempts to write an entire buffer into this writer.
    ///
    /// See also: [`AsyncWriteExt::write_buf`].
    fn write_buf(&mut self, buf: Bytes) -> impl Future<Output = io::Result<()>> + Send;
}

impl<T> WriteStream for T
where
    T: AsyncWrite + Unpin + Send,
{
    async fn write_buf(&mut self, mut buf: Bytes) -> io::Result<()> {
        AsyncWriteExt::write_buf(self, &mut buf).await.map(|_| ())
    }
}

/// A generic destination for downloads.
///
/// The destination can be simply converted into a [`AsyncWrite`],
/// but some destinations can choose to implement [`Destination::try_into_streams`],
/// to support concurrent partial downloads.
pub trait Destination: Sized + Send {
    /// Type returned from [`Destination::try_into_streams`].
    type Streams: MultiStreamDestination;
    /// Type returned from [`Destination::into_write`].
    type Write: AsyncWrite + Send;

    /// Attempts to convert this `Destination` into a destination which
    /// supports parallelized downloads through the [`MultiStreamDestination`] interface.
    ///
    /// Destinations which do not support parallelized downloads will return `Err` here.
    ///
    fn try_into_streams(self) -> impl Future<Output = Result<Self::Streams, Self>> + Send;

    /// Converts the destination into a generic [`AsyncWrite`].
    fn into_write(self) -> Self::Write;
}

/// A destination which supports multiple concurrent streams writing to it.
pub trait MultiStreamDestination: Send {
    /// Type returned from [`MultiStreamDestination::stream`].
    type Stream<'a>: WriteStream
    where
        Self: 'a;
    /// Type returned from [`MultiStreamDestination::into_write`].
    type Write: AsyncWrite + Send;

    /// Configures the amount of bytes that will be written total.
    ///
    /// By contract a size must be set before acquiring any
    /// [streams](`MultiStreamDestination::stream`).
    ///
    /// The specified `size` is the size of the fully downloaded file.
    /// Some underlying storages first need to resize to support concurrent writes.
    fn set_size(&mut self, size: u64) -> impl Future<Output = io::Result<()>> + Send;

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
    fn stream(&self, offset: u64, size: u64) -> Self::Stream<'_>;

    /// Flushes this output stream, ensuring that all intermediately buffered contents reach their destination.
    fn flush(&mut self) -> impl Future<Output = io::Result<()>> + Send;

    /// Converts the destination into a generic [`AsyncWrite`].
    fn into_write(self) -> Self::Write;
}

impl MultiStreamDestination for Infallible {
    type Stream<'a>
        = Vec<u8>
    where
        Self: 'a;
    type Write = Vec<u8>;

    async fn set_size(&mut self, _size: u64) -> io::Result<()> {
        match *self {}
    }

    fn stream(&self, _offset: u64, _len: u64) -> Self::Stream<'_> {
        match *self {}
    }

    async fn flush(&mut self) -> io::Result<()> {
        match *self {}
    }

    fn into_write(self) -> Self::Write {
        match self {}
    }
}

impl<'a> Destination for &'a mut tokio::fs::File {
    #[cfg(unix)]
    type Streams = FileMultiStreamDestination<'a>;
    #[cfg(not(unix))]
    type Streams = Infallible;
    type Write = Self;

    #[cfg(unix)]
    async fn try_into_streams(self) -> Result<Self::Streams, Self> {
        let dup = match self.try_clone().await {
            Ok(file) => file.into_std().await,
            Err(_) => return Err(self),
        };

        Ok(FileMultiStreamDestination {
            original: self,
            std: Arc::new(dup),
        })
    }

    #[cfg(not(unix))]
    async fn try_into_streams(self, _size: u64) -> Result<Self::Streams, Self> {
        Err(self)
    }

    fn into_write(self) -> Self::Write {
        self
    }
}

/// File based multi stream destination.
pub struct FileMultiStreamDestination<'a> {
    original: &'a mut tokio::fs::File,
    std: Arc<std::fs::File>,
}

#[cfg(unix)]
impl<'file> MultiStreamDestination for FileMultiStreamDestination<'file> {
    type Stream<'a>
        = OffsetFileWriteStream
    where
        Self: 'a;
    type Write = &'file mut tokio::fs::File;

    async fn set_size(&mut self, size: u64) -> io::Result<()> {
        // While not strictly necessary for the implementation using `pwrite`,
        // we can already resize the file to prevent sparse files.
        self.original.set_len(size).await
    }

    fn stream(&self, offset: u64, size: u64) -> Self::Stream<'_> {
        OffsetFileWriteStream {
            file: Arc::clone(&self.std),
            offset,
            end: offset + size + 1,
        }
    }
    async fn flush(&mut self) -> io::Result<()> {
        let mut file = Arc::clone(&self.std);
        tokio::task::spawn_blocking(move || file.flush()).await?
    }

    fn into_write(self) -> Self::Write {
        self.original
    }
}

/// A [`WriteStream`] for a file with an arbitrary offset.
#[cfg(unix)]
pub struct OffsetFileWriteStream {
    file: Arc<std::fs::File>,
    offset: u64,
    end: u64,
}

#[cfg(unix)]
impl WriteStream for OffsetFileWriteStream {
    async fn write_buf(&mut self, buf: Bytes) -> io::Result<()>
    where
        Self: Unpin,
    {
        use std::os::unix::fs::FileExt;

        let offset = self.offset;
        let length = buf.len() as u64;

        debug_assert!(
            offset + length < self.end,
            "attempt to write past end of stream"
        );

        let file = Arc::clone(&self.file);
        tokio::task::spawn_blocking(move || file.write_all_at(&buf, offset)).await??;

        // Update the offset for the next write.
        //
        // There are no concurrent writes, because we have an exclusive reference here.
        self.offset += length;

        Ok(())
    }
}

/// Turns a [`AsyncWrite`] into a [`Destination`].
///
/// The resulting destination will not support concurrent streaming writes.
pub struct AsyncWriteDestination<T>(pub T);

impl<T> Destination for AsyncWriteDestination<T>
where
    T: AsyncWrite + Send,
{
    type Streams = Infallible;
    type Write = T;

    async fn try_into_streams(self) -> Result<Self::Streams, Self> {
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

#[cfg(test)]
mod tests {
    use futures::{stream::FuturesUnordered, StreamExt as _};
    use tokio::io::AsyncReadExt;

    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn test_partial_streams_unix() {
        let temp_file = tempfile::NamedTempFile::new().unwrap();
        let mut tokio_file = tokio::fs::File::options()
            .write(true)
            .read(true)
            .open(temp_file.path())
            .await
            .unwrap();

        let destination = &mut tokio_file;

        let mut streams = destination.try_into_streams().await.unwrap();
        streams.set_size(10).await.unwrap();

        let mut aa = streams.stream(0, 2);
        let mut bb = streams.stream(2, 4);
        let mut cc = streams.stream(4, 6);
        let mut dd = streams.stream(6, 8);
        let mut ee = streams.stream(8, 10);

        let mut futures = FuturesUnordered::new();
        futures.push(aa.write_buf(Bytes::from("aa")));
        futures.push(cc.write_buf(Bytes::from("cc")));
        futures.push(bb.write_buf(Bytes::from("bb")));
        futures.push(ee.write_buf(Bytes::from("ee")));
        futures.push(dd.write_buf(Bytes::from("dd")));
        while futures.next().await.transpose().unwrap().is_some() {}

        streams.flush().await.unwrap();

        // The original file handle is still usable and can read the written contents.
        let mut contents = String::new();
        tokio_file.read_to_string(&mut contents).await.unwrap();
        assert_eq!(&contents, "aabbccddee");

        // A new file can also read the contents.
        let contents = std::fs::read_to_string(temp_file.path()).unwrap();
        assert_eq!(&contents, "aabbccddee");
    }
}
