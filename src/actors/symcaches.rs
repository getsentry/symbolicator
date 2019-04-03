use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use actix::{Actor, Addr, Context, Handler, Message, ResponseFuture};
use failure::{Fail, ResultExt};
use futures::Future;
use symbolic::common::{Arch, ByteView};
use symbolic::symcache::{SymCache, SymCacheWriter};
use tokio_threadpool::ThreadPool;

use crate::actors::cache::{CacheActor, CacheItemRequest, CacheKey, ComputeMemoized};
use crate::actors::objects::{FetchObject, ObjectsActor};
use crate::futures::measure_task;
use crate::types::{FileType, ObjectId, ObjectType, Scope, SourceConfig};

#[derive(Fail, Debug, Clone, Copy)]
pub enum SymCacheErrorKind {
    #[fail(display = "failed to download")]
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
        let mut cache_key = self.request.identifier.cache_key();

        for source in &self.request.sources {
            cache_key.push_str(&format!(".s:{}", source.id()));
        }

        CacheKey {
            cache_key,
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
            })
            .map_err(|e| e.context(SymCacheErrorKind::Mailbox).into())
            .and_then(move |result| {
                threadpool.spawn_handle(futures::lazy(move || {
                    let object = result.context(SymCacheErrorKind::Fetching)?;
                    let mut file =
                        BufWriter::new(File::create(&path).context(SymCacheErrorKind::Io)?);
                    match object.parse() {
                        Ok(Some(object)) => {
                            SymCacheWriter::write_object(&object, file)
                                .context(SymCacheErrorKind::Io)?;
                        }
                        Ok(None) => (),
                        Err(err) => {
                            log::warn!("Could not parse object: {}", err);
                            file.write_all(b"malformed")
                                .context(SymCacheErrorKind::Io)?;
                        }
                    };

                    Ok(object.scope().clone())
                }))
            });

        Box::new(measure_task(
            "fetch_symcache",
            Some((Duration::from_secs(1200), SymCacheErrorKind::Timeout.into())),
            result,
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
                .send(ComputeMemoized(FetchSymCacheInternal {
                    request,
                    objects: self.objects.clone(),
                    threadpool: self.threadpool.clone(),
                }))
                .map_err(|e| Arc::new(e.context(SymCacheErrorKind::Mailbox).into()))
                .and_then(|response| Ok(response?)),
        )
    }
}
