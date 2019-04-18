use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use actix::{Actor, Addr, Context, Handler, Message, ResponseFuture};
use failure::{Fail, ResultExt};
use futures::Future;
use sentry::integrations::failure::capture_fail;
use symbolic::common::{Arch, ByteView};
use symbolic::symcache::{self, SymCache, SymCacheWriter};
use tokio_threadpool::ThreadPool;

use crate::actors::cache::{CacheActor, CacheItemRequest, CacheKey, ComputeMemoized};
use crate::actors::objects::{FetchObject, ObjectPurpose, ObjectsActor};
use crate::sentry::SentryFutureExt;
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
    symcaches: Addr<CacheActor<FetchSymCacheInternal>>,
    objects: Addr<ObjectsActor>,
    threadpool: Arc<ThreadPool>,
}

impl Actor for SymCacheActor {
    type Context = Context<Self>;
}

impl SymCacheActor {
    pub fn new(
        symcaches: Addr<CacheActor<FetchSymCacheInternal>>,
        objects: Addr<ObjectsActor>,
        threadpool: Arc<ThreadPool>,
    ) -> Self {
        SymCacheActor {
            symcaches,
            objects,
            threadpool,
        }
    }
}

#[derive(Clone)]
pub struct SymCacheFile {
    inner: Option<ByteView<'static>>,
    scope: Scope,
    request: FetchSymCacheInternal,
    arch: Arch,
}

impl SymCacheFile {
    pub fn parse(&self) -> Result<Option<SymCache<'_>>, SymCacheError> {
        let bytes = match self.inner {
            Some(ref x) => x,
            None => return Ok(None),
        };

        if &bytes[..] == b"malformed" {
            return Err(SymCacheErrorKind::ObjectParsing.into());
        }

        Ok(Some(
            SymCache::parse(bytes).context(SymCacheErrorKind::Parsing)?,
        ))
    }

    /// Returns the architecture of this symcache.
    pub fn arch(&self) -> Arch {
        self.arch
    }
}

#[derive(Clone)]
pub struct FetchSymCacheInternal {
    request: FetchSymCache,
    objects: Addr<ObjectsActor>,
    threadpool: Arc<ThreadPool>,
}

impl CacheItemRequest for FetchSymCacheInternal {
    type Item = SymCacheFile;
    type Error = SymCacheError;

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
            .send(FetchObject {
                filetypes: FileType::from_object_type(&self.request.object_type),
                identifier: self.request.identifier.clone(),
                sources: self.request.sources.clone(),
                scope: self.request.scope.clone(),
                purpose: ObjectPurpose::Debug,
            })
            .map_err(|e| e.context(SymCacheErrorKind::Mailbox).into())
            .and_then(clone!(path, |result| {
                threadpool.spawn_handle(futures::lazy(move || {
                    let object = result.context(SymCacheErrorKind::Fetching)?;
                    let file = File::create(&path).context(SymCacheErrorKind::Io)?;
                    let symbolic_object =
                        match object.parse().context(SymCacheErrorKind::ObjectParsing)? {
                            Some(object) => object,
                            None => return Ok(object.scope().clone()),
                        };

                    let mut writer = BufWriter::new(file);

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

                    let metadata = file.metadata().context(SymCacheErrorKind::Io)?;
                    metric!(time_raw("symcaches.size") = metadata.len());

                    Ok(object.scope().clone())
                }))
            }))
            .map_err(|e: SymCacheError| {
                capture_fail(e.cause().unwrap_or(&e));
                e
            })
            .or_else(clone!(path, |e| {
                log::warn!("Could not write symcache: {}", e);
                let mut file = File::create(&path).context(SymCacheErrorKind::Io)?;

                file.write_all(b"malformed")
                    .context(SymCacheErrorKind::Io)?;

                file.sync_all().context(SymCacheErrorKind::Io)?;
                Err(e)
            }));

        let num_sources = self.request.sources.len();

        Box::new(future_metrics!(
            "symcaches",
            Some((Duration::from_secs(1200), SymCacheErrorKind::Timeout.into())),
            result,
            "num_sources" => &num_sources.to_string()
        ))
    }

    fn load(self, scope: Scope, data: ByteView<'static>) -> Result<Self::Item, Self::Error> {
        // TODO: Figure out if this double-parsing could be avoided
        let arch = SymCache::parse(&data)
            .map(|cache| cache.arch())
            .unwrap_or_default();

        Ok(SymCacheFile {
            request: self,
            scope,
            inner: if !data.is_empty() { Some(data) } else { None },
            arch,
        })
    }
}

/// Information for fetching the symbols for this symcache
#[derive(Debug, Clone)]
pub struct FetchSymCache {
    pub object_type: ObjectType,
    pub identifier: ObjectId,
    pub sources: Vec<SourceConfig>,
    pub scope: Scope,
}

impl Message for FetchSymCache {
    type Result = Result<Arc<SymCacheFile>, Arc<SymCacheError>>;
}

impl Handler<FetchSymCache> for SymCacheActor {
    type Result = ResponseFuture<Arc<SymCacheFile>, Arc<SymCacheError>>;

    fn handle(&mut self, request: FetchSymCache, _ctx: &mut Self::Context) -> Self::Result {
        Box::new(
            self.symcaches
                .send(
                    ComputeMemoized(FetchSymCacheInternal {
                        request,
                        objects: self.objects.clone(),
                        threadpool: self.threadpool.clone(),
                    })
                    .sentry_hub_new_from_current(),
                )
                .map_err(|e| Arc::new(e.context(SymCacheErrorKind::Mailbox).into()))
                .and_then(|response| Ok(response?)),
        )
    }
}

handle_sentry_actix_message!(SymCacheActor, FetchSymCache);
