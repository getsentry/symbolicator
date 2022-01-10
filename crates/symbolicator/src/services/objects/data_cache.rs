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
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use sentry::{Hub, SentryFutureExt};
use symbolic::common::ByteView;
use symbolic::debuginfo::{Archive, Object};
use tempfile::tempfile_in;
use tempfile::NamedTempFile;

use crate::cache::CacheStatus;
use crate::logging::LogError;
use crate::services::cacher::{CacheItemRequest, CacheKey, CachePath};
use crate::services::download::DownloadService;
use crate::services::download::RemoteDif;
use crate::services::download::{DownloadError, DownloadStatus};
use crate::types::{ObjectId, Scope};
use crate::utils::compression::decompress_object_file;
use crate::utils::futures::{m, measure};
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
        // Interestingly all usages of parse() check to make sure that self.status == Positive before
        // actually invoking it.
        match &self.status {
            CacheStatus::Positive => Ok(Some(Object::parse(&self.data)?)),
            CacheStatus::Negative => Ok(None),
            CacheStatus::Malformed(_) => Err(ObjectError::Malformed),
            CacheStatus::CacheSpecificError(message) => Err(ObjectError::Download(
                DownloadError::from_cache(&self.status)
                    .unwrap_or_else(|| DownloadError::CachedError(message.clone())),
            )),
        }
    }

    pub fn status(&self) -> &CacheStatus {
        &self.status
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

/// Downloads the object file, processes it and returns whether the file is in the cache.
///
/// If the object file was successfully downloaded it is first decompressed.  If it is
/// an archive containing multiple objects, then next the object matching the code or
/// debug ID of our request is extracted first.  Finally the object is parsed with
/// symbolic to ensure it is not malformed.
///
/// If there is an error decompression then an `Err` of [`ObjectError`] is returned.  If the
/// parsing the final object file failed, or there is an error downloading the file an `Ok` with
/// [`CacheStatus::Malformed`] is returned.
///
/// If the object file did not exist on the source a [`CacheStatus::Negative`] will be
/// returned.
///
/// If there was an error downloading the object file, an `Ok` with
/// [`CacheStatus::CacheSpecificError`] is returned.
///
/// If the object file did not exist on the source an `Ok` with [`CacheStatus::Negative`] will
/// be returned.
///
/// This is the actual implementation of [`CacheItemRequest::compute`] for
/// [`FetchFileDataRequest`] but outside of the trait so it can be written as async/await
/// code.
#[tracing::instrument(skip_all)]
async fn fetch_file(
    path: PathBuf,
    cache_key: CacheKey,
    object_id: ObjectId,
    file_id: RemoteDif,
    downloader: Arc<DownloadService>,
    tempfile: std::io::Result<NamedTempFile>,
) -> Result<CacheStatus, ObjectError> {
    log::trace!("Fetching file data for {}", cache_key);
    sentry::configure_scope(|scope| {
        scope.set_transaction(Some("download_file"));
        file_id.to_scope(scope);
        object_id.to_scope(scope);
    });

    let download_file = tempfile?;
    let download_dir = download_file
        .path()
        .parent()
        .ok_or(ObjectError::NoTempDir)?;

    let status = downloader.download(file_id, download_file.path()).await;

    match status {
        Ok(DownloadStatus::NotFound) => {
            log::debug!("No debug file found for {}", cache_key);
            return Ok(CacheStatus::Negative);
        }

        Err(e) => {
            // We want to error-log "interesting" download errors so we can look them up
            // in our internal sentry. We downgrade to debug-log for unactionable
            // permissions errors. Since this function does a fresh download, it will never
            // hit `CachedError`, but listing it for completeness is not a bad idea either.
            match e {
                DownloadError::Permissions | DownloadError::CachedError(_) => {
                    log::debug!("Error while downloading file: {}", LogError(&e))
                }
                _ => log::error!("Error while downloading file: {}", LogError(&e)),
            }

            return Ok(CacheStatus::CacheSpecificError(e.for_cache()));
        }

        Ok(DownloadStatus::Completed) => {
            // fall-through
        }
    }

    log::trace!("Finished download of {}", cache_key);
    let decompress_result = decompress_object_file(&download_file, tempfile_in(download_dir)?);

    // Treat decompression errors as malformed files. It is more likely that
    // the error comes from a corrupt file than a local file system error.
    let mut decompressed = match decompress_result {
        Ok(decompressed) => decompressed,
        Err(e) => return Ok(CacheStatus::Malformed(e.to_string())),
    };

    // Seek back to the start and parse this object so we can deal with it.
    // Since objects in Sentry (and potentially also other sources) might be
    // multi-arch files (e.g. FatMach), we parse as Archive and try to
    // extract the wanted file.
    decompressed.seek(SeekFrom::Start(0))?;
    let view = ByteView::map_file(decompressed)?;
    let archive = match Archive::parse(&view) {
        Ok(archive) => archive,
        Err(e) => return Ok(CacheStatus::Malformed(e.to_string())),
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
                if let Some(Err(err)) = archive.objects().find(|r| r.is_err()) {
                    return Ok(CacheStatus::Malformed(err.to_string()));
                } else {
                    return Ok(CacheStatus::Negative);
                }
            }
        };

        io::copy(&mut object.data(), &mut persist_file)?;
    } else {
        // Attempt to parse the object to capture errors. The result can be
        // discarded as the object's data is the entire ByteView.
        if let Err(err) = archive.object_by_index(0) {
            return Ok(CacheStatus::Malformed(err.to_string()));
        }

        io::copy(&mut view.as_ref(), &mut persist_file)?;
    }

    Ok(CacheStatus::Positive)
}

