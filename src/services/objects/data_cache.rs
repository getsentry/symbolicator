//! Data cache for the object actor.
//!
//! This implements a cache holding the content of object files.  It does this by
//! implementing the [`CacheItemRequest`] trait for a [`FetchFileDataRequest`] which can be
//! used with a [`Cacher`] to make a filesystem based cache.
//!
//! [`Cacher`]: crate::services::cacher::Cacher

use std::cmp;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::process;
use std::time::Duration;

use futures::compat::Future01CompatExt;
use futures::future::{FutureExt, TryFutureExt};
use sentry::{Hub, SentryFutureExt};
use symbolic::common::ByteView;
use symbolic::debuginfo::{Archive, Object};
use tempfile::{tempfile_in, NamedTempFile};

use crate::cache::{CacheKey, CacheStatus};
use crate::services::cacher::{CacheItemRequest, CachePath};
use crate::services::download::{DownloadStatus, ObjectFileSource};
use crate::types::{ObjectId, Scope};
use crate::utils::futures::BoxedFuture;
use crate::utils::sentry::ConfigureScope;

use super::meta_cache::FetchFileMetaRequest;
use super::ObjectError;

/// This requests the file content of a single file at a specific path/url.
/// The attributes for this are the same as for `FetchFileMetaRequest`, hence the newtype
#[derive(Clone, Debug)]
pub(super) struct FetchFileDataRequest(pub(super) FetchFileMetaRequest);

/// Handle to local cache file of an object.
///
/// This handle contains some information identifying the object it is for as well as the
/// cache information.
#[derive(Debug, Clone)]
pub struct ObjectHandle {
    pub(super) object_id: ObjectId,
    pub(super) scope: Scope,

    pub(super) file_source: ObjectFileSource,
    pub(super) cache_key: CacheKey,

    /// The mmapped object.
    ///
    /// This only contains the object **if** [`ObjectHandle::status`] is
    /// [`CacheStatus::Positive`], otherwise it will contain an empty string or the special
    /// malformed marker.
    pub(super) data: ByteView<'static>,
    pub(super) status: CacheStatus,
}

impl ObjectHandle {
    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn has_object(&self) -> bool {
        self.status == CacheStatus::Positive
    }

    pub fn parse(&self) -> Result<Option<Object<'_>>, ObjectError> {
        match self.status {
            CacheStatus::Positive => Ok(Some(Object::parse(&self.data)?)),
            CacheStatus::Negative => Ok(None),
            CacheStatus::Malformed => Err(ObjectError::Malformed),
        }
    }

    pub fn status(&self) -> CacheStatus {
        self.status
    }

    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    pub fn cache_key(&self) -> &CacheKey {
        &self.cache_key
    }

    pub fn data(&self) -> ByteView<'static> {
        self.data.clone()
    }
}

impl ConfigureScope for ObjectHandle {
    fn to_scope(&self, scope: &mut ::sentry::Scope) {
        self.object_id.to_scope(scope);
        self.file_source.to_scope(scope);

        scope.set_tag("object_file.scope", self.scope());

        scope.set_extra(
            "object_file.first_16_bytes",
            format!("{:x?}", &self.data[..cmp::min(self.data.len(), 16)]).into(),
        );
    }
}

impl CacheItemRequest for FetchFileDataRequest {
    type Item = ObjectHandle;
    type Error = ObjectError;

    fn get_cache_key(&self) -> CacheKey {
        self.0.get_cache_key()
    }

