use std::fs::File;
use std::io::{self, BufWriter};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use failure::{Fail, ResultExt};
use futures::Future;
use sentry::configure_scope;
use sentry::integrations::failure::capture_fail;
use symbolic::common::{Arch, ByteView};
use symbolic::symcache::{self, SymCache, SymCacheWriter};
use tokio_threadpool::ThreadPool;

use crate::actors::common::cache::{CacheItemRequest, Cacher};
use crate::actors::objects::{FetchObject, ObjectFile, ObjectPurpose, ObjectsActor};
use crate::cache::{Cache, CacheKey, CacheStatus};
use crate::sentry::{SentryFutureExt, WriteSentryScope};
use crate::types::{FileType, ObjectId, ObjectType, Scope, SourceConfig};

#[derive(Fail, Debug, Clone, Copy)]
pub enum SymCacheErrorKind {
    #[fail(display = "failed to write symcache")]
    Io,

    #[fail(display = "failed to download object")]
    Fetching,

    #[fail(display = "failed sending message to objects actor")]
    Mailbox,

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

pub struct SymCacheActor {
    symcaches: Arc<Cacher<FetchSymCacheInternal>>,
    objects: Arc<ObjectsActor>,
    threadpool: Arc<ThreadPool>,
}

impl SymCacheActor {
    pub fn new(cache: Cache, objects: Arc<ObjectsActor>, threadpool: Arc<ThreadPool>) -> Self {
        SymCacheActor {
            symcaches: Arc::new(Cacher::new(cache)),
            objects,
            threadpool,
        }
    }
}

#[derive(Clone)]
pub struct SymCacheFile {
    object_type: ObjectType,
    identifier: ObjectId,
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

#[derive(Clone)]
struct FetchSymCacheInternal {
    request: FetchSymCache,
    objects: Arc<ObjectsActor>,
    threadpool: Arc<ThreadPool>,
}

impl CacheItemRequest for FetchSymCacheInternal {
    type Item = SymCacheFile;
    type Error = SymCacheError;

    fn get_cache_key(&self) -> CacheKey {
        unimplemented!()
    }

    fn get_lookup_cache_keys(&self) -> Vec<CacheKey> {
        self.request
            .sources
            .iter()
            .map(|source: &SourceConfig| CacheKey {
                cache_key: format!("{}.{}", source.id(), self.request.identifier.cache_key()),
                scope: self.request.scope.clone(),
            })
            .collect()
    }

    fn compute(
        &self,
        path: &Path,
    ) -> Box<dyn Future<Item = (CacheStatus, Option<CacheKey>), Error = Self::Error>> {
        let objects = self.objects.clone();

        let path = path.to_owned();
        let threadpool = self.threadpool.clone();

        let result = objects
            .fetch(FetchObject {
                filetypes: FileType::from_object_type(&self.request.object_type),
                identifier: self.request.identifier.clone(),
                sources: self.request.sources.clone(),
                scope: self.request.scope.clone(),
                purpose: ObjectPurpose::Debug,
            })
            .map_err(|e| SymCacheError::from(e.context(SymCacheErrorKind::Fetching)))
            .and_then(move |object| {
                threadpool.spawn_handle(
                    futures::lazy(move || {
                        let new_cache_key = if let Some(ref source) = object.source() {
                            Some(CacheKey {
                                cache_key: format!(
                                    "{}.{}",
                                    source.id(),
                                    object.object_id().cache_key()
                                ),
                                scope: object.scope().clone(),
                            })
                        } else {
                            None
                        };

                        if object.status() != CacheStatus::Positive {
                            return Ok((object.status(), new_cache_key));
                        }

                        let status = if let Err(e) = write_symcache(&path, &*object) {
                            log::warn!("Failed to write symcache: {}", e);
                            capture_fail(e.cause().unwrap_or(&e));

                            CacheStatus::Malformed
                        } else {
                            CacheStatus::Positive
                        };

                        Ok((status, new_cache_key))
                    })
                    .sentry_hub_current(),
                )
            })
            .sentry_hub_current();

        let num_sources = self.request.sources.len();

        Box::new(future_metrics!(
            "symcaches",
            Some((Duration::from_secs(1200), SymCacheErrorKind::Timeout.into())),
            result,
            "num_sources" => &num_sources.to_string()
        ))
    }

    fn should_load(&self, data: &[u8]) -> bool {
        SymCache::parse(data)
            .map(|symcache| symcache.is_latest())
            .unwrap_or(false)
    }

    fn load(
        &self,
        _cache_key: Option<CacheKey>,
        status: CacheStatus,
        data: ByteView<'static>,
    ) -> Self::Item {
        // TODO: Figure out if this double-parsing could be avoided
        let arch = SymCache::parse(&data)
            .map(|cache| cache.arch())
            .unwrap_or_default();

        SymCacheFile {
            object_type: self.request.object_type.clone(),
            identifier: self.request.identifier.clone(),
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
        self.symcaches.compute_memoized(FetchSymCacheInternal {
            request,
            objects: self.objects.clone(),
            threadpool: self.threadpool.clone(),
        })
    }
}

fn write_symcache(path: &Path, object: &ObjectFile) -> Result<(), SymCacheError> {
    configure_scope(|scope| {
        scope.set_transaction(Some("compute_symcache"));
        object.write_sentry_scope(scope);
    });

    let symbolic_object = object
        .parse()
        .context(SymCacheErrorKind::ObjectParsing)?
        .unwrap();

    let file = File::create(&path).context(SymCacheErrorKind::Io)?;
    let mut writer = BufWriter::new(file);

    if let Some(cache_key) = object.cache_key() {
        log::debug!("Converting symcache for {}", cache_key);
    }

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
