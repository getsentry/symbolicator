use std::io::{self, Read, Seek};

use cab::Cabinet;
use flate2::read::{MultiGzDecoder, ZlibDecoder};
use reqwest::header::HeaderMap;
use tempfile::NamedTempFile;
use tokio::io::{AsyncBufRead, AsyncRead};

use crate::caching::CacheError;

/// Decompresses a downloaded file.
///
/// The passed [`NamedTempFile`] might be swapped with a fresh one in case decompression happens.
/// That new temp file will be created in the same directory as the original one.
pub fn maybe_decompress_file(
    src: &mut NamedTempFile,
    max_uncompressed_size: u64,
) -> io::Result<()> {
    // Ensure that both meta data and file contents are available to the
    // subsequent reads of the file metadata and reads from other threads.
    let mut file = src.as_file();
    file.sync_all()?;

    let metadata = file.metadata()?;
    // TODO(swatinem): we should rename this to a more descriptive metric, as we use this for *all*
    // kinds of downloaded files, not only "objects".
    metric!(distribution("objects.size") = metadata.len() as f64);

    file.rewind()?;
    if metadata.len() < 4 {
        // we don’t want to error for empty files here
        return Ok(());
    }

    let mut magic_bytes: [u8; 4] = [0, 0, 0, 0];
    file.read_exact(&mut magic_bytes)?;
    file.rewind()?;

    // Read one more byte than we actually want so we can detect
    // the size being exceeded.
    let read_limit = max_uncompressed_size.saturating_add(1);

    // For a comprehensive list also refer to
    // https://en.wikipedia.org/wiki/List_of_file_signatures
    //
    // XXX: The decoders in the flate2 crate also support being used as a
    // wrapper around a Write. Only zstd doesn't. If we can get this into
    // zstd we could save one tempfile and especially avoid the io::copy
    // for downloads that were not compressed.
    match magic_bytes {
        // Magic bytes for zstd
        // https://tools.ietf.org/id/draft-kucherawy-dispatch-zstd-00.html#rfc.section.2.1.1
        [0x28, 0xb5, 0x2f, 0xfd] => {
            metric!(counter("compression") += 1, "type" => "zstd");

            let mut dst = crate::utils::fs::tempfile_in_parent(src)?;
            let mut reader = zstd::stream::Decoder::new(file)?.take(read_limit);
            io::copy(&mut reader, &mut dst)?;

            std::mem::swap(src, &mut dst);
        }
        // Magic bytes for gzip
        // https://tools.ietf.org/html/rfc1952#section-2.3.1
        [0x1f, 0x8b, _, _] => {
            metric!(counter("compression") += 1, "type" => "gz");

            // We assume MultiGzDecoder accepts a strict superset of input
            // values compared to GzDecoder.
            let mut dst = crate::utils::fs::tempfile_in_parent(src)?;
            let mut reader = MultiGzDecoder::new(file).take(read_limit);
            io::copy(&mut reader, &mut dst)?;

            std::mem::swap(src, &mut dst);
        }
        // Magic bytes for zlib
        [0x78, 0x01, _, _] | [0x78, 0x9c, _, _] | [0x78, 0xda, _, _] => {
            metric!(counter("compression") += 1, "type" => "zlib");

            let mut dst = crate::utils::fs::tempfile_in_parent(src)?;
            let mut reader = ZlibDecoder::new(file).take(read_limit);
            io::copy(&mut reader, &mut dst)?;

            std::mem::swap(src, &mut dst);
        }
        // Magic bytes for ZipArchive
        // https://pkware.cachefly.net/webdocs/APPNOTE/APPNOTE-6.3.10.TXT
        // Section: 4.3.7  Local file header
        [0x50, 0x4b, 0x03, 0x04] => {
            metric!(counter("compression") += 1, "type" => "zip");

            let mut dst = {
                let mut archive = zip::ZipArchive::new(file)?;

                if archive.len() != 1 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "ZipArchive contains more than one symbol file",
                    ));
                }

                let mut dst = crate::utils::fs::tempfile_in_parent(src)?;
                let mut reader = archive.by_index(0)?.take(read_limit);
                io::copy(&mut reader, &mut dst)?;

                dst
            };

            std::mem::swap(src, &mut dst);
        }
        // Magic bytes for CAB
        [77, 83, 67, 70] => {
            metric!(counter("compression") += 1, "type" => "cab");

            let mut cab_file = Cabinet::new(file)?;

            let mut contained_files = cab_file
                .folder_entries()
                .flat_map(|folder| folder.file_entries())
                .map(|file| file.name());

            let first_file = contained_files
                .next()
                .map(String::from)
                .ok_or(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "cab file is empty",
                ))?;

            if contained_files.next().is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "cab file contains more than 1 symbol file",
                ));
            }

            let mut dst = crate::utils::fs::tempfile_in_parent(src)?;
            let mut reader = cab_file.read_file(&first_file)?.take(read_limit);
            std::io::copy(&mut reader, &mut dst)?;

            std::mem::swap(src, &mut dst);
        }
        // Probably not compressed
        _ => {
            metric!(counter("compression") += 1, "type" => "none");
        }
    }

    if src.as_file().metadata()?.len() > max_uncompressed_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Maximum file size exceeded",
        ));
    }

    Ok(())
}

