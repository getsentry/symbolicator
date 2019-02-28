use std::fs;
use std::io;
use std::io::Write;

use crate::actors::cache::CacheItem;
use crate::actors::cache::Compute;
use crate::actors::cache::ComputeMemoized;
use crate::actors::cache::GetCacheKey;
use crate::actors::cache::LoadCache;
use crate::http::follow_redirects;
use actix::fut::wrap_future;
use actix::MailboxError;
use actix::MessageResult;
use actix_web::error::PayloadError;
use futures::future::{join_all, Either, Future, IntoFuture};
use futures::Stream;
use url::Url;

use failure::Fail;

use crate::actors::cache::CacheActor;

use actix::Actor;
use actix::Addr;
use actix::Context;
use actix::Handler;
use actix::Message;
use actix::ResponseActFuture;

use actix_web::client;
use actix_web::client::SendRequestError;
use actix_web::http::StatusCode;
use actix_web::HttpMessage;

use symbolic::common::ByteView;
use symbolic::common::DebugId;
use symbolic::debuginfo;

#[derive(Deserialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SourceConfig {
    Http {
        id: String,
        #[serde(with = "url_serde")]
        url: Url,
    },
}

impl SourceConfig {
    fn get_base_url(&self) -> &Url {
        match *self {
            SourceConfig::Http { ref url, .. } => &url,
        }
    }
}

#[derive(Debug, Fail, From)]
pub enum ObjectError {
    #[fail(display = "Failed to download: {}", _0)]
    Io(io::Error),

    #[fail(display = "Failed sending message to actor: {}", _0)]
    Mailbox(MailboxError),

    #[fail(display = "Failed parsing object: {}", _0)]
    Parsing(symbolic::debuginfo::ObjectError),

    #[fail(display = "Mismatching IDs")]
    IdMismatch,

    #[fail(display = "Bad status code: {}", _0)]
    BadStatusCode(StatusCode),

    #[fail(display = "Failed sending request to bucket: {}", _0)]
    SendRequest(SendRequestError),

    #[fail(display = "Failed downloading bucket: {}", _0)]
    Payload(PayloadError),

    #[fail(display = "No symbols found")]
    NotFound,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileType {
    Debug,
    Code,
    Breakpad,
}

/// Information to find a Object in external buckets and also internal cache.
#[derive(Debug, Clone)]
pub struct ObjectId {
    pub debug_id: Option<DebugId>,
    pub code_id: Option<String>,
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
            rv.push_str(code_id);
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

/// Handle to local cache file.
#[derive(Clone)]
pub struct Object {
    request: FetchObject,
    object: Option<ByteView<'static>>,
}

impl Object {
    pub fn get_object<'a>(&'a self) -> Result<debuginfo::Object<'a>, ObjectError> {
        Ok(debuginfo::Object::parse(
            &self.object.as_ref().ok_or(ObjectError::NotFound)?,
        )?)
    }
}

impl Actor for Object {
    type Context = Context<Self>;
}

impl CacheItem for Object {
    type Error = ObjectError;
}

impl Handler<GetCacheKey> for Object {
    type Result = String;

    fn handle(&mut self, _item: GetCacheKey, _ctx: &mut Self::Context) -> Self::Result {
        // TODO: Add bucket config here
        self.request.identifier.get_cache_key()
    }
}

/// Hack: Get access to symbolic structs even though they live in the actor.
struct GetObject;

impl Message for GetObject {
    type Result = Object;
}

impl Handler<GetObject> for Object {
    type Result = MessageResult<GetObject>;

    fn handle(&mut self, _item: GetObject, _ctx: &mut Self::Context) -> Self::Result {
        MessageResult(self.clone())
    }
}

impl Handler<Compute<Object>> for Object {
    type Result = ResponseActFuture<Self, (), <Object as CacheItem>::Error>;

