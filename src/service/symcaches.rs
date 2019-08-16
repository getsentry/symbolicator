use std::fs::File;
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use failure::{Fail, ResultExt};
use futures::{future, future::Either, Future};
use sentry::configure_scope;
use sentry::integrations::failure::capture_fail;
use symbolic::common::{Arch, ByteView};
use symbolic::symcache::{self, SymCache, SymCacheWriter};

use crate::cache::{Cache, CacheKey, CacheStatus};
use crate::service::cache::{CacheItemRequest, Cacher};
use crate::service::objects::{
    FindObject, ObjectFile, ObjectFileMeta, ObjectPurpose, ObjectsActor,
};
use crate::types::{FileType, ObjectId, ObjectType, Scope, SourceConfig};
use crate::utils::helpers::FutureExt;
use crate::utils::sentry::ToSentryScope;
use crate::utils::threadpool::ThreadPool;

#[derive(Fail, Debug, Clone, Copy)]
pub enum SymCacheErrorKind {
    #[fail(display = "failed to write symcache")]
    Io,

    #[fail(display = "failed to download object")]
    Fetching,

    #[fail(display = "failed to parse symcache")]
    Parsing,

    #[fail(display = "failed to parse object")]
    ObjectParsing,

    #[fail(display = "symcache building took too long")]
    Timeout,
}

symbolic::common::derive_failure!(
    SymCacheError,
    SymCacheErrorKind,
    doc = "Errors happening while generating a symcache"
);

impl From<io::Error> for SymCacheError {
    fn from(e: io::Error) -> Self {
        e.context(SymCacheErrorKind::Io).into()
    }
}

#[derive(Debug)]
pub struct SymCacheActor {
    symcaches: Arc<Cacher<FetchSymCacheInternal>>,
    objects: Arc<ObjectsActor>,
    threadpool: ThreadPool,
}

