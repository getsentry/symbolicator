use std::io::{self, Read, Seek};

use cab::Cabinet;
use flate2::read::{MultiGzDecoder, ZlibDecoder};
use tempfile::NamedTempFile;

/// The compression format that was detected on a downloaded file.
///
/// Returned by `maybe_decompress_file` so callers know whether (and how) the
/// original bytes were compressed before decompression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionKind {
    Zstd,
    Gzip,
    Zlib,
    Zip,
    Cab,
}

/// Outcome of a `maybe_decompress_file` call.
#[derive(Debug, Default)]
pub struct DecompressOutcome {
    /// The detected compression kind, or `None` if the file was already uncompressed.
    pub kind: Option<CompressionKind>,
    /// The original (still-compressed) payload. `Some` only when the caller requested
    /// preservation via `want_raw_sink` *and* a compressed magic was actually detected.
    pub raw_sink: Option<NamedTempFile>,
}

/// Creates a sibling tempfile and copies the still-compressed contents of `src` into it,
/// rewinding both. Returns the populated sink.
///
/// Called from each decompression branch *before* the source tempfile is swapped, so the
/// caller can preserve the upstream-compressed payload (used by the `raw_compressed` cache
/// to answer `.pd_` / `.dl_` / `.ex_` proxy requests byte-identically).
fn allocate_raw_sink(src: &NamedTempFile) -> io::Result<NamedTempFile> {
    let mut sink = tempfile_in_parent(src)?;
    let mut reader = src.as_file();
    reader.rewind()?;
    io::copy(&mut reader, sink.as_file_mut())?;
    sink.as_file_mut().sync_all()?;
    sink.as_file_mut().rewind()?;
    // Leave `src` rewound so the subsequent decompressor reads from the start.
    src.as_file().rewind()?;
    Ok(sink)
}

/// Decompresses a downloaded file.
///
/// The passed [`NamedTempFile`] might be swapped with a fresh one in case decompression happens.
/// That new temp file will be created in the same directory as the original one.
///
/// When `want_raw_sink` is `true` *and* the file is detected as a **CAB** payload, the
/// original (still-compressed) bytes are copied into a fresh sibling tempfile and returned
/// as [`DecompressOutcome::raw_sink`]. Other compression formats (gzip / zstd / zlib / zip)
/// are decompressed normally but the original bytes are not preserved: the proxy serves
/// the raw-compressed mirror under `Content-Type: application/vnd.ms-cab-compressed`, so
/// it would be incorrect to return non-CAB bytes from it. For those formats the fallback
/// path synthesizes a fresh CAB on demand instead.
pub fn maybe_decompress_file(
    src: &mut NamedTempFile,
    want_raw_sink: bool,
) -> io::Result<DecompressOutcome> {
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
        return Ok(DecompressOutcome::default());
    }

    let mut magic_bytes: [u8; 4] = [0, 0, 0, 0];
    file.read_exact(&mut magic_bytes)?;
    file.rewind()?;

    let mut raw_sink: Option<NamedTempFile> = None;
    // Allocate the tee sink lazily -- only when we know a CAB magic was matched. Other
    // compression formats are decompressed normally but their original bytes are
    // discarded because the proxy serves the mirror under `vnd.ms-cab-compressed` and
    // cannot honestly label a gzip / zstd / zlib / zip body as CAB.
    let mut tee_cab = |src: &NamedTempFile| -> io::Result<()> {
        if want_raw_sink {
            raw_sink = Some(allocate_raw_sink(src)?);
        }
        Ok(())
    };

    // For a comprehensive list also refer to
    // https://en.wikipedia.org/wiki/List_of_file_signatures
    //
    // XXX: The decoders in the flate2 crate also support being used as a
    // wrapper around a Write. Only zstd doesn't. If we can get this into
    // zstd we could save one tempfile and especially avoid the io::copy
    // for downloads that were not compressed.
    let kind = match magic_bytes {
        // Magic bytes for zstd
        // https://tools.ietf.org/id/draft-kucherawy-dispatch-zstd-00.html#rfc.section.2.1.1
        [0x28, 0xb5, 0x2f, 0xfd] => {
            metric!(counter("compression") += 1, "type" => "zstd");

            let mut dst = tempfile_in_parent(src)?;
            zstd::stream::copy_decode(file, &mut dst)?;

            std::mem::swap(src, &mut dst);
            Some(CompressionKind::Zstd)
        }
        // Magic bytes for gzip
        // https://tools.ietf.org/html/rfc1952#section-2.3.1
        [0x1f, 0x8b, _, _] => {
            metric!(counter("compression") += 1, "type" => "gz");

            // We assume MultiGzDecoder accepts a strict superset of input
            // values compared to GzDecoder.
            let mut dst = tempfile_in_parent(src)?;
            let mut reader = MultiGzDecoder::new(file);
            io::copy(&mut reader, &mut dst)?;

            std::mem::swap(src, &mut dst);
            Some(CompressionKind::Gzip)
        }
        // Magic bytes for zlib
        [0x78, 0x01, _, _] | [0x78, 0x9c, _, _] | [0x78, 0xda, _, _] => {
            metric!(counter("compression") += 1, "type" => "zlib");

            let mut dst = tempfile_in_parent(src)?;
            let mut reader = ZlibDecoder::new(file);
            io::copy(&mut reader, &mut dst)?;

            std::mem::swap(src, &mut dst);
            Some(CompressionKind::Zlib)
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
                let mut symbol_file = archive.by_index(0)?;

                let mut dst = tempfile_in_parent(src)?;
                io::copy(&mut symbol_file, &mut dst)?;

                dst
            };

            std::mem::swap(src, &mut dst);
            Some(CompressionKind::Zip)
        }
        // Magic bytes for CAB
        [77, 83, 67, 70] => {
            metric!(counter("compression") += 1, "type" => "cab");
            tee_cab(src)?;

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

            let mut dst = tempfile_in_parent(src)?;
            let mut reader = cab_file.read_file(&first_file)?;
            std::io::copy(&mut reader, &mut dst)?;
            std::mem::swap(src, &mut dst);
            Some(CompressionKind::Cab)
        }
        // Probably not compressed
        _ => {
            metric!(counter("compression") += 1, "type" => "none");
            None
        }
    };
    drop(tee_cab);

    Ok(DecompressOutcome { kind, raw_sink })
}

