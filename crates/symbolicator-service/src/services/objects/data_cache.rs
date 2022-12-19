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
use std::io::{self, Seek};
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use sentry::{Hub, SentryFutureExt};
use symbolic::common::SelfCell;
use tempfile::{tempfile_in, NamedTempFile};

use symbolic::common::ByteView;
use symbolic::debuginfo::{Archive, Object};
use symbolicator_sources::ObjectId;

use crate::cache::{CacheEntry, CacheError, ExpirationTime};
use crate::services::cacher::{CacheItemRequest, CacheKey};
use crate::services::download::{DownloadError, DownloadService, DownloadStatus, RemoteDif};
use crate::types::Scope;
use crate::utils::compression::decompress_object_file;
use crate::utils::futures::{m, measure};
use crate::utils::sentry::ConfigureScope;

use super::meta_cache::FetchFileMetaRequest;

/// This requests the file content of a single file at a specific path/url.
/// The attributes for this are the same as for `FetchFileMetaRequest`, hence the newtype
#[derive(Clone, Debug)]
pub(super) struct FetchFileDataRequest(pub(super) FetchFileMetaRequest);

#[derive(Debug)]
pub struct OwnedObject(SelfCell<ByteView<'static>, Object<'static>>);

impl OwnedObject {
    fn parse(byteview: ByteView<'static>) -> CacheEntry<OwnedObject> {
        let obj = SelfCell::try_new(byteview, |p| unsafe {
            Object::parse(&*p).map_err(CacheError::from_std_error)
        })?;
        Ok(OwnedObject(obj))
    }
}

impl Clone for OwnedObject {
    fn clone(&self) -> Self {
        let byteview = self.0.owner().clone();
        Self::parse(byteview).unwrap()
    }
}

/// Handle to local cache file of an object.
///
/// This handle contains some information identifying the object it is for as well as the
/// cache information.
#[derive(Debug, Clone)]
pub struct ObjectHandle {
    pub object_id: ObjectId,

    object: OwnedObject,

    // FIXME(swatinem): the scope is only ever used for sentry events/scope
    scope: Scope,

    // FIXME(swatinem): the cache_key is only ever used for debug logging
    pub cache_key: CacheKey,
}

impl ObjectHandle {
    /// Get a reference to the parsed [`Object`].
    pub fn object(&self) -> &Object<'_> {
        self.object.0.get()
    }

    /// Get a reference to the underlying data.
    pub fn data(&self) -> &ByteView<'static> {
        self.object.0.owner()
    }
}

impl ConfigureScope for ObjectHandle {
    fn to_scope(&self, scope: &mut ::sentry::Scope) {
        self.object_id.to_scope(scope);
        scope.set_tag("object_file.scope", &self.scope);
        let data = self.data();
        scope.set_extra(
            "object_file.first_16_bytes",
            format!("{:x?}", &data[..cmp::min(data.len(), 16)]).into(),
        );
    }
}