    fn handle(&mut self, item: Compute<Object>, _ctx: &mut Self::Context) -> Self::Result {
        let mut urls = vec![];
        for bucket in &self.request.sources {
            let url = bucket.get_base_url();

            // PDB
            if let (FileType::Debug, Some(ref debug_name), Some(ref debug_id)) = (
                self.request.filetype,
                &self.request.identifier.debug_name,
                &self.request.identifier.debug_id,
            ) {
                urls.push(url.join(&format!(
                    "{}/{}/{}",
                    debug_name,
                    debug_id.breakpad(),
                    debug_name
                )));
            }

            // PE
            if let (FileType::Code, Some(ref code_name), Some(ref code_id)) = (
                self.request.filetype,
                &self.request.identifier.code_name,
                &self.request.identifier.code_id,
            ) {
                urls.push(url.join(&format!("{}/{}/{}", code_name, code_id, code_name)));
            }

            // ELF
            if let Some(ref code_id) = self.request.identifier.code_id {
                if self.request.filetype == FileType::Debug {
                    urls.push(url.join(&format!("_.debug/elf-buildid-sym-{}/_.debug", code_id)));
                }

                if let (FileType::Code, Some(ref code_name)) =
                    (self.request.filetype, &self.request.identifier.code_name)
                {
                    urls.push(url.join(&format!(
                        "{}/elf-buildid-{}/{}",
                        code_name, code_id, code_name
                    )));
                }
            }

            // TODO: MachO, Breakpad
            // TODO: Use Object.filetype to filter out unwanted variants
        }

        let requests = join_all(urls.into_iter().enumerate().map(|(i, url_parse_result)| {
            // TODO: Is this fallible?
            let url = url_parse_result.unwrap();

            debug!("Fetching {}", url);

            follow_redirects(
                client::get(&url)
                    .header("User-Agent", "sentry-symbolicator/1.0")
                    .finish()
                    .unwrap(),
                10,
            )
            .then(move |result| match result {
                Ok(response) => {
                    if response.status().is_success() {
                        Ok((i, Some(response.payload())))
                    } else {
                        debug!("Unexpected status code from {}: {}", url, response.status());
                        Ok((i, None))
                    }
                }
                Err(e) => {
                    debug!("Skipping response from {}: {}", url, e);
                    Ok((i, None))
                }
            })
            // The above future is infallible, so Rust complains about unknown type for future
            // error.
            .map_err(|_: ()| unreachable!())
        }));

        let result = requests.and_then(move |requests| {
            let payload = requests
                .into_iter()
                .filter_map(|(i, payload)| Some((i, payload?.map_err(ObjectError::from))))
                .min_by_key(|(ref i, _)| *i);

            // TODO: Destructure FatObject, validate file download before LoadCache is called
            // We also need to parse object here for new caching key, which depends on the object
            // type
            //
            // XXX(markus): Can we ever take info from Object for caching?

            if let Some((_, payload)) = payload {
                let file = fs::File::create(&item.path)
                    .into_future()
                    .map_err(ObjectError::from);

                let result = file.and_then(|mut file| {
                    payload.for_each(move |chunk| {
                        // TODO: Call out to SyncArbiter
                        file.write_all(&chunk).map_err(ObjectError::from)
                    })
                });

                Either::A(result)
            } else {
                Either::B(Ok(()).into_future())
            }
        });

        Box::new(wrap_future(result))
    }
}

impl Handler<LoadCache<Object>> for Object {
    type Result = ResponseActFuture<Self, (), <Object as CacheItem>::Error>;

    fn handle(&mut self, item: LoadCache<Object>, _ctx: &mut Self::Context) -> Self::Result {
        if item.value.is_empty() {
            println!("Loading cache: empty!");
            self.object = None;
        } else {
            println!("Loading cache: not empty!");
            self.object = Some(item.value);
            let object = tryfa!(self.get_object());

            if let Some(ref debug_id) = self.request.identifier.debug_id {
                // TODO: Also check code_id when exposed in symbolic
                if object.debug_id() != *debug_id {
                    println!("Loading cache: debug_id MISMATCH!?!?!?!??!?!");
                    tryfa!(Err(ObjectError::IdMismatch));
                }
            }
        }

        Box::new(wrap_future(Ok(()).into_future()))
    }
}

pub struct ObjectsActor {
    cache: Addr<CacheActor<Object>>,
}

impl ObjectsActor {
    pub fn new(cache: Addr<CacheActor<Object>>) -> Self {
        ObjectsActor { cache }
    }
}

impl Actor for ObjectsActor {
    type Context = Context<Self>;
}

/// Fetch a Object from external buckets or internal cache.
#[derive(Debug, Clone)]
pub struct FetchObject {
    pub filetype: FileType,
    pub identifier: ObjectId,
    pub sources: Vec<SourceConfig>,
}

impl Message for FetchObject {
    type Result = Result<Object, ObjectError>;
}

impl Handler<FetchObject> for ObjectsActor {
    type Result = ResponseActFuture<Self, Object, ObjectError>;

    fn handle(&mut self, message: FetchObject, _ctx: &mut Self::Context) -> Self::Result {
        let res = self.cache.send(ComputeMemoized(Object {
            request: message,
            object: None,
        }));

        Box::new(wrap_future(
            res.flatten()
                .and_then(|object_addr| object_addr.send(GetObject).map_err(ObjectError::from))
                .map_err(ObjectError::from),
        ))
    }
}
