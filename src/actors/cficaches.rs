use std::fs::File;
use std::io::{self, BufWriter};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use failure::{Fail, ResultExt};
use futures::{
    future::{Either, IntoFuture},
    Future,
};
use sentry::configure_scope;
use sentry::integrations::failure::capture_fail;
use symbolic::{common::ByteView, minidump::cfi::CfiCache};
use tokio_threadpool::ThreadPool;

use crate::actors::common::cache::{CacheItemRequest, Cacher};
use crate::actors::objects::{FetchObject, ObjectFile, ObjectPurpose, ObjectsActor};
use crate::cache::{Cache, CacheKey, CacheStatus};
use crate::sentry::{SentryFutureExt, WriteSentryScope};
use crate::types::{FileType, ObjectId, ObjectType, Scope, SourceConfig};

#[derive(Fail, Debug, Clone, Copy)]
pub enum CfiCacheErrorKind {
    #[fail(display = "failed to download")]
    Io,

    #[fail(display = "failed to download object")]
    Fetching,

    #[fail(display = "failed sending message to objects actor")]
    Mailbox,

    #[fail(display = "failed to parse cficache")]
    Parsing,

    #[fail(display = "failed to parse object")]
    ObjectParsing,

    #[fail(display = "cficache building took too long")]
    Timeout,
}

symbolic::common::derive_failure!(
    CfiCacheError,
    CfiCacheErrorKind,
    doc = "Errors happening while generating a cficache"
);

impl From<io::Error> for CfiCacheError {
    fn from(e: io::Error) -> Self {
        e.context(CfiCacheErrorKind::Io).into()
    }
}

pub struct CfiCacheActor {
    cficaches: Arc<Cacher<FetchCfiCacheInternal>>,
    objects: Arc<ObjectsActor>,
    threadpool: Arc<ThreadPool>,
}

impl CfiCacheActor {
    pub fn new(cache: Cache, objects: Arc<ObjectsActor>, threadpool: Arc<ThreadPool>) -> Self {
        CfiCacheActor {
            cficaches: Arc::new(Cacher::new(cache)),
            objects,
            threadpool,
        }
    }
}

#[derive(Clone)]
pub struct CfiCacheFile {
    object_type: ObjectType,
    identifier: ObjectId,
    scope: Scope,
    data: ByteView<'static>,
    status: CacheStatus,
}

impl CfiCacheFile {
    pub fn parse(&self) -> Result<Option<CfiCache<'_>>, CfiCacheError> {
        match self.status {
            CacheStatus::Negative => Ok(None),
            CacheStatus::Malformed => Err(CfiCacheErrorKind::ObjectParsing.into()),
            CacheStatus::Positive => Ok(Some(
                CfiCache::from_bytes(self.data.clone()).context(CfiCacheErrorKind::Parsing)?,
            )),
        }
    }
}

#[derive(Clone)]
struct FetchCfiCacheInternal {
    request: FetchCfiCache,
    object: Arc<ObjectFile>,
    threadpool: Arc<ThreadPool>,
}

impl CacheItemRequest for FetchCfiCacheInternal {
    type Item = CfiCacheFile;
    type Error = CfiCacheError;

    fn get_cache_key(&self) -> CacheKey {
        self.object.cache_key().clone()
    }

    fn compute(
        &self,
        path: &Path,
    ) -> Box<dyn Future<Item = (CacheStatus, Scope), Error = Self::Error>> {
        let object = self.object.clone();

        let path = path.to_owned();
        let threadpool = self.threadpool.clone();

        let result = threadpool.spawn_handle(
            futures::lazy(move || {
                if object.status() != CacheStatus::Positive {
                    return Ok((object.status(), object.scope().clone()));
                }

                let status = if let Err(e) = write_cficache(&path, &*object) {
                    log::warn!("Could not write cficache: {}", e);
                    capture_fail(e.cause().unwrap_or(&e));

                    CacheStatus::Malformed
                } else {
                    CacheStatus::Positive
                };

                Ok((status, object.scope().clone()))
            })
            .sentry_hub_current(),
        );

        let num_sources = self.request.sources.len();

        Box::new(future_metrics!(
            "cficaches",
            Some((Duration::from_secs(1200), CfiCacheErrorKind::Timeout.into())),
            result,
            "num_sources" => &num_sources.to_string()
        ))
    }

    fn should_load(&self, data: &[u8]) -> bool {
        CfiCache::from_bytes(ByteView::from_slice(data))
            .map(|cficache| cficache.is_latest())
            .unwrap_or(false)
    }

    fn load(&self, scope: Scope, status: CacheStatus, data: ByteView<'static>) -> Self::Item {
        CfiCacheFile {
            object_type: self.request.object_type.clone(),
            identifier: self.request.identifier.clone(),
            scope,
            data,
            status,
        }
    }
}

/// Information for fetching the symbols for this cficache
#[derive(Debug, Clone)]
pub struct FetchCfiCache {
    pub object_type: ObjectType,
    pub identifier: ObjectId,
    pub sources: Arc<Vec<SourceConfig>>,
    pub scope: Scope,
}

impl CfiCacheActor {
    pub fn fetch(
        &self,
        request: FetchCfiCache,
    ) -> impl Future<Item = Arc<CfiCacheFile>, Error = Arc<CfiCacheError>> {
        let object = self
            .objects
            .fetch(FetchObject {
                filetypes: FileType::from_object_type(&request.object_type),
                identifier: request.identifier.clone(),
                sources: request.sources.clone(),
                scope: request.scope.clone(),
                purpose: ObjectPurpose::Unwind,
            })
            .map_err(|e| Arc::new(CfiCacheError::from(e.context(CfiCacheErrorKind::Fetching))));

        let cficaches = self.cficaches.clone();
        let threadpool = self.threadpool.clone();

        let object_type = request.object_type.clone();
        let identifier = request.identifier.clone();
        let scope = request.scope.clone();

        let cficache = object.and_then(move |object| {
            object
                .map(|object| {
                    Either::A(cficaches.compute_memoized(FetchCfiCacheInternal {
                        request,
                        object,
                        threadpool,
                    }))
                })
                .unwrap_or_else(move || {
                    Either::B(
                        Ok(Arc::new(CfiCacheFile {
                            object_type,
                            identifier,
                            scope,
                            data: ByteView::from_slice(b""),
                            status: CacheStatus::Negative,
                        }))
                        .into_future(),
                    )
                })
        });

        cficache
    }
}

fn write_cficache(path: &Path, object_file: &ObjectFile) -> Result<(), CfiCacheError> {
    configure_scope(|scope| {
        scope.set_transaction(Some("compute_cficache"));
        object_file.write_sentry_scope(scope);
    });

    let object = object_file
        .parse()
        .context(CfiCacheErrorKind::ObjectParsing)?
        .unwrap();

    let file = File::create(&path).context(CfiCacheErrorKind::Io)?;
    let writer = BufWriter::new(file);

    log::debug!("Converting cficache for {}", object_file.cache_key());

    CfiCache::from_object(&object)
        .context(CfiCacheErrorKind::ObjectParsing)?
        .write_to(writer)
        .context(CfiCacheErrorKind::Io)?;

    Ok(())
}
