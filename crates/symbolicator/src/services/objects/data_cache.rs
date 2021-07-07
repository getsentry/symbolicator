//! Data cache for the object actor.
//!
//! This implements a cache holding the content of object files.  It does this by
//! implementing the [`CacheItemRequest`] trait for a [`FetchFileDataRequest`] which can be
//! used with a [`Cacher`] to make a filesystem based cache.
//!
//! [`Cacher`]: crate::services::cacher::Cacher

use std::cmp;
use std::fmt;
use std::fs;
use std::io::{self, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;

use futures::compat::Future01CompatExt;
use futures::future::{FutureExt, TryFutureExt};
use sentry::{Hub, SentryFutureExt};
use symbolic::common::ByteView;
use symbolic::debuginfo::{Archive, Object};
use tempfile::tempfile_in;

use crate::cache::{CacheKey, CacheStatus};
use crate::logging::LogError;
use crate::services::cacher::{CacheItemRequest, CachePath};
use crate::services::download::{DownloadStatus, RemoteDif};
use crate::types::{ObjectId, Scope};
use crate::utils::compression::decompress_object_file;
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

    pub(super) file_source: RemoteDif,
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
        scope.set_tag("object_file.scope", self.scope());
        scope.set_extra(
            "object_file.first_16_bytes",
            format!("{:x?}", &self.data[..cmp::min(self.data.len(), 16)]).into(),
        );
    }
}

impl fmt::Display for ObjectHandle {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if let Some(ref debug_id) = self.object_id.debug_id {
            write!(f, "<object handle for {}>", debug_id)
        } else if let Some(ref code_id) = self.object_id.code_id {
            write!(f, "<object handle for {}>", code_id)
        } else {
            write!(f, "<object handle for unknown>")
        }
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
                .await;

            match status {
                Ok(DownloadStatus::NotFound) => {
                    log::debug!("No debug file found for {}", cache_key);
                    return Ok(CacheStatus::Negative);
                }

                Err(e) => {
                    log::error!("Error while downloading file: {}", LogError(&e));
                    return Ok(CacheStatus::Negative);
                }

                Ok(DownloadStatus::Completed) => {
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
                Err(_) => return Ok(CacheStatus::Malformed),
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::time::Duration;

    use crate::cache::Cache;
    use crate::config::{CacheConfig, CacheConfigs, Config};
    use crate::services::download::DownloadService;
    use crate::services::objects::data_cache::{CacheStatus, Scope};
    use crate::services::objects::{FindObject, ObjectPurpose, ObjectsActor};
    use crate::sources::{FileType, HttpSourceConfig, SourceConfig, SourceId};
    use crate::test::{self, fixture, Server};

    use symbolic::common::DebugId;

    use warp::filters::fs::File;
    use warp::reject::{Reject, Rejection};
    use warp::Filter;

    #[derive(Debug, Clone, Copy)]
    struct GoAway;

    impl Reject for GoAway {}

    struct FailingSymbolServer {
        server: Server,
        times_accessed: Arc<AtomicUsize>,
        accept_source: SourceConfig,
        reject_source: SourceConfig,
        pending_source: SourceConfig,
        not_found_source: SourceConfig,
    }
    impl FailingSymbolServer {
        fn new() -> Self {
            let times_accessed = Arc::new(AtomicUsize::new(0));

            let times = times_accessed.clone();
            let reject = warp::path("reject")
                .and(warp::fs::dir(fixture("symbols")))
                .and_then(move |_| {
                    let times = Arc::clone(&times);
                    async move {
                        (*times).fetch_add(1, std::sync::atomic::Ordering::SeqCst);

                        Err::<File, _>(warp::reject::custom(GoAway))
                    }
                });

            let times = times_accessed.clone();
            let not_found = warp::path("not-found")
                .and(warp::fs::dir(fixture("symbols")))
                .and_then(move |_| {
                    let times = Arc::clone(&times);
                    async move {
                        (*times).fetch_add(1, std::sync::atomic::Ordering::SeqCst);

                        Err::<File, _>(warp::reject::not_found())
                    }
                });

            let times = times_accessed.clone();
            let pending = warp::path("pending")
                .and(warp::fs::dir(fixture("symbols")))
                .and_then(move |_| {
                    let times = Arc::clone(&times);
                    (*times).fetch_add(1, std::sync::atomic::Ordering::SeqCst);

                    std::future::pending::<Result<File, Rejection>>()
                });

            let times = times_accessed.clone();
            let accept = warp::path("pending")
                .and(warp::fs::dir(fixture("symbols")))
                .map(move |file| {
                    let times = Arc::clone(&times);
                    (*times).fetch_add(1, Ordering::SeqCst);

                    file
                });

            let server = Server::new(reject.or(not_found).or(pending).or(accept));

            // The sources use the same identifier ("local") as the local file system source to avoid
            // differences when changing the bucket in tests.

            let accept_source = SourceConfig::Http(Arc::new(HttpSourceConfig {
                id: SourceId::new("accept"),
                url: server.url("accept/"),
                headers: Default::default(),
                files: Default::default(),
            }));

            let reject_source = SourceConfig::Http(Arc::new(HttpSourceConfig {
                id: SourceId::new("reject"),
                url: server.url("reject/"),
                headers: Default::default(),
                files: Default::default(),
            }));

            let pending_source = SourceConfig::Http(Arc::new(HttpSourceConfig {
                id: SourceId::new("pending"),
                url: server.url("pending/"),
                headers: Default::default(),
                files: Default::default(),
            }));

            let not_found_source = SourceConfig::Http(Arc::new(HttpSourceConfig {
                id: SourceId::new("not-found"),
                url: server.url("not-found/"),
                headers: Default::default(),
                files: Default::default(),
            }));

            FailingSymbolServer {
                server,
                times_accessed,
                accept_source,
                reject_source,
                pending_source,
                not_found_source,
            }
        }
    }
    fn objects_actor() -> ObjectsActor {
        let meta_cache = Cache::from_config(
            "meta",
            None,
            None,
            CacheConfig::from(CacheConfigs::default().derived),
        )
        .unwrap();

        let data_cache = Cache::from_config(
            "data",
            None,
            None,
            CacheConfig::from(CacheConfigs::default().downloaded),
        )
        .unwrap();

        let config = Arc::new(Config {
            connect_to_reserved_ips: true,
            download_timeout: Duration::from_secs(1),
            ..Config::default()
        });

        let download_svc = DownloadService::new(config);
        ObjectsActor::new(meta_cache, data_cache, download_svc)
    }

    #[tokio::test]
    async fn test_negative_cache() {
        test::setup();

        let server = FailingSymbolServer::new();

        let objects_actor = objects_actor();

        let find_object = FindObject {
            filetypes: FileType::all(),
            purpose: ObjectPurpose::Debug,
            scope: Scope::Global,
            identifier: DebugId::default().into(),
            sources: Arc::new([server.reject_source]),
        };

        objects_actor.find(find_object.clone()).await.unwrap();
        assert_eq!(server.times_accessed.load(Ordering::SeqCst), 1);
        objects_actor.find(find_object).await.unwrap();
        assert_eq!(server.times_accessed.load(Ordering::SeqCst), 1);
    }
}