    /// Downloads the object file, processes it and returns whether the file is in the cache.
    ///
    /// If the object file was successfully downloaded it is first decompressed.  If it is
    /// an archive containing multiple objects, then next the object matching the code or
    /// debug ID of our request is extracted first.  Finally the object is parsed with
    /// symbolic to ensure it is not malformed.
    ///
    /// If there is an error with downloading or decompression then an `Err` of
    /// [`ObjectError`] is returned.  However if only the final object file parsing failed
    /// then an `Ok` with [`CacheStatus::Malformed`] is returned.
    ///
    /// If the object file did not exist on the source a [`CacheStatus::Negative`] will be
    /// returned.
    fn compute(&self, path: &Path) -> BoxedFuture<Result<CacheStatus, Self::Error>> {
        let cache_key = self.get_cache_key();
        log::trace!("Fetching file data for {}", cache_key);

        let path = path.to_owned();
        let object_id = self.0.object_id.clone();

        sentry::configure_scope(|scope| {
            scope.set_transaction(Some("download_file"));
            self.0.file_source.to_scope(scope);
        });

        let file_id = self.0.file_source.clone();
        let downloader = self.0.download_svc.clone();
        let download_file = tryf!(self.0.data_cache.tempfile());
        let download_dir =
            tryf!(download_file.path().parent().ok_or(ObjectError::NoTempDir)).to_owned();

        let future = async move {
            let status = downloader
                .download(file_id, download_file.path().to_owned())
                .await
                .map_err(Self::Error::from)?;

            match status {
                DownloadStatus::NotFound => {
                    log::debug!("No debug file found for {}", cache_key);
                    return Ok(CacheStatus::Negative);
                }
                DownloadStatus::Completed => {
                    // fall-through
                }
            }

            log::trace!("Finished download of {}", cache_key);
            let decompress_result =
                decompress_object_file(&download_file, tempfile_in(download_dir)?);

            // Treat decompression errors as malformed files. It is more likely that
            // the error comes from a corrupt file than a local file system error.
            let mut decompressed = match decompress_result {
                Ok(decompressed) => decompressed,
                Err(_) => return Ok(CacheStatus::Malformed),
            };

            // Seek back to the start and parse this object so we can deal with it.
            // Since objects in Sentry (and potentially also other sources) might be
            // multi-arch files (e.g. FatMach), we parse as Archive and try to
            // extract the wanted file.
            decompressed.seek(SeekFrom::Start(0))?;
            let view = ByteView::map_file(decompressed)?;
            let archive = match Archive::parse(&view) {
                Ok(archive) => archive,
                Err(_) => {
                    return Ok(CacheStatus::Malformed);
                }
            };
            let mut persist_file = fs::File::create(&path)?;
            if archive.is_multi() {
                let object_opt = archive
                    .objects()
                    .filter_map(Result::ok)
                    .find(|object| object_id.match_object(object));

                let object = match object_opt {
                    Some(object) => object,
                    None => {
                        if archive.objects().any(|r| r.is_err()) {
                            return Ok(CacheStatus::Malformed);
                        } else {
                            return Ok(CacheStatus::Negative);
                        }
                    }
                };

                io::copy(&mut object.data(), &mut persist_file)?;
            } else {
                // Attempt to parse the object to capture errors. The result can be
                // discarded as the object's data is the entire ByteView.
                if archive.object_by_index(0).is_err() {
                    return Ok(CacheStatus::Malformed);
                }

                io::copy(&mut view.as_ref(), &mut persist_file)?;
            }

            Ok(CacheStatus::Positive)
        };

        let result = future
            .boxed_local()
            .map_err(|e| {
                sentry::capture_error(&e);
                e
            })
            .bind_hub(Hub::current());

        let type_name = self.0.file_source.source_type_name();

        Box::pin(
            future_metrics!(
                "objects",
                Some((Duration::from_secs(600), ObjectError::Timeout)),
                result.compat(),
                "source_type" => type_name,
            )
            .compat(),
        )
    }

    fn load(
        &self,
        scope: Scope,
        status: CacheStatus,
        data: ByteView<'static>,
        _: CachePath,
    ) -> Self::Item {
        let object_handle = ObjectHandle {
            object_id: self.0.object_id.clone(),
            scope,

            file_source: self.0.file_source.clone(),
            cache_key: self.get_cache_key(),

            status,
            data,
        };

        object_handle.configure_scope();

        object_handle
    }
}

/// Decompresses an object file.
///
/// Some compression methods are implemented by spawning an external tool and can only
/// process from a named pathname, hence we need a [`NamedTempFile`] as source.
fn decompress_object_file(src: &NamedTempFile, mut dst: fs::File) -> io::Result<fs::File> {
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
            let mut reader = flate2::read::MultiGzDecoder::new(src.as_file());
            io::copy(&mut reader, &mut dst)?;
            Ok(dst)
        }
        // Magic bytes for zlib
        [0x78, 0x01, _, _] | [0x78, 0x9c, _, _] | [0x78, 0xda, _, _] => {
            metric!(counter("compression") += 1, "type" => "zlib");

            let mut reader = flate2::read::ZlibDecoder::new(src.as_file());
            io::copy(&mut reader, &mut dst)?;
            Ok(dst)
        }
        // Magic bytes for CAB
        [77, 83, 67, 70] => {
            metric!(counter("compression") += 1, "type" => "cab");

            let status = process::Command::new("cabextract")
                .arg("-sfqp")
                .arg(src.path())
                .stdout(process::Stdio::from(dst.try_clone()?))
                .stderr(process::Stdio::null())
                .status()?;

            if !status.success() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
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