impl fmt::Display for ObjectHandle {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if let Some(ref debug_id) = self.object_id.debug_id {
            write!(f, "<object handle for {debug_id}>")
        } else if let Some(ref code_id) = self.object_id.code_id {
            write!(f, "<object handle for {code_id}>")
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
) -> CacheEntry<()> {
    tracing::trace!("Fetching file data for {}", cache_key);
    sentry::configure_scope(|scope| {
        file_id.to_scope(scope);
        object_id.to_scope(scope);
    });

    let download_file = match downloader.download(file_id, tempfile?).await {
        Ok(DownloadStatus::NotFound) => {
            tracing::debug!("No debug file found for {}", cache_key);
            return Err(CacheError::NotFound);
        }
        Ok(DownloadStatus::PermissionDenied) => {
            // FIXME: this is really unreachable as the downloader converts these already
            return Err(CacheError::PermissionDenied("".into()));
        }

        Err(e) => {
            // We want to error-log "interesting" download errors so we can look them up
            // in our internal sentry. We downgrade to debug-log for unactionable
            // permissions errors. Since this function does a fresh download, it will never
            // hit `CachedError`, but listing it for completeness is not a bad idea either.
            let stderr: &dyn std::error::Error = &e;
            match e {
                DownloadError::Permissions | DownloadError::CachedError(_) => {
                    tracing::debug!(stderr, "Error while downloading file")
                }
                _ => tracing::error!(stderr, "Error while downloading file"),
            }

            return Err(CacheError::from(e));
        }

        Ok(DownloadStatus::Completed(download_file)) => download_file,
    };
    tracing::trace!("Finished download of {}", cache_key);

    let download_dir = download_file
        .path()
        .parent()
        .ok_or(CacheError::InternalError)?;
    let decompress_result = decompress_object_file(&download_file, tempfile_in(download_dir)?);

    // Treat decompression errors as malformed files. It is more likely that
    // the error comes from a corrupt file than a local file system error.
    let mut decompressed = match decompress_result {
        Ok(decompressed) => decompressed,
        Err(e) => return Err(CacheError::Malformed(e.to_string())),
    };

    // Seek back to the start and parse this object so we can deal with it.
    // Since objects in Sentry (and potentially also other sources) might be
    // multi-arch files (e.g. FatMach), we parse as Archive and try to
    // extract the wanted file.
    decompressed.rewind()?;
    let view = ByteView::map_file(decompressed)?;
    let archive = match Archive::parse(&view) {
        Ok(archive) => archive,
        Err(e) => return Err(CacheError::Malformed(e.to_string())),
    };
    let mut persist_file = fs::File::create(&path)?;
    if archive.is_multi() {
        let object_opt = archive
            .objects()
            .filter_map(Result::ok)
            .find(|object| object_matches_id(object, &object_id));

        let object = match object_opt {
            Some(object) => object,
            None => {
                if let Some(Err(err)) = archive.objects().find(|r| r.is_err()) {
                    return Err(CacheError::Malformed(err.to_string()));
                } else {
                    return Err(CacheError::NotFound);
                }
            }
        };

        io::copy(&mut object.data(), &mut persist_file)?;
    } else {
        // Attempt to parse the object to capture errors. The result can be
        // discarded as the object's data is the entire ByteView.
        if let Err(err) = archive.object_by_index(0) {
            return Err(CacheError::Malformed(err.to_string()));
        }

        io::copy(&mut view.as_ref(), &mut persist_file)?;
    }

    Ok(())
}

/// Validates that the object matches expected identifiers.
fn object_matches_id(object: &Object<'_>, id: &ObjectId) -> bool {
    if let Some(ref debug_id) = id.debug_id {
        let parsed_id = object.debug_id();

        // Microsoft symbol server sometimes stores updated files with a more recent
        // (=higher) age, but resolves it for requests with lower ages as well. Thus, we
        // need to check whether the parsed debug file fullfills the *miniumum* age bound.
        // For example:
        // `4A236F6A0B3941D1966B41A4FC77738C2` is reported as
        // `4A236F6A0B3941D1966B41A4FC77738C4` from the server.
        //                                  ^
        return parsed_id.uuid() == debug_id.uuid() && parsed_id.appendix() >= debug_id.appendix();
    }

    if let Some(ref code_id) = id.code_id {
        if let Some(ref object_code_id) = object.code_id() {
            if object_code_id != code_id {
                return false;
            }
        }
    }

    true
}

impl CacheItemRequest for FetchFileDataRequest {
    type Item = Arc<ObjectHandle>;

    fn get_cache_key(&self) -> CacheKey {
        self.0.get_cache_key()
    }

    fn compute(&self, path: &Path) -> BoxFuture<'static, CacheEntry<()>> {
        let future = fetch_file(
            path.to_owned(),
            self.get_cache_key(),
            self.0.object_id.clone(),
            self.0.file_source.clone(),
            self.0.download_svc.clone(),
            self.0.data_cache.tempfile(),
        )
        .bind_hub(Hub::current());

        let type_name = self.0.file_source.source_type_name().into();

        let timeout = Duration::from_secs(600);
        let future = tokio::time::timeout(timeout, future);
        let future = measure(
            "objects",
            m::timed_result,
            Some(("source_type", type_name)),
            future,
        );
        Box::pin(async move { future.await.map_err(|_| CacheError::Timeout(timeout))? })
    }

