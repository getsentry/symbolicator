use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use actix::{Actor, Addr, Context, Handler, Message, ResponseFuture};
use failure::{Fail, ResultExt};
use futures::Future;
use sentry::integrations::failure::capture_fail;
use symbolic::{common::ByteView, minidump};
use tokio_threadpool::ThreadPool;

use crate::actors::cache::{CacheActor, CacheItemRequest, ComputeMemoized};
use crate::actors::objects::{FetchObject, ObjectPurpose, ObjectsActor};
use crate::cache::{Cache, CacheKey, MALFORMED_MARKER};
use crate::sentry::SentryFutureExt;
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
    cficaches: Addr<CacheActor<FetchCfiCacheInternal>>,
    objects: Addr<ObjectsActor>,
    threadpool: Arc<ThreadPool>,
}

impl Actor for CfiCacheActor {
    type Context = Context<Self>;
}

impl CfiCacheActor {
    pub fn new(cache: Cache, objects: Addr<ObjectsActor>, threadpool: Arc<ThreadPool>) -> Self {
        CfiCacheActor {
            cficaches: CacheActor::new(cache).start(),
            objects,
            threadpool,
        }
    }
}

#[derive(Clone)]
pub struct CfiCacheFile {
    inner: Option<ByteView<'static>>,
    scope: Scope,
    request: FetchCfiCacheInternal,
}

impl CfiCacheFile {
    pub fn parse(&self) -> Result<Option<minidump::cfi::CfiCache<'_>>, CfiCacheError> {
        let bytes = match self.inner {
            Some(ref x) => x,
            None => return Ok(None),
        };

        if &bytes[..] == MALFORMED_MARKER {
            return Err(CfiCacheErrorKind::ObjectParsing.into());
        }

        Ok(Some(
            minidump::cfi::CfiCache::from_bytes(bytes.clone())
                .context(CfiCacheErrorKind::Parsing)?,
        ))
    }
}

#[derive(Clone)]
struct FetchCfiCacheInternal {
    request: FetchCfiCache,
    objects: Addr<ObjectsActor>,
    threadpool: Arc<ThreadPool>,
}

impl CacheItemRequest for FetchCfiCacheInternal {
    type Item = CfiCacheFile;
    type Error = CfiCacheError;

    fn get_cache_key(&self) -> CacheKey {
        CacheKey {
            cache_key: self.request.identifier.cache_key(),
            scope: self.request.scope.clone(),
        }
    }

    fn compute(&self, path: &Path) -> Box<dyn Future<Item = Scope, Error = Self::Error>> {
        let objects = self.objects.clone();

        let path = path.to_owned();
        let threadpool = self.threadpool.clone();

        // TODO: Backoff + retry when download is interrupted? Or should we just have retry logic
        // in Sentry itself?
        let result = objects
            .send(
                FetchObject {
                    filetypes: FileType::from_object_type(&self.request.object_type),
                    identifier: self.request.identifier.clone(),
                    sources: self.request.sources.clone(),
                    scope: self.request.scope.clone(),
                    purpose: ObjectPurpose::Unwind,
                }
                .sentry_hub_new_from_current(),
            )
            .map_err(|e| e.context(CfiCacheErrorKind::Mailbox).into())
            .and_then(clone!(path, |result| {
                threadpool.spawn_handle(futures::lazy(move || {
                    let object = result.context(CfiCacheErrorKind::Fetching)?;
                    let file = BufWriter::new(File::create(&path).context(CfiCacheErrorKind::Io)?);
                    if let Some(object) =
                        object.parse().context(CfiCacheErrorKind::ObjectParsing)?
                    {
                        minidump::cfi::CfiCache::from_object(&object)
                            .context(CfiCacheErrorKind::ObjectParsing)?
                            .write_to(file)
                            .context(CfiCacheErrorKind::Io)?;
                    }

                    Ok(object.scope().clone())
                }))
            }))
            .map_err(|e: CfiCacheError| {
                capture_fail(e.cause().unwrap_or(&e));
                e
            })
            .or_else(clone!(path, |e| {
                log::warn!("Could not write cficache: {}", e);
                let mut file = File::create(&path).context(CfiCacheErrorKind::Io)?;

                file.write_all(MALFORMED_MARKER)
                    .context(CfiCacheErrorKind::Io)?;

                file.sync_all().context(CfiCacheErrorKind::Io)?;
                Err(e)
            }));

        let num_sources = self.request.sources.len();

        Box::new(future_metrics!(
            "cficaches",
            Some((Duration::from_secs(1200), CfiCacheErrorKind::Timeout.into())),
            result,
            "num_sources" => &num_sources.to_string()
        ))
    }

    fn load(self, scope: Scope, data: ByteView<'static>) -> Result<Self::Item, Self::Error> {
        Ok(CfiCacheFile {
            request: self,
            scope,
            inner: if !data.is_empty() { Some(data) } else { None },
        })
    }
}

/// Information for fetching the symbols for this cficache
#[derive(Debug, Clone)]
pub struct FetchCfiCache {
    pub object_type: ObjectType,
    pub identifier: ObjectId,
    pub sources: Vec<SourceConfig>,
    pub scope: Scope,
}

impl Message for FetchCfiCache {
    type Result = Result<Arc<CfiCacheFile>, Arc<CfiCacheError>>;
}

impl Handler<FetchCfiCache> for CfiCacheActor {
    type Result = ResponseFuture<Arc<CfiCacheFile>, Arc<CfiCacheError>>;

    fn handle(&mut self, request: FetchCfiCache, _ctx: &mut Self::Context) -> Self::Result {
        Box::new(
            self.cficaches
                .send(
                    ComputeMemoized(FetchCfiCacheInternal {
                        request,
                        objects: self.objects.clone(),
                        threadpool: self.threadpool.clone(),
                    })
                    .sentry_hub_new_from_current(),
                )
                .map_err(|e| Arc::new(e.context(CfiCacheErrorKind::Mailbox).into()))
                .and_then(|response| Ok(response?)),
        )
    }
}

handle_sentry_actix_message!(CfiCacheActor, FetchCfiCache);
