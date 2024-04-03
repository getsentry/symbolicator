//! Data cache for the object actor.
//!
//! This implements a cache holding the content of object files.  It does this by
//! implementing the [`CacheItemRequest`] trait for a [`FetchFileDataRequest`] which can be
//! used with a [`Cacher`] to make a filesystem based cache.
//!
//! [`Cacher`]: crate::caching::Cacher

use std::cmp;
use std::fmt;
use std::io;
use std::sync::Arc;

use futures::future::BoxFuture;
use symbolic::common::SelfCell;
use tempfile::NamedTempFile;

use symbolic::common::ByteView;
use symbolic::debuginfo::{Archive, Object};
use symbolicator_sources::{ObjectId, RemoteFile};

use crate::caches::versions::OBJECTS_CACHE_VERSIONS;
use crate::caching::CacheVersions;
use crate::caching::{CacheEntry, CacheError, CacheItemRequest, CacheKey};
use crate::download::{fetch_file, tempfile_in_parent, DownloadService};
use crate::types::Scope;
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

    pub scope: Scope,

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
#[tracing::instrument(skip(downloader, temp_file), fields(object_id, %file_id))]
async fn fetch_object_file(
    object_id: &ObjectId,
    file_id: RemoteFile,
    downloader: Arc<DownloadService>,
    temp_file: &mut NamedTempFile,
) -> CacheEntry {
    sentry::configure_scope(|scope| {
        file_id.to_scope(scope);
        object_id.to_scope(scope);
    });

    fetch_file(downloader, file_id, temp_file).await?;

    // Since objects in Sentry (and potentially also other sources) might be
    // multi-arch files (e.g. FatMach), we parse as Archive and try to
    // extract the wanted file.
    let view = ByteView::map_file_ref(temp_file.as_file())?;
    let archive = match Archive::parse(&view) {
        Ok(archive) => archive,
        Err(e) => return Err(CacheError::Malformed(e.to_string())),
    };

    if archive.is_multi() {
        let object_opt = archive
            .objects()
            .filter_map(Result::ok)
            .find(|object| object_matches_id(object, object_id));

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

        let mut dst = tempfile_in_parent(temp_file)?;

        io::copy(&mut object.data(), dst.as_file_mut())?;

        std::mem::swap(temp_file, &mut dst);
    } else {
        // Attempt to parse the object to capture errors. The result can be
        // discarded as the object's data is the entire ByteView.
        if let Err(err) = archive.object_by_index(0) {
            return Err(CacheError::Malformed(err.to_string()));
        }
    };

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

    const VERSIONS: CacheVersions = OBJECTS_CACHE_VERSIONS;

    fn compute<'a>(&'a self, temp_file: &'a mut NamedTempFile) -> BoxFuture<'a, CacheEntry> {
        tracing::trace!(
            "Fetching file data for {}",
            CacheKey::from_scoped_file(&self.0.scope, &self.0.file_source)
        );
        Box::pin(fetch_object_file(
            &self.0.object_id,
            self.0.file_source.clone(),
            self.0.download_svc.clone(),
            temp_file,
        ))
    }

    fn load(&self, data: ByteView<'static>) -> CacheEntry<Self::Item> {
        let object = OwnedObject::parse(data)?;
        let object_handle = ObjectHandle {
            object_id: self.0.object_id.clone(),
            object,

            scope: self.0.scope.clone(),
            cache_key: CacheKey::from_scoped_file(&self.0.scope, &self.0.file_source),
        };

        // FIXME(swatinem): This `configure_scope` call happens in a spawned/deduplicated
        // cache request, which means users of the object (such as `write_symcache`) do
        // not observe it directly, hence they need to do their own.
        object_handle.configure_scope();

        Ok(Arc::new(object_handle))
    }

    fn use_shared_cache(&self) -> bool {
        self.0.file_source.worth_using_shared_cache()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use symbolic::common::DebugId;
    use symbolicator_sources::FileType;
    use tempfile::TempDir;

    use crate::caching::{Cache, CacheName};
    use crate::config::{CacheConfig, CacheConfigs, Config};
    use crate::objects::{FindObject, ObjectPurpose, ObjectsActor};
    use crate::test::{self, tempdir};

    use super::*;

    async fn make_objects_actor(tempdir: &TempDir) -> ObjectsActor {
        let config = Config {
            connect_to_reserved_ips: true,
            max_download_timeout: Duration::from_millis(100),
            cache_dir: Some(tempdir.path().to_path_buf()),
            ..Default::default()
        };

        let meta_cache = Cache::from_config(
            CacheName::ObjectMeta,
            &config,
            CacheConfig::from(CacheConfigs::default().derived),
            Default::default(),
            1024,
        )
        .unwrap();

        let data_cache = Cache::from_config(
            CacheName::Objects,
            &config,
            CacheConfig::from(CacheConfigs::default().downloaded),
            Default::default(),
            1024,
        )
        .unwrap();

        let download_svc = DownloadService::new(&config, tokio::runtime::Handle::current());
        ObjectsActor::new(meta_cache, data_cache, Default::default(), download_svc)
    }

    #[tokio::test]
    async fn test_download_error_cache_server_error() {
        test::setup();

        let hitcounter = test::Server::new();
        let cachedir = tempdir();
        let objects_actor = make_objects_actor(&cachedir).await;

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
            sources: Arc::new([hitcounter.source("rejected", "/respond_statuscode/500/")]),
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
        assert_eq!(hitcounter.accesses(), 3); // up to 3 tries on failure

        // NOTE: creating a fresh instance to avoid in-memory cache
        let objects_actor = make_objects_actor(&cachedir).await;
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
        assert_eq!(hitcounter.accesses(), 0);
    }

    #[tokio::test]
    async fn test_negative_cache_not_found() {
        test::setup();

        let hitcounter = test::Server::new();
        let cachedir = tempdir();
        let objects_actor = make_objects_actor(&cachedir).await;

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
            sources: Arc::new([hitcounter.source("notfound", "/respond_statuscode/404/")]),
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
        assert_eq!(hitcounter.accesses(), 1);

        // NOTE: creating a fresh instance to avoid in-memory cache
        let objects_actor = make_objects_actor(&cachedir).await;
        let result = objects_actor
            .find(find_object.clone())
            .await
            .meta
            .unwrap()
            .handle
            .unwrap_err();
        assert_eq!(result, CacheError::NotFound);
        assert_eq!(hitcounter.accesses(), 0);
    }

    #[tokio::test]
    async fn test_download_error_cache_timeout() {
        test::setup();

        let hitcounter = test::Server::new();
        let cachedir = tempdir();
        let objects_actor = make_objects_actor(&cachedir).await;

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
            sources: Arc::new([hitcounter.source("pending", "/delay/1h/")]),
            ..find_object
        };
        let err = CacheError::Timeout(Duration::from_millis(100));
        let result = objects_actor
            .find(find_object.clone())
            .await
            .meta
            .unwrap()
            .handle
            .unwrap_err();
        assert_eq!(result, err);
        // XXX: why are we not trying this 3 times?
        assert_eq!(hitcounter.accesses(), 1);

        // NOTE: creating a fresh instance to avoid in-memory cache
        let objects_actor = make_objects_actor(&cachedir).await;
        let result = objects_actor
            .find(find_object.clone())
            .await
            .meta
            .unwrap()
            .handle
            .unwrap_err();
        assert_eq!(result, err);
        assert_eq!(hitcounter.accesses(), 0);
    }

    #[tokio::test]
    async fn test_download_error_cache_forbidden() {
        test::setup();

        let hitcounter = test::Server::new();
        let cachedir = tempdir();
        let objects_actor = make_objects_actor(&cachedir).await;

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
            sources: Arc::new([hitcounter.source("permissiondenied", "/respond_statuscode/403/")]),
            ..find_object
        };
        let err = CacheError::PermissionDenied("403 Forbidden".into());
        let result = objects_actor
            .find(find_object.clone())
            .await
            .meta
            .unwrap()
            .handle
            .unwrap_err();
        assert_eq!(result, err);
        assert_eq!(hitcounter.accesses(), 1);

        // NOTE: creating a fresh instance to avoid in-memory cache
        let objects_actor = make_objects_actor(&cachedir).await;
        let result = objects_actor
            .find(find_object.clone())
            .await
            .meta
            .unwrap()
            .handle
            .unwrap_err();
        assert_eq!(result, err);
        assert_eq!(hitcounter.accesses(), 0);
    }
}
