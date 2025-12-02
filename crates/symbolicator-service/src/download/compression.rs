use std::io::{self, Read, Seek};

use cab::Cabinet;
use flate2::read::{MultiGzDecoder, ZlibDecoder};
use tempfile::NamedTempFile;

/// Decompresses a downloaded file.
///
/// The passed [`NamedTempFile`] might be swapped with a fresh one in case decompression happens.
/// That new temp file will be created in the same directory as the original one.
pub fn maybe_decompress_file(src: &mut NamedTempFile) -> io::Result<()> {
    // Ensure that both meta data and file contents are available to the
    // subsequent reads of the file metadata and reads from other threads.
    let mut file = src.as_file();
    file.sync_all()?;

    let metadata = file.metadata()?;
    // TODO(swatinem): we should rename this to a more descriptive metric, as we use this for *all*
    // kinds of downloaded files, not only "objects".
    metric!(distribution("objects.size") = metadata.len());

    file.rewind()?;
    if metadata.len() < 4 {
        // we don’t want to error for empty files here
        return Ok(());
    }

    let mut magic_bytes: [u8; 4] = [0, 0, 0, 0];
    file.read_exact(&mut magic_bytes)?;
    file.rewind()?;

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

            let mut dst = tempfile_in_parent(src)?;
            zstd::stream::copy_decode(file, &mut dst)?;

            std::mem::swap(src, &mut dst);
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
        }
        // Magic bytes for zlib
        [0x78, 0x01, _, _] | [0x78, 0x9c, _, _] | [0x78, 0xda, _, _] => {
            metric!(counter("compression") += 1, "type" => "zlib");

            let mut dst = tempfile_in_parent(src)?;
            let mut reader = ZlibDecoder::new(file);
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
                let mut symbol_file = archive.by_index(0)?;

                let mut dst = tempfile_in_parent(src)?;
                io::copy(&mut symbol_file, &mut dst)?;

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

            let mut dst = tempfile_in_parent(src)?;
            let mut reader = cab_file.read_file(&first_file)?;
            std::io::copy(&mut reader, &mut dst)?;
            std::mem::swap(src, &mut dst);
        }
        // Probably not compressed
        _ => {
            metric!(counter("compression") += 1, "type" => "none");
        }
    }

    Ok(())
}

// FIXME(swatinem): this fn needs a better place
pub fn tempfile_in_parent(file: &NamedTempFile) -> io::Result<NamedTempFile> {
    let dir = file
        .path()
        .parent()
        .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
    NamedTempFile::new_in(dir)
}