impl SymCacheActor {
    pub fn new(cache: Cache, objects: Arc<ObjectsActor>, threadpool: ThreadPool) -> Self {
        SymCacheActor {
            symcaches: Arc::new(Cacher::new(cache)),
            objects,
            threadpool,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SymCacheFile {
    object_type: ObjectType,
    identifier: ObjectId,
    scope: Scope,
    data: ByteView<'static>,
    status: CacheStatus,
    arch: Arch,
}

impl SymCacheFile {
    pub fn parse(&self) -> Result<Option<SymCache<'_>>, SymCacheError> {
        match self.status {
            CacheStatus::Negative => Ok(None),
            CacheStatus::Malformed => Err(SymCacheErrorKind::ObjectParsing.into()),
            CacheStatus::Positive => Ok(Some(
                SymCache::parse(&self.data).context(SymCacheErrorKind::Parsing)?,
            )),
        }
    }

    /// Returns the architecture of this symcache.
    pub fn arch(&self) -> Arch {
        self.arch
    }
}

fn compute_symcache(
    object: Arc<ObjectFile>,
    path: PathBuf,
    threadpool: &ThreadPool,
) -> Box<dyn Future<Item = CacheStatus, Error = SymCacheError>> {
    if object.status() != CacheStatus::Positive {
        return Box::new(future::ok(object.status()));
    }

    let compute_future = futures::lazy(move || -> Result<CacheStatus, SymCacheError> {
        let status = if let Err(e) = write_symcache(&path, &*object) {
            log::warn!("Failed to write symcache: {}", e);
            capture_fail(e.cause().unwrap_or(&e));
            CacheStatus::Malformed
        } else {
            CacheStatus::Positive
        };

        Ok(status)
    });

    let bounded_future = compute_future
        .timeout(Duration::from_secs(300), || SymCacheErrorKind::Timeout)
        .measure("compute_symcache")
        .map_err(|e| {
            capture_fail(e.cause().unwrap_or(&e));
            e
        })
        .spawn_on(threadpool);

    Box::new(bounded_future)
}

#[derive(Clone, Debug)]
struct FetchSymCacheInternal {
    request: FetchSymCache,
    objects_actor: Arc<ObjectsActor>,
    object_meta: Arc<ObjectFileMeta>,
    threadpool: ThreadPool,
}

impl CacheItemRequest for FetchSymCacheInternal {
    type Item = SymCacheFile;
    type Error = SymCacheError;

    fn get_cache_key(&self) -> CacheKey {
        self.object_meta.cache_key().clone()
    }

    fn compute(&self, path: &Path) -> Box<dyn Future<Item = CacheStatus, Error = Self::Error>> {
        let path = path.to_owned();
        let threadpool = self.threadpool.clone();

        let future = self
            .objects_actor
            .fetch(self.object_meta.clone())
            .map_err(|e| SymCacheError::from(e.context(SymCacheErrorKind::Fetching)))
            .and_then(move |object| compute_symcache(object, path, &threadpool))
            .measure("symcaches");

        Box::new(future)
    }

    fn should_load(&self, data: &[u8]) -> bool {
        SymCache::parse(data)
            .map(|symcache| symcache.is_latest())
            .unwrap_or(false)
    }

    fn load(&self, scope: Scope, status: CacheStatus, data: ByteView<'static>) -> Self::Item {
        // TODO: Figure out if this double-parsing could be avoided
        let arch = SymCache::parse(&data)
            .map(|cache| cache.arch())
            .unwrap_or_default();

        SymCacheFile {
            object_type: self.request.object_type.clone(),
            identifier: self.request.identifier.clone(),
            scope,
            data,
            status,
            arch,
        }
    }
}

/// Information for fetching the symbols for this symcache
#[derive(Debug, Clone)]
pub struct FetchSymCache {
    pub object_type: ObjectType,
    pub identifier: ObjectId,
    pub sources: Arc<Vec<SourceConfig>>,
    pub scope: Scope,
}

impl SymCacheActor {
    pub fn fetch(
        &self,
        request: FetchSymCache,
    ) -> impl Future<Item = Arc<SymCacheFile>, Error = Arc<SymCacheError>> {
        let object = self
            .objects
            .find(FindObject {
                filetypes: FileType::from_object_type(&request.object_type),
                identifier: request.identifier.clone(),
                sources: request.sources.clone(),
                scope: request.scope.clone(),
                purpose: ObjectPurpose::Debug,
            })
            .map_err(|e| Arc::new(SymCacheError::from(e.context(SymCacheErrorKind::Fetching))));

        let symcaches = self.symcaches.clone();
        let threadpool = self.threadpool.clone();
        let objects = self.objects.clone();

        let object_type = request.object_type.clone();
        let identifier = request.identifier.clone();
        let scope = request.scope.clone();

        object.and_then(move |object| {
            object
                .map(move |object| {
                    Either::A(symcaches.compute_memoized(FetchSymCacheInternal {
                        request,
                        objects_actor: objects,
                        object_meta: object,
                        threadpool,
                    }))
                })
                .unwrap_or_else(move || {
                    Either::B(future::ok(Arc::new(SymCacheFile {
                        object_type,
                        identifier,
                        scope,
                        data: ByteView::from_slice(b""),
                        status: CacheStatus::Negative,
                        arch: Arch::Unknown,
                    })))
                })
        })
    }
}

fn write_symcache(path: &Path, object: &ObjectFile) -> Result<(), SymCacheError> {
    configure_scope(|scope| {
        scope.set_transaction(Some("compute_symcache"));
        object.to_scope(scope);
    });

    let symbolic_object = object
        .parse()
        .context(SymCacheErrorKind::ObjectParsing)?
        .unwrap();

    let file = File::create(&path).context(SymCacheErrorKind::Io)?;
    let mut writer = BufWriter::new(file);

    log::debug!("Converting symcache for {}", object.cache_key());

    if let Err(e) = SymCacheWriter::write_object(&symbolic_object, &mut writer) {
        match e.kind() {
            symcache::SymCacheErrorKind::WriteFailed => {
                return Err(e.context(SymCacheErrorKind::Io).into())
            }
            _ => return Err(e.context(SymCacheErrorKind::ObjectParsing).into()),
        }
    }

    let file = writer.into_inner().context(SymCacheErrorKind::Io)?;
    file.sync_all().context(SymCacheErrorKind::Io)?;

    Ok(())
}