/// Different compressions supported by Symbolicator.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Compression {
    /// The identity compression.
    ///
    /// Also known as no compression.
    Identity,
    /// Brotli compression.
    Brotli,
    /// Deflate compression.
    Deflate,
    /// GZIP compression.
    Gzip,
    /// Zstd compression.
    Zstd,
}

impl Compression {
    /// Parses the [`Compression`] from [HTTP headers](`HeaderMap`).
    pub fn from_headers(headers: &HeaderMap) -> Result<Self, CompressionError> {
        let content_encodings = headers.get_all(reqwest::header::CONTENT_ENCODING);
        let mut content_encodings = content_encodings.iter();

        let Some(content_encoding) = content_encodings.next() else {
            return Ok(Self::Identity);
        };

        if content_encodings.next().is_some() {
            return Err(CompressionError("Multiple content encodings not supported"));
        }

        Ok(match content_encoding.as_bytes().trim_ascii() {
            b"" | b"identity" => Self::Identity,
            b"br" => Self::Brotli,
            b"gzip" | b"x-gzip" => Self::Gzip,
            b"deflate" => Self::Deflate,
            b"zstd" => Self::Zstd,
            _ => {
                return Err(CompressionError("Unsupported content encoding"));
            }
        })
    }

    /// Returns a `Accept-Encoding` compatible header of all supported compressions.
    pub fn accept_encoding() -> reqwest::header::HeaderValue {
        const ENCODINGS: &str = "br, deflate, gzip, zstd";
        reqwest::header::HeaderValue::from_static(ENCODINGS)
    }

