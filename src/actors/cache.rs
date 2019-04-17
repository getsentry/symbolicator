use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use actix::fut::{ActorFuture, WrapFuture};
use actix::{Actor, AsyncContext, Context, Handler, Message, ResponseActFuture};
use futures::future::{Future, IntoFuture, Shared, SharedError};
use futures::sync::oneshot;
use symbolic::common::ByteView;
use tempfile::NamedTempFile;

use crate::types::Scope;

// Inner result necessary because `futures::Shared` won't give us `Arc`s but its own custom
// newtype around it.
type ComputationChannel<T, E> = Shared<oneshot::Receiver<Result<Arc<T>, Arc<E>>>>;

/// Manages a filesystem cache of any kind of data that can be serialized into bytes and read from
/// it:
///
/// - Object files
/// - Symcaches
/// - CFI caches (soon)
///
/// Handles a single message `ComputeMemoized` that transparently performs cache lookups,
/// downloads and cache stores via the `CacheItemRequest` trait and associated types.
///
/// Internally deduplicates concurrent cache lookups (in-memory).
#[derive(Clone)]
pub struct CacheActor<T: CacheItemRequest> {
    /// Cache identifier used for metric names.
    name: &'static str,

    /// Directory to use for storing cache items. Assumed to exist.
    cache_dir: Option<PathBuf>,

    /// Used for deduplicating cache lookups.
    current_computations: BTreeMap<CacheKey, ComputationChannel<T::Item, T::Error>>,
}

impl<T: CacheItemRequest> CacheActor<T> {
    pub fn new<P: AsRef<Path>>(name: &'static str, cache_dir: Option<P>) -> Self {
        CacheActor {
            name,
            cache_dir: cache_dir.map(|x| x.as_ref().to_owned()),
            current_computations: BTreeMap::new(),
        }
    }
}

impl<T: CacheItemRequest> Actor for CacheActor<T> {
    type Context = Context<Self>;
}

#[derive(Debug, Clone, Eq, Ord, PartialEq, PartialOrd)]
pub struct CacheKey {
    pub cache_key: String,
    pub scope: Scope,
}

impl fmt::Display for CacheKey {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} (scope {})", self.cache_key, self.scope)
    }
}

pub trait CacheItemRequest: 'static + Send {
    type Item: 'static + Send;

    // XXX: Probably should have our own concrete error type for cacheactor instead of forcing our
    // ioerrors into other errors
    type Error: 'static + From<io::Error> + Send;

    fn get_cache_key(&self) -> CacheKey;
    fn compute(&self, path: &Path) -> Box<dyn Future<Item = Scope, Error = Self::Error>>;
    fn load(self, scope: Scope, data: ByteView<'static>) -> Result<Self::Item, Self::Error>;
}

pub struct ComputeMemoized<T>(pub T);

impl<T: CacheItemRequest> Message for ComputeMemoized<T> {
    type Result = Result<Arc<T::Item>, Arc<T::Error>>;
}

impl<T: CacheItemRequest> Handler<ComputeMemoized<T>> for CacheActor<T> {
    type Result = ResponseActFuture<Self, Arc<T::Item>, Arc<T::Error>>;

