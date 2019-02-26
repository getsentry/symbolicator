use crate::actors::cache::CacheItem;
use crate::actors::cache::Compute;
use crate::actors::cache::ComputeMemoized;
use crate::actors::cache::GetCacheKey;
use crate::actors::cache::LoadCache;
use crate::http::follow_redirects;
use actix::fut::ok;
use actix::fut::wrap_future;
use actix::MailboxError;
use actix_web::error::PayloadError;
use futures::future::{join_all, Either, Future, IntoFuture};
use futures::Stream;
use std::io;
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
use symbolic::common::SelfCell;
use symbolic::debuginfo::Object;

use tokio::fs::File;
use tokio::io::write_all;

#[derive(Deserialize, Clone)]
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
pub enum DebugInfoError {
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

    #[fail(display = "???")]
    Other,
}

#[derive(Clone, Copy, Debug)]
pub enum FileType {
    Debug,
    Code,
    Breakpad,
}

/// Information to find a DebugInfo in external buckets and also internal cache.
pub struct DebugInfoId {
    pub filetype: FileType,
    pub debug_id: Option<DebugId>,
    pub code_id: Option<String>,
    pub debug_name: Option<String>,
    pub code_name: Option<String>,
}

impl DebugInfoId {
    pub fn get_cache_key(&self) -> String {
        let mut rv = String::new();
        // TODO: Add bucket config here
        if let Some(ref debug_id) = self.debug_id {
            rv.push_str(&debug_id.to_string());
        }
        rv.push_str("-");
        if let Some(ref code_id) = self.code_id {
            rv.push_str(code_id);
        }
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
pub struct DebugInfo {
    request: FetchDebugInfo,
    object: Option<ByteView<'static>>,
}

impl DebugInfo {
    fn get_object<'a>(&'a self) -> Result<Object<'a>, DebugInfoError> {
        Ok(Object::parse(&self.object.as_ref().ok_or(DebugInfoError::NotFound)?)?)
    }
}

impl Actor for DebugInfo {
    type Context = Context<Self>;
}

impl CacheItem for DebugInfo {
    type Error = DebugInfoError;
}

impl Handler<GetCacheKey> for DebugInfo {
    type Result = String;

    fn handle(&mut self, _item: GetCacheKey, _ctx: &mut Self::Context) -> Self::Result {
        // TODO: Add bucket config here
        self.request.identifier.get_cache_key()
    }
}

impl Handler<Compute<DebugInfo>> for DebugInfo {
    type Result = ResponseActFuture<Self, (), <DebugInfo as CacheItem>::Error>;

    fn handle(&mut self, item: Compute<DebugInfo>, _ctx: &mut Self::Context) -> Self::Result {
        let mut urls = vec![];
        for bucket in &self.request.configs {
            let url = bucket.get_base_url();

            // PDB
            if let (Some(ref debug_name), Some(ref debug_id)) = (
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
            if let (Some(ref code_name), Some(ref code_id)) = (
                &self.request.identifier.code_name,
                &self.request.identifier.code_id,
            ) {
                urls.push(url.join(&format!("{}/{}/{}", code_name, code_id, code_name)));
            }

            // ELF
            if let Some(ref code_id) = self.request.identifier.code_id {
                urls.push(url.join(&format!("_.debug/elf-buildid-sym-{}/_.debug", code_id)));
                if let Some(ref code_name) = self.request.identifier.code_name {
                    urls.push(url.join(&format!(
                        "{}/elf-buildid-{}/{}",
                        code_name, code_id, code_name
                    )));
                }
            }

            // TODO: MachO, Breakpad
            // TODO: Use DebugInfo.filetype to filter out unwanted variants
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
            // error. Hint at compiler that E = !
            .map_err(|_: ()| unreachable!())
        }));

        let result = requests.and_then(move |requests| {
            let payload = requests
                .into_iter()
                .filter_map(|(i, payload)| Some((i, payload?.map_err(DebugInfoError::from))))
                .min_by_key(|(ref i, _)| *i);

            if let Some((_, payload)) = payload {
                Either::A(
                    File::open(item.path.clone())
                        .map_err(DebugInfoError::from)
                        .and_then(|file| {
                            payload.fold(file, |file, chunk| {
                                write_all(file, chunk).map(|(file, _chunk)| file)
                            })
                        })
                        .map(|_| ()),
                )
            } else {
                Either::B(Ok(()).into_future())
            }
        });

        Box::new(wrap_future(result))
    }
}

impl Handler<LoadCache<DebugInfo>> for DebugInfo {
    type Result = ResponseActFuture<Self, (), <DebugInfo as CacheItem>::Error>;

    fn handle(&mut self, item: LoadCache<DebugInfo>, _ctx: &mut Self::Context) -> Self::Result {
        self.object = Some(item.value);


        if let Some(ref debug_id) = self.request.identifier.debug_id {
            let object = tryfa!(self.get_object());

            // TODO: Also check code_id when exposed in symbolic
            if object.id() != *debug_id {
                tryfa!(Err(DebugInfoError::IdMismatch));
            }
        }

        Box::new(ok(()))
    }
}

pub struct DebugSymbolsActor {
    cache: Addr<CacheActor<DebugInfo>>,
}

impl DebugSymbolsActor {
    pub fn new(cache: Addr<CacheActor<DebugInfo>>) -> Self {
        DebugSymbolsActor { cache }
    }
}

impl Actor for DebugSymbolsActor {
    type Context = Context<Self>;
}

/// Fetch a DebugInfo from external buckets or internal cache.
pub struct FetchDebugInfo {
    pub identifier: DebugInfoId,
    pub configs: Vec<SourceConfig>,
}

impl Message for FetchDebugInfo {
    type Result = Result<Addr<DebugInfo>, DebugInfoError>;
}

impl Handler<FetchDebugInfo> for DebugSymbolsActor {
    type Result = ResponseActFuture<Self, Addr<DebugInfo>, DebugInfoError>;

    fn handle(&mut self, message: FetchDebugInfo, _ctx: &mut Self::Context) -> Self::Result {
        let res = self.cache.send(ComputeMemoized(DebugInfo {
            request: message,
            object: None,
        }));

        Box::new(wrap_future(res.map_err(DebugInfoError::from).flatten()))
    }
}