impl CacheItemRequest for FetchFileDataRequest {
    type Item = ObjectHandle;
    type Error = ObjectError;

    fn get_cache_key(&self) -> CacheKey {
        self.0.get_cache_key()
    }

    fn compute(&self, path: &Path) -> BoxFuture<'static, Result<CacheStatus, Self::Error>> {
        let future = fetch_file(
            path.to_owned(),
            self.get_cache_key(),
            self.0.object_id.clone(),
            self.0.file_source.clone(),
            self.0.download_svc.clone(),
            self.0.data_cache.tempfile(),
        );

        let future = async move {
            future.await.map_err(|e| {
                sentry::capture_error(&e);
                e
            })
        }
        .bind_hub(Hub::current());

        let type_name = self.0.file_source.source_type_name().into();

        let future = tokio::time::timeout(Duration::from_secs(600), future);
        let future = measure(
            "objects",
            m::timed_result,
            Some(("source_type", type_name)),
            future,
        );
        Box::pin(async move { future.await.map_err(|_| ObjectError::Timeout)? })
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
    use std::sync::Arc;
    use std::time::Duration;

    use crate::cache::{Cache, CacheName, CacheStatus};
    use crate::config::{CacheConfig, CacheConfigs, Config};
    use crate::services::download::{DownloadError, DownloadService};
    use crate::services::objects::data_cache::Scope;
    use crate::services::objects::{FindObject, ObjectPurpose, ObjectsActor};
    use crate::services::shared_cache::SharedCacheService;
    use crate::sources::FileType;
    use crate::test::{self, tempdir};

    use symbolic::common::DebugId;
    use tempfile::TempDir;

    async fn objects_actor(tempdir: &TempDir) -> ObjectsActor {
        let meta_cache = Cache::from_config(
            CacheName::ObjectMeta,
            Some(tempdir.path().join("meta")),
            None,
            CacheConfig::from(CacheConfigs::default().derived),
            Default::default(),
        )
        .unwrap();

        let data_cache = Cache::from_config(
            CacheName::Objects,
            Some(tempdir.path().join("data")),
            None,
            CacheConfig::from(CacheConfigs::default().downloaded),
            Default::default(),
        )
        .unwrap();

        let config = Arc::new(Config {
            connect_to_reserved_ips: true,
            max_download_timeout: Duration::from_millis(100),
            ..Config::default()
        });

        let download_svc = DownloadService::new(config);
        let shared_cache_svc = Arc::new(SharedCacheService::new(None).await);
        ObjectsActor::new(meta_cache, data_cache, shared_cache_svc, download_svc)
    }

    #[tokio::test]
    async fn test_download_error_cache_server_error() {
        test::setup();

        let server = test::FailingSymbolServer::new();
        let cachedir = tempdir();
        let objects_actor = objects_actor(&cachedir).await;

        let find_object = FindObject {
            // A request for a bcsymbolmap will expand to only one file that is being looked up.
            // Other filetypes will lead to multiple requests, trying different file extensions, etc
            filetypes: &[FileType::BcSymbolMap],
            purpose: ObjectPurpose::Debug,
            scope: Scope::Global,
            identifier: DebugId::default().into(),
            sources: Arc::new([]),
        };

        // for each of the different symbol sources, we assert that:
        // * we get a cache-specific error no matter how often we try
        // * we hit the symbol source exactly once for the initial request, followed by 3 retries
        // * the second try should *not* hit the symbol source, but should rather be served by the cache

        // server rejects the request (500)
        let find_object = FindObject {
            sources: Arc::new([server.reject_source.clone()]),
            ..find_object
        };
        let result = objects_actor.find(find_object.clone()).await.unwrap();
        assert_eq!(
            result.meta.clone().unwrap().status,
            CacheStatus::CacheSpecificError(String::from(
                "failed to download: 500 Internal Server Error"
            ))
        );
        assert_eq!(server.accesses(), 1 + 3); // 1 initial attempt + 3 retries
        let result = objects_actor.find(find_object.clone()).await.unwrap();
        assert_eq!(
            result.meta.unwrap().status,
            CacheStatus::CacheSpecificError(String::from(
                "failed to download: 500 Internal Server Error"
            ))
        );
        assert_eq!(server.accesses(), 0);
    }

