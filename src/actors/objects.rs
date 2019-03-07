use std::{
    fs,
    io::{self, Write},
    path::Path,
    sync::Arc,
};

use actix::{Actor, Addr, Context, Handler, Message, ResponseFuture};

use actix_web::{client, HttpMessage};

use failure::{Fail, ResultExt};

use futures::{
    future::{join_all, lazy, Either, Future, IntoFuture},
    Stream,
};

use symbolic::{common::ByteView, debuginfo};

use tokio_threadpool::ThreadPool;

use void::Void;

use crate::{
    actors::cache::{CacheActor, CacheItemRequest, CacheKey, ComputeMemoized},
    http::follow_redirects,
    types::{ArcFail, FileType, ObjectId, Scope, SourceConfig},
};

const USER_AGENT: &str = concat!("symbolicator/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Fail, Clone, Copy)]
pub enum ObjectErrorKind {
    #[fail(display = "failed to download")]
    Io,

    #[fail(display = "failed sending message to actor")]
    Mailbox,

    #[fail(display = "failed parsing object")]
    Parsing,

    #[fail(display = "mismatching IDs")]
    IdMismatch,

    #[fail(display = "bad status code")]
    BadStatusCode,

    #[fail(display = "failed sending request to source")]
    SendRequest,

    #[fail(display = "no symbols found")]
    NotFound,

    #[fail(display = "failed to look into cache")]
    Caching,
}

symbolic::common::derive_failure!(
    ObjectError,
    ObjectErrorKind,
    doc = "Errors happening while fetching objects"
);

impl From<io::Error> for ObjectError {
    fn from(e: io::Error) -> Self {
        e.context(ObjectErrorKind::Io).into()
    }
}

pub struct FetchObjectInternal {
    request: FetchObject,
    threadpool: Arc<ThreadPool>,
}

impl CacheItemRequest for FetchObjectInternal {
    type Item = Object;
    type Error = ObjectError;

    fn get_cache_key(&self) -> CacheKey {
        CacheKey {
            cache_key: self.request.identifier.get_cache_key(),
            scope: self.request.scope.clone(),
        }
    }

    fn compute(&self, path: &Path) -> Box<dyn Future<Item = Scope, Error = Self::Error>> {
        let mut urls = vec![];
        for source in &self.request.sources {
            let url = source.get_base_url();

            // PDB
            if let (FileType::Debug, Some(ref debug_name), Some(ref debug_id)) = (
                self.request.filetype,
                &self.request.identifier.debug_name,
                &self.request.identifier.debug_id,
            ) {
                urls.push((
                    source.clone(),
                    url.join(&format!(
                        "{}/{}/{}",
                        debug_name,
                        debug_id.breakpad(),
                        debug_name
                    )),
                ));
            }

            // PE
            if let (FileType::Code, Some(ref code_name), Some(ref code_id)) = (
                self.request.filetype,
                &self.request.identifier.code_name,
                &self.request.identifier.code_id,
            ) {
                urls.push((
                    source.clone(),
                    url.join(&format!("{}/{}/{}", code_name, code_id, code_name)),
                ));
            }

            // ELF
            if let Some(ref code_id) = self.request.identifier.code_id {
                if self.request.filetype == FileType::Debug {
                    urls.push((
                        source.clone(),
                        url.join(&format!("_.debug/elf-buildid-sym-{}/_.debug", code_id)),
                    ));
                }

                if let (FileType::Code, Some(ref code_name)) =
                    (self.request.filetype, &self.request.identifier.code_name)
                {
                    urls.push((
                        source.clone(),
                        url.join(&format!(
                            "{}/elf-buildid-{}/{}",
                            code_name, code_id, code_name
                        )),
                    ));
                }
            }

            // TODO: MachO, Breakpad
            // TODO: Use Object.filetype to filter out unwanted variants
            // TODO: Native variants
        }

        let requests = join_all(urls.into_iter().enumerate().map(
            |(i, (source, url_parse_result))| {
                // TODO: Is this fallible?
                let url = url_parse_result.unwrap();

                log::debug!("Fetching {}", url);

                follow_redirects(
                    client::get(&url)
                        .header("User-Agent", USER_AGENT)
                        .finish()
                        .unwrap(),
                    10,
                )
                .then(move |result| match result {
                    Ok(response) => {
                        if response.status().is_success() {
                            log::info!("Success hitting {}", url);
                            Ok((i, source, Some(response.payload())))
                        } else {
                            log::debug!(
                                "Unexpected status code from {}: {}",
                                url,
                                response.status()
                            );
                            Ok((i, source, None))
                        }
                    }
                    Err(e) => {
                        log::warn!("Skipping response from {}: {}", url, e);
                        Ok((i, source, None))
                    }
                })
                .map_err(|_: Void| unreachable!())
            },
        ));

        let path = path.to_owned();
        let filetype = self.request.filetype;
        let request_scope = self.request.scope.clone();
        let threadpool = self.threadpool.clone();

        let result = requests.and_then(move |requests| {
            let payload = requests
                .into_iter()
                .filter_map(|(i, source, payload)| {
                    Some((
                        i,
                        source,
                        payload?.map_err(|e| ObjectError::from(e.context(ObjectErrorKind::Io))),
                    ))
                })
                .min_by_key(|(ref i, _, _)| *i);

            if let Some((_, source, payload)) = payload {
                log::debug!("Found {:?} file", filetype);
                let file = fs::File::create(&path)
                    .map_err(|e| ObjectError::from(e.context(ObjectErrorKind::Io)))
                    .into_future();

                let result = file.and_then(|file| {
                    payload.fold(file, move |mut file, chunk| {
                        threadpool.spawn_handle(lazy(move || file.write_all(&chunk).map(|_| file)))
                    })
                });

                let scope = if source.is_public() {
                    Scope::Global
                } else {
                    request_scope
                };

                Either::A(result.map(|_| scope))
            } else {
                log::debug!("No {:?} file found", filetype);
                Either::B(Ok(Scope::Global).into_future())
            }
        });

        Box::new(result)
    }

    fn load(self, scope: Scope, data: ByteView<'static>) -> Result<Self::Item, Self::Error> {
        let is_empty = data.is_empty();
        let rv = Object {
            request: self.request,
            scope,
            object: if is_empty { None } else { Some(data) },
        };

        if !is_empty {
            let object = rv.get_object()?;

            if let Some(ref debug_id) = rv.request.identifier.debug_id {
                if object.debug_id() != *debug_id {
                    return Err(ObjectErrorKind::IdMismatch.into());
                }
            }

            if let Some(ref code_id) = rv.request.identifier.code_id {
                if let Some(ref object_code_id) = object.code_id() {
                    if object_code_id != code_id {
                        return Err(ObjectErrorKind::IdMismatch.into());
                    }
                }
            }
        }

        Ok(rv)
    }
}

/// Handle to local cache file.
#[derive(Debug, Clone)]
pub struct Object {
    request: FetchObject,
    scope: Scope,
    // TODO: cache symbolic object here
    object: Option<ByteView<'static>>,
}

impl Object {
    pub fn get_object(&self) -> Result<debuginfo::Object<'_>, ObjectError> {
        let bytes = self.object.as_ref().ok_or(ObjectErrorKind::NotFound)?;
        Ok(debuginfo::Object::parse(&bytes).context(ObjectErrorKind::Parsing)?)
    }

    pub fn scope(&self) -> &Scope {
        &self.scope
    }
}