    fn handle(&mut self, request: ComputeMemoized<T>, ctx: &mut Self::Context) -> Self::Result {
        let key = request.0.get_cache_key();
        let name = self.name;

        let channel = if let Some(channel) = self.current_computations.get(&key) {
            // A concurrent cache lookup was deduplicated.
            metric!(counter(&format!("caches.{}.channel.hit", self.name)) += 1);
            channel.clone()
        } else {
            // A concurrent cache lookup is considered new. This does not imply a full cache miss.
            metric!(counter(&format!("caches.{}.channel.miss", self.name)) += 1);
            let cache_dir = self.cache_dir.clone();

            let (tx, rx) = oneshot::channel();

            let file = if let Some(ref dir) = self.cache_dir {
                NamedTempFile::new_in(dir)
            } else {
                NamedTempFile::new()
            }
            .map_err(T::Error::from)
            .into_future();

            let result = file.and_then(clone!(key, |file| {
                for &scope in &[&key.scope, &Scope::Global] {
                    let path = tryf!(get_scope_path(
                        cache_dir.as_ref().map(|x| &**x),
                        scope,
                        &key.cache_key
                    ));

                    let path = match path {
                        Some(x) => x,
                        None => continue,
                    };

                    let byteview = match check_cache_hit(name, &path) {
                        Ok(x) => x,
                        Err(e) => {
                            if e.kind() == io::ErrorKind::NotFound {
                                continue;
                            } else {
                                tryf!(Err(e))
                            }
                        }
                    };

                    let item = tryf!(request.0.load(scope.clone(), byteview));
                    return Box::new(Ok(item).into_future());
                }

                // A file was not found. If this spikes, it's possible that the filesystem cache
                // just got pruned.
                metric!(counter(&format!("caches.{}.file.miss", name)) += 1);

                // XXX: Unsure if we need SyncArbiter here
                Box::new(request.0.compute(file.path()).and_then(move |new_scope| {
                    let new_cache_path = tryf!(get_scope_path(
                        cache_dir.as_ref().map(|x| &**x),
                        &new_scope,
                        &key.cache_key
                    ));

                    let byteview = tryf!(ByteView::open(file.path()));

                    metric!(
                        time_raw(&format!("caches.{}.file.size", name)) = byteview.len() as u64,
                        "hit" => "false"
                    );

                    let item = tryf!(request.0.load(new_scope, byteview));

                    if let Some(ref cache_path) = new_cache_path {
                        tryf!(file.persist(cache_path).map_err(|x| x.error));
                    }

                    Box::new(Ok(item).into_future())
                })) as Box<dyn Future<Item = T::Item, Error = T::Error>>
            }));

            ctx.spawn(
                result
                    .into_actor(self)
                    .then(clone!(key, |result, slf, _ctx| {
                        slf.current_computations.remove(&key);

                        tx.send(match result {
                            Ok(x) => Ok(Arc::new(x)),
                            Err(e) => Err(Arc::new(e)),
                        })
                        .map_err(|_| ())
                        .into_future()
                        .into_actor(slf)
                    })),
            );

            let channel = rx.shared();

            self.current_computations
                .insert(key.clone(), channel.clone());
            channel
        };

        Box::new(
            channel
                .map_err(|_: SharedError<oneshot::Canceled>| {
                    panic!("Oneshot channel cancelled! Race condition or system shutting down")
                })
                .and_then(|result| (*result).clone())
                .into_actor(self),
        )
    }
}

fn get_scope_path(
    cache_dir: Option<&Path>,
    scope: &Scope,
    cache_key: &str,
) -> Result<Option<PathBuf>, io::Error> {
    let dir = match cache_dir {
        Some(x) => x.join(safe_path_segment(scope.as_ref())),
        None => return Ok(None),
    };

    fs::create_dir_all(&dir)?;
    Ok(Some(dir.join(safe_path_segment(cache_key))))
}

fn safe_path_segment(s: &str) -> String {
    s.replace(".", "_") // protect against ..
        .replace("/", "_") // protect against absolute paths
        .replace(":", "_") // not a threat on POSIX filesystems, but confuses OS X Finder
}

fn check_cache_hit(name: &'static str, path: &Path) -> io::Result<ByteView<'static>> {
    let metadata = path.metadata()?;

    // A file was found and we're about to mmap it. This is also reported
    // for "negative cache hits": When we cached the 404 response from a
    // server as empty file.
    metric!(counter(&format!("caches.{}.file.hit", name)) += 1);

    if metadata
        .modified()?
        .elapsed()
        .map(|elapsed| elapsed > Duration::from_secs(3600))
        .unwrap_or(true)
    {
        // We use mtime to keep track of "cache last used", because filesystem is usually mounted
        // with `noatime` and therefore atime is nonsense.
        //
        // Since we're about to use the cache, let's touch the file. We don't touch the file if it
        // was touched in the last hour to avoid too many disk writes.
        OpenOptions::new()
            .append(true)
            .truncate(false)
            .open(&path)?;
    }

    let byteview = ByteView::open(path)?;
    metric!(
        time_raw(&format!("caches.{}.file.size", name)) = byteview.len() as u64,
        "hit" => "true"
    );

    Ok(byteview)
}