    /// Returns a [`Decoder`] which decompresses `r`.
    pub fn decompress<R>(self, r: R) -> Decoder<R>
    where
        R: AsyncBufRead,
    {
        match self {
            Self::Identity => Decoder {
                inner: DecoderInner::Identity { r },
            },
            Self::Brotli => Decoder {
                inner: DecoderInner::Brotli {
                    r: async_compression::tokio::bufread::BrotliDecoder::new(r),
                },
            },
            Self::Deflate => Decoder {
                inner: DecoderInner::Deflate {
                    r: async_compression::tokio::bufread::ZlibDecoder::new(r),
                },
            },
            Self::Gzip => Decoder {
                inner: DecoderInner::Gzip {
                    r: async_compression::tokio::bufread::GzipDecoder::new(r),
                },
            },
            Self::Zstd => Decoder {
                inner: DecoderInner::Zstd {
                    r: async_compression::tokio::bufread::ZstdDecoder::new(r),
                },
            },
        }
    }
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct CompressionError(&'static str);

impl From<CompressionError> for CacheError {
    fn from(error: CompressionError) -> Self {
        Self::Malformed(error.0.to_owned())
    }
}

pin_project_lite::pin_project! {
    pub struct Decoder<R> { #[pin] inner: DecoderInner<R> }
}

impl<R> AsyncRead for Decoder<R>
where
    R: AsyncBufRead,
{
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match self.project().inner.project() {
            DecoderInnerProj::Identity { r } => r.poll_read(cx, buf),
            DecoderInnerProj::Brotli { r } => r.poll_read(cx, buf),
            DecoderInnerProj::Deflate { r } => r.poll_read(cx, buf),
            DecoderInnerProj::Gzip { r } => r.poll_read(cx, buf),
            DecoderInnerProj::Zstd { r } => r.poll_read(cx, buf),
        }
    }
}

pin_project_lite::pin_project! {
    #[project = DecoderInnerProj]
    enum DecoderInner<R> {
        Identity { #[pin] r: R },
        Brotli{ #[pin] r: async_compression::tokio::bufread::BrotliDecoder<R> },
        // In HTTP 1.1 `Content-Encoding: deflate` refers to `DEFLATE` compression,
        // wrapped in the zlip data format.
        //
        // See: <https://github.com/seanmonstar/reqwest/pull/1257>.
        Deflate{ #[pin] r: async_compression::tokio::bufread::ZlibDecoder<R> },
        Gzip{ #[pin] r: async_compression::tokio::bufread::GzipDecoder<R> },
        Zstd{ #[pin] r: async_compression::tokio::bufread::ZstdDecoder<R> },
    }
}

#[cfg(test)]
mod tests {
    use reqwest::header::{CONTENT_ENCODING, HeaderValue};
    use tokio::io::{AsyncReadExt, BufReader};

    use super::*;

    #[test]
    fn test_from_headers_identity() {
        let mut headers = HeaderMap::new();

        assert_eq!(
            Compression::from_headers(&headers),
            Ok(Compression::Identity)
        );

        headers.insert(CONTENT_ENCODING, HeaderValue::from_static(""));
        assert_eq!(
            Compression::from_headers(&headers),
            Ok(Compression::Identity)
        );

        headers.insert(CONTENT_ENCODING, HeaderValue::from_static(" identity "));
        assert_eq!(
            Compression::from_headers(&headers),
            Ok(Compression::Identity)
        );
    }

    #[test]
    fn test_from_headers_supported_encodings() {
        for (header, expected) in [
            ("br", Compression::Brotli),
            (" br ", Compression::Brotli),
            ("deflate", Compression::Deflate),
            ("gzip", Compression::Gzip),
            ("x-gzip", Compression::Gzip),
            ("zstd", Compression::Zstd),
            (" zstd ", Compression::Zstd),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(CONTENT_ENCODING, HeaderValue::from_static(header));

            assert_eq!(Compression::from_headers(&headers), Ok(expected));
        }
    }

    #[test]
    fn test_from_headers_unsupported_encoding() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_ENCODING, HeaderValue::from_static("compress"));

        assert_eq!(
            Compression::from_headers(&headers),
            Err(CompressionError("Unsupported content encoding"))
        );
    }

    #[test]
    fn test_from_headers_multiple_encodings() {
        let mut headers = HeaderMap::new();
        headers.append(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        headers.append(CONTENT_ENCODING, HeaderValue::from_static("zstd"));

        assert_eq!(
            Compression::from_headers(&headers),
            Err(CompressionError("Multiple content encodings not supported"))
        );
    }

    macro_rules! test_decompress {
        ($test_name:ident, $compression:ident, $encoder:expr) => {
            #[tokio::test]
            async fn $test_name() {
                let input = b"hello from a compressed response";
                let mut encoder = ($encoder)(BufReader::new(&input[..]));
                let mut compressed = Vec::new();
                encoder.read_to_end(&mut compressed).await.unwrap();

                let source = BufReader::new(compressed.as_slice());
                let mut decoder = Compression::$compression.decompress(source);
                let mut output = Vec::new();
                decoder.read_to_end(&mut output).await.unwrap();

                assert_eq!(output, input);
            }
        };
    }

    test_decompress!(test_decompress_identity, Identity, std::convert::identity);
    test_decompress!(
        test_decompress_brotli,
        Brotli,
        async_compression::tokio::bufread::BrotliEncoder::new
    );
    test_decompress!(
        test_decompress_deflate,
        Deflate,
        async_compression::tokio::bufread::ZlibEncoder::new
    );
    test_decompress!(
        test_decompress_gzip,
        Gzip,
        async_compression::tokio::bufread::GzipEncoder::new
    );
    test_decompress!(
        test_decompress_zstd,
        Zstd,
        async_compression::tokio::bufread::ZstdEncoder::new
    );
}
