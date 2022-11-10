use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::process::{Command, Stdio};

use flate2::read::{MultiGzDecoder, ZlibDecoder};
use tempfile::NamedTempFile;

/// Decompresses an object file.
///
/// Some compression methods are implemented by spawning an external tool and can only
/// process from a named pathname, hence we need a [`NamedTempFile`] as source.
pub fn decompress_object_file(src: &NamedTempFile, mut dst: File) -> io::Result<File> {
    // Ensure that both meta data and file contents are available to the
    // subsequent reads of the file metadata and reads from other threads.
    src.as_file().sync_all()?;

    let metadata = src.as_file().metadata()?;
    metric!(time_raw("objects.size") = metadata.len());

    src.as_file().seek(SeekFrom::Start(0))?;
    let mut magic_bytes: [u8; 4] = [0, 0, 0, 0];
    src.as_file().read_exact(&mut magic_bytes)?;
    src.as_file().seek(SeekFrom::Start(0))?;

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

            zstd::stream::copy_decode(src.as_file(), &mut dst)?;
            Ok(dst)
        }
        // Magic bytes for gzip
        // https://tools.ietf.org/html/rfc1952#section-2.3.1
        [0x1f, 0x8b, _, _] => {
            metric!(counter("compression") += 1, "type" => "gz");

            // We assume MultiGzDecoder accepts a strict superset of input
            // values compared to GzDecoder.
            let mut reader = MultiGzDecoder::new(src.as_file());
            io::copy(&mut reader, &mut dst)?;
            Ok(dst)
        }
        // Magic bytes for zlib
        [0x78, 0x01, _, _] | [0x78, 0x9c, _, _] | [0x78, 0xda, _, _] => {
            metric!(counter("compression") += 1, "type" => "zlib");

            let mut reader = ZlibDecoder::new(src.as_file());
            io::copy(&mut reader, &mut dst)?;
            Ok(dst)
        }
        // Magic bytes for CAB
        [77, 83, 67, 70] => {
            metric!(counter("compression") += 1, "type" => "cab");

            let status = Command::new("cabextract")
                .arg("-sfqp")
                .arg(src.path())
                .stdout(Stdio::from(dst.try_clone()?))
                .stderr(Stdio::null())
                .status()?;

            if !status.success() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "failed to decompress cab file",
                ));
            }

            Ok(dst)
        }
        // Probably not compressed
        _ => {
            metric!(counter("compression") += 1, "type" => "none");
            Ok(src.reopen()?)
        }
    }
}