    #[tokio::test]
    async fn test_negative_cache_not_found() {
        test::setup();

        let server = test::FailingSymbolServer::new();
        let cachedir = tempdir();
        let objects_actor = objects_actor(&cachedir).await;

        let find_object = FindObject {
            // A request for a bcsymbolmap will expand to only one file that is being looked up.
            // Other filetypes will lead to multiple requests, trying different file extensions, etc
            filetypes: &[FileType::BcSymbolMap],
            purpose: ObjectPurpose::Debug,
            scope: Scope::Global,
            identifier: DebugId::default().into(),
            sources: Arc::new([]),
        };

        // for each of the different symbol sources, we assert that:
        // * we get a negative cache result no matter how often we try
        // * we hit the symbol source exactly once for the initial request
        // * the second try should *not* hit the symbol source, but should rather be served by the cache

        // server responds with not found (404)
        let find_object = FindObject {
            sources: Arc::new([server.not_found_source.clone()]),
            ..find_object
        };
        let result = objects_actor.find(find_object.clone()).await.unwrap();
        assert_eq!(result.meta.unwrap().status, CacheStatus::Negative);
        assert_eq!(server.accesses(), 1);
        let result = objects_actor.find(find_object.clone()).await.unwrap();
        assert_eq!(result.meta.unwrap().status, CacheStatus::Negative);
        assert_eq!(server.accesses(), 0);
    }

    #[tokio::test]
    async fn test_download_error_cache_timeout() {
        test::setup();

        let server = test::FailingSymbolServer::new();
        let cachedir = tempdir();
        let objects_actor = objects_actor(&cachedir).await;

        let find_object = FindObject {
            // A request for a bcsymbolmap will expand to only one file that is being looked up.
            // Other filetypes will lead to multiple requests, trying different file extensions, etc
            filetypes: &[FileType::BcSymbolMap],
            purpose: ObjectPurpose::Debug,
            scope: Scope::Global,
            identifier: DebugId::default().into(),
            sources: Arc::new([]),
        };

        // for each of the different symbol sources, we assert that:
        // * we get a cache-specific error no matter how often we try
        // * we hit the symbol source exactly once for the initial request, followed by 3 retries
        // * the second try should *not* hit the symbol source, but should rather be served by the cache

        // server accepts the request, but never sends any reply (timeout)
        let find_object = FindObject {
            sources: Arc::new([server.pending_source.clone()]),
            ..find_object
        };
        let result = objects_actor.find(find_object.clone()).await.unwrap();
        assert_eq!(
            result.meta.unwrap().status,
            CacheStatus::CacheSpecificError(String::from("download was cancelled"))
        );
        assert_eq!(server.accesses(), 1);
        let result = objects_actor.find(find_object.clone()).await.unwrap();
        assert_eq!(
            result.meta.unwrap().status,
            CacheStatus::CacheSpecificError(String::from("download was cancelled"))
        );
        assert_eq!(server.accesses(), 0);
    }

    #[tokio::test]
    async fn test_download_error_cache_forbidden() {
        test::setup();

        let server = test::FailingSymbolServer::new();
        let cachedir = tempdir();
        let objects_actor = objects_actor(&cachedir).await;

        let find_object = FindObject {
            // A request for a bcsymbolmap will expand to only one file that is being looked up.
            // Other filetypes will lead to multiple requests, trying different file extensions, etc
            filetypes: &[FileType::BcSymbolMap],
            purpose: ObjectPurpose::Debug,
            scope: Scope::Global,
            identifier: DebugId::default().into(),
            sources: Arc::new([]),
        };

        // for each of the different symbol sources, we assert that:
        // * we get a cache-specific error no matter how often we try
        // * we hit the symbol source exactly once for the initial request, followed by 3 retries
        // * the second try should *not* hit the symbol source, but should rather be served by the cache

        // server rejects the request (403)
        let find_object = FindObject {
            sources: Arc::new([server.forbidden_source.clone()]),
            ..find_object
        };
        let result = objects_actor.find(find_object.clone()).await.unwrap();
        assert_eq!(
            result.meta.clone().unwrap().status,
            CacheStatus::CacheSpecificError(DownloadError::Permissions.to_string())
        );
        assert_eq!(server.accesses(), 1 + 3); // 1 initial attempt + 3 retries
        let result = objects_actor.find(find_object.clone()).await.unwrap();
        assert_eq!(
            result.meta.unwrap().status,
            CacheStatus::CacheSpecificError(DownloadError::Permissions.to_string())
        );
        assert_eq!(server.accesses(), 0);
    }
}