    fn load(&self, data: ByteView<'static>, _expiration: ExpirationTime) -> CacheEntry<Self::Item> {
        let object = OwnedObject::parse(data)?;
        let object_handle = ObjectHandle {
            object_id: self.0.object_id.clone(),
            object,

            scope: self.0.scope.clone(),
            cache_key: self.get_cache_key(),
        };

        // FIXME(swatinem): This `configure_scope` call happens in a spawned/deduplicated
        // cache request, which means users of the object (such as `write_symcache`) do
        // not observe it directly, hence they need to do their own.
        object_handle.configure_scope();

        Ok(Arc::new(object_handle))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use symbolicator_sources::FileType;

    use crate::cache::{Cache, CacheError, CacheName};
    use crate::config::{CacheConfig, CacheConfigs, Config};
    use crate::services::download::DownloadService;
    use crate::services::objects::data_cache::Scope;
    use crate::services::objects::{FindObject, ObjectPurpose, ObjectsActor};
    use crate::services::shared_cache::SharedCacheService;
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

        let config = Config {
            connect_to_reserved_ips: true,
            max_download_timeout: Duration::from_millis(100),
            ..Config::default()
        };

        let runtime = tokio::runtime::Handle::current();
        let download_svc = DownloadService::new(&config, runtime.clone());
        let shared_cache_svc = Arc::new(SharedCacheService::new(None, runtime).await);
        ObjectsActor::new(meta_cache, data_cache, shared_cache_svc, download_svc)
    }

    #[tokio::test]
    async fn test_download_error_cache_server_error() {
        test::setup();

        let server = test::FailingSymbolServer::new();
        let cachedir = tempdir();
        let objects_actor = objects_actor(&cachedir).await;

        let find_object = FindObject {
            filetypes: &[FileType::MachCode],
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
        let result = objects_actor
            .find(find_object.clone())
            .await
            .meta
            .unwrap()
            .handle
            .unwrap_err();
        assert_eq!(
            result,
            CacheError::DownloadError("500 Internal Server Error".into())
        );
        assert_eq!(server.accesses(), 3); // up to 3 tries on failure
        let result = objects_actor
            .find(find_object.clone())
            .await
            .meta
            .unwrap()
            .handle
            .unwrap_err();
        assert_eq!(
            result,
            CacheError::DownloadError("500 Internal Server Error".into())
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
            filetypes: &[FileType::MachCode],
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
        let result = objects_actor
            .find(find_object.clone())
            .await
            .meta
            .unwrap()
            .handle
            .unwrap_err();
        assert_eq!(result, CacheError::NotFound);
        assert_eq!(server.accesses(), 1);
        let result = objects_actor
            .find(find_object.clone())
            .await
            .meta
            .unwrap()
            .handle
            .unwrap_err();
        assert_eq!(result, CacheError::NotFound);
        assert_eq!(server.accesses(), 0);
    }

    #[tokio::test]
    async fn test_download_error_cache_timeout() {
        test::setup();

        let server = test::FailingSymbolServer::new();
        let cachedir = tempdir();
        let objects_actor = objects_actor(&cachedir).await;

        let find_object = FindObject {
            filetypes: &[FileType::MachCode],
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
        // FIXME(swatinem): we are not yet threading `Duration` values through our Caching layer
        let timeout = Duration::ZERO;
        let result = objects_actor
            .find(find_object.clone())
            .await
            .meta
            .unwrap()
            .handle
            .unwrap_err();
        assert_eq!(result, CacheError::Timeout(timeout));
        // XXX: why are we not trying this 3 times?
        assert_eq!(server.accesses(), 1);
        let result = objects_actor
            .find(find_object.clone())
            .await
            .meta
            .unwrap()
            .handle
            .unwrap_err();
        assert_eq!(result, CacheError::Timeout(timeout));
        assert_eq!(server.accesses(), 0);
    }

    #[tokio::test]
    async fn test_download_error_cache_forbidden() {
        test::setup();

        let server = test::FailingSymbolServer::new();
        let cachedir = tempdir();
        let objects_actor = objects_actor(&cachedir).await;

        let find_object = FindObject {
            filetypes: &[FileType::MachCode],
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
        let result = objects_actor
            .find(find_object.clone())
            .await
            .meta
            .unwrap()
            .handle
            .unwrap_err();
        assert_eq!(result, CacheError::PermissionDenied("".into()));
        assert_eq!(server.accesses(), 1);
        let result = objects_actor
            .find(find_object.clone())
            .await
            .meta
            .unwrap()
            .handle
            .unwrap_err();
        assert_eq!(result, CacheError::PermissionDenied("".into()));
        assert_eq!(server.accesses(), 0);
    }
}
