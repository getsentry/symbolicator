use crate::{
    actors::cache::{CacheKey, ComputeMemoized},
    types::{ArcFail, Scope, SourceConfig},
};
use actix::ResponseFuture;
use std::{
    fs,
    io::{self, Write},
    path::Path,
    sync::Arc,
};
use void::Void;

use crate::{actors::cache::CacheItemRequest, http::follow_redirects};
use futures::{
    future::{join_all, Either, Future, IntoFuture},
    Stream,
};

use failure::{Fail, ResultExt};

use crate::actors::cache::CacheActor;

use actix::{Actor, Addr, Context, Handler, Message};

use actix_web::{client, HttpMessage};

use symbolic::{
    common::{ByteView, CodeId, DebugId},
    debuginfo,
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

    #[fail(display = "failed downloading source")]
    Payload,

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileType {
    Debug,
    Code,
    Breakpad,
}

/// Information to find a Object in external sources and also internal cache.
#[derive(Debug, Clone)]
pub struct ObjectId {
    pub debug_id: Option<DebugId>,
    pub code_id: Option<CodeId>,
    pub debug_name: Option<String>,
    pub code_name: Option<String>,
}

impl ObjectId {
    pub fn get_cache_key(&self) -> String {
        let mut rv = String::new();
        if let Some(ref debug_id) = self.debug_id {
            rv.push_str(&debug_id.to_string());
        }
        rv.push_str("-");
        if let Some(ref code_id) = self.code_id {
            rv.push_str(code_id.as_str());
        }

        // TODO: replace with new caching key discussed with jauer
        rv.push_str("-");
        if let Some(ref debug_name) = self.debug_name {
            rv.push_str(debug_name);
        }

        rv.push_str("-");
        if let Some(ref code_name) = self.code_name {
            rv.push_str(code_name);
        }

        rv
    }
}

impl CacheItemRequest for FetchObject {
    type Item = Object;
    type Error = ObjectError;

    fn get_cache_key(&self) -> CacheKey {
        CacheKey {
            cache_key: self.identifier.get_cache_key(),
            scope: self.scope.clone(),
        }
    }

    fn compute(&self, path: &Path) -> Box<dyn Future<Item = Scope, Error = Self::Error>> {
        let mut urls = vec![];
        for source in &self.sources {
            let url = source.get_base_url();

            // PDB
            if let (FileType::Debug, Some(ref debug_name), Some(ref debug_id)) = (
                self.filetype,
                &self.identifier.debug_name,
                &self.identifier.debug_id,
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
                self.filetype,
                &self.identifier.code_name,
                &self.identifier.code_id,
            ) {
                urls.push((
                    source.clone(),
                    url.join(&format!("{}/{}/{}", code_name, code_id, code_name)),
                ));
            }

            // ELF
            if let Some(ref code_id) = self.identifier.code_id {
                if self.filetype == FileType::Debug {
                    urls.push((
                        source.clone(),
                        url.join(&format!("_.debug/elf-buildid-sym-{}/_.debug", code_id)),
                    ));
                }

                if let (FileType::Code, Some(ref code_name)) =
                    (self.filetype, &self.identifier.code_name)
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
        let filetype = self.filetype;
        let request_scope = self.scope.clone();

        let result = requests.and_then(move |requests| {
            let payload = requests
                .into_iter()
                .filter_map(|(i, source, payload)| {
                    Some((
                        i,
                        source,
                        payload?.map_err(|e| e.context(ObjectErrorKind::Payload).into()),
                    ))
                })
                .min_by_key(|(ref i, _, _)| *i);

            if let Some((_, source, payload)) = payload {
                log::debug!("Found {:?} file", filetype);
                let file = fs::File::create(&path)
                    .context(ObjectErrorKind::Io)
                    .map_err(ObjectError::from)
                    .into_future();

                let result = file.and_then(|mut file| {
                    payload.for_each(move |chunk| {
                        // TODO: Call out to SyncArbiter
                        file.write_all(&chunk)
                            .map_err(|e| e.context(ObjectErrorKind::Io).into())
                    })
                });

                let scope = if source.is_public() {
                    Scope::Global
                } else {
                    request_scope
                };

                Either::A(result.map(|()| scope))
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
            request: self,
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
    cache: Addr<CacheActor<FetchObject>>,
}

impl ObjectsActor {
    pub fn new(cache: Addr<CacheActor<FetchObject>>) -> Self {
        ObjectsActor { cache }
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

    fn handle(&mut self, message: FetchObject, _ctx: &mut Self::Context) -> Self::Result {
        Box::new(
            self.cache
                .send(ComputeMemoized(message))
                .map_err(|e| e.context(ObjectErrorKind::Mailbox).into())
                .and_then(|response| {
                    Ok(response.map_err(|e| ArcFail(e).context(ObjectErrorKind::Caching))?)
                }),
        )
    }
}