// FIXME(swatinem): this fn needs a better place
pub fn tempfile_in_parent(file: &NamedTempFile) -> io::Result<NamedTempFile> {
    let dir = file
        .path()
        .parent()
        .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
    NamedTempFile::new_in(dir)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn tempfile_with(contents: &[u8]) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(contents).unwrap();
        f.as_file_mut().sync_all().unwrap();
        f.as_file_mut().rewind().unwrap();
        f
    }

    fn read_to_vec(f: &mut NamedTempFile) -> Vec<u8> {
        f.as_file_mut().rewind().unwrap();
        let mut out = Vec::new();
        f.as_file_mut().read_to_end(&mut out).unwrap();
        out
    }

    #[test]
    fn test_uncompressed_returns_none() {
        let mut src = tempfile_with(b"hello world, not compressed at all");
        let outcome = maybe_decompress_file(&mut src, true).unwrap();
        assert_eq!(outcome.kind, None);
        // No tee allocation for uncompressed input -- this is the lazy-allocation contract.
        assert!(outcome.raw_sink.is_none());
        assert_eq!(read_to_vec(&mut src), b"hello world, not compressed at all");
    }

    #[test]
    fn test_gzip_decompresses_but_does_not_tee() {
        // Non-CAB compression must NOT populate `raw_sink` even when `want_raw_sink: true`.
        // The proxy serves the mirror as `vnd.ms-cab-compressed`; returning gzip bytes
        // under that content-type would break clients (WinDbg etc.).
        let payload = b"the original symbol bytes";
        let mut gz = Vec::new();
        {
            let mut enc = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::default());
            enc.write_all(payload).unwrap();
            enc.finish().unwrap();
        }

        let mut src = tempfile_with(&gz);
        let outcome = maybe_decompress_file(&mut src, true).unwrap();

        assert_eq!(outcome.kind, Some(CompressionKind::Gzip));
        assert!(outcome.raw_sink.is_none(), "gzip bytes must not enter the CAB mirror");
        assert_eq!(read_to_vec(&mut src), payload);
    }

    #[test]
    fn test_gzip_no_sink_still_decompresses() {
        let payload = b"abc";
        let mut gz = Vec::new();
        {
            let mut enc = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::default());
            enc.write_all(payload).unwrap();
            enc.finish().unwrap();
        }
        let mut src = tempfile_with(&gz);
        let outcome = maybe_decompress_file(&mut src, false).unwrap();
        assert_eq!(outcome.kind, Some(CompressionKind::Gzip));
        assert!(outcome.raw_sink.is_none());
        assert_eq!(read_to_vec(&mut src), payload);
    }

    #[test]
    fn test_cab_decompresses_and_tees_raw() {
        // CAB is the only format whose original bytes are preserved -- those bytes ARE
        // valid `vnd.ms-cab-compressed` content and can be served verbatim by the proxy.
        let payload = b"hello world, fake PDB bytes inside a CAB";
        let mut cab_bytes = Vec::new();
        {
            let mut builder = cab::CabinetBuilder::new();
            builder
                .add_folder(cab::CompressionType::MsZip)
                .add_file("foo.pdb".to_owned());
            let mut w = builder.build(std::io::Cursor::new(&mut cab_bytes)).unwrap();
            while let Some(mut fw) = w.next_file().unwrap() {
                let mut src = std::io::Cursor::new(payload);
                io::copy(&mut src, &mut fw).unwrap();
            }
            w.finish().unwrap();
        }

        let mut src = tempfile_with(&cab_bytes);
        let outcome = maybe_decompress_file(&mut src, true).unwrap();

        assert_eq!(outcome.kind, Some(CompressionKind::Cab));
        let mut sink = outcome.raw_sink.expect("CAB tee must populate raw_sink");
        assert_eq!(read_to_vec(&mut sink), cab_bytes);
        assert_eq!(read_to_vec(&mut src), payload);
    }
}