#[derive(Clone)]
pub struct ObjectsActor {
    cache: Addr<CacheActor<FetchObjectInternal>>,
    threadpool: Arc<ThreadPool>,
}

impl ObjectsActor {
    pub fn new(cache: Addr<CacheActor<FetchObjectInternal>>) -> Self {
        ObjectsActor {
            cache,
            threadpool: Arc::new(ThreadPool::new()),
        }
    }
}

impl Actor for ObjectsActor {
    type Context = Context<Self>;
}

/// Fetch a Object from external sources or internal cache.
#[derive(Debug, Clone)]
pub struct FetchObject {
    pub scope: Scope,
    pub filetype: FileType,
    pub identifier: ObjectId,
    pub sources: Vec<SourceConfig>,
}

impl Message for FetchObject {
    type Result = Result<Arc<Object>, ObjectError>;
}

impl Handler<FetchObject> for ObjectsActor {
    type Result = ResponseFuture<Arc<Object>, ObjectError>;

    fn handle(&mut self, request: FetchObject, _ctx: &mut Self::Context) -> Self::Result {
        Box::new(
            self.cache
                .send(ComputeMemoized(FetchObjectInternal {
                    request,
                    threadpool: self.threadpool.clone(),
                }))
                .map_err(|e| e.context(ObjectErrorKind::Mailbox).into())
                .and_then(|response| {
                    Ok(response.map_err(|e| ArcFail(e).context(ObjectErrorKind::Caching))?)
                }),
        )
    }
}
