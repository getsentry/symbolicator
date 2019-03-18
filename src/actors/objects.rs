use std::{
    fs,
    io::{self, Write},
    path::Path,
    sync::Arc,
    time::Duration,
};

use actix::{Actor, Addr, Context, Handler, Message, ResponseFuture};

use actix_web::{client, HttpMessage};

use bytes::Bytes;

use failure::{Fail, ResultExt};

use futures::{
    future::{join_all, lazy, Either, Future, IntoFuture},
    Stream,
};

use symbolic::{common::ByteView, debuginfo};

use tokio_threadpool::ThreadPool;

use crate::futures::measure_task;

use crate::types::DirectoryLayout;

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

    #[fail(display = "object download took too long")]
    Timeout,
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

#[derive(Debug, Clone)]
pub struct FetchObjectInternal {
    filetype: FileType,
    scope: Scope,
    identifier: ObjectId,
    source: SourceConfig,
    threadpool: Arc<ThreadPool>,
}

impl CacheItemRequest for FetchObjectInternal {
    type Item = Object;
    type Error = ObjectError;

    fn get_cache_key(&self) -> CacheKey {
        CacheKey {
            cache_key: format!(
                "{}.{}.{}",
                self.source.id(),
                self.identifier.get_cache_key(),
                self.filetype.as_ref()
            ),
            scope: self.scope.clone(),
        }
    }

    fn compute(&self, path: &Path) -> Box<dyn Future<Item = Scope, Error = Self::Error>> {
        let request = download_from_source(self.source.clone(), self.filetype, &self.identifier);

        let path = path.to_owned();
        let filetype = self.filetype;
        let source = self.source.clone();
        let request_scope = self.scope.clone();
        let threadpool = self.threadpool.clone();

        let final_scope = if source.is_public() {
            Scope::Global
        } else {
            request_scope
        };

        let result = request.and_then(move |payload| {
            if let Some(payload) = payload {
                log::debug!("Found {:?} file", filetype);
                let file = fs::File::create(&path)
                    .map_err(|e| ObjectError::from(e.context(ObjectErrorKind::Io)))
                    .into_future();

                let result = file.and_then(|file| {
                    payload.fold(file, move |mut file, chunk| {
                        threadpool.spawn_handle(lazy(move || file.write_all(&chunk).map(|_| file)))
                    })
                });

                Either::A(result.map(|_| final_scope))
            } else {
                log::debug!("No {:?} file found", filetype);
                Either::B(Ok(final_scope).into_future())
            }
        });

        Box::new(measure_task(
            "fetch_object",
            Some((Duration::from_secs(120), || ObjectErrorKind::Timeout.into())),
            result,
        ))
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
    request: FetchObjectInternal,
    scope: Scope,
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
    pub fn new(cache: Addr<CacheActor<FetchObjectInternal>>, threadpool: Arc<ThreadPool>) -> Self {
        ObjectsActor { cache, threadpool }
    }
}

impl Actor for ObjectsActor {
    type Context = Context<Self>;
}

/// Fetch a Object from external sources or internal cache.
#[derive(Debug, Clone)]
pub struct FetchObject {
    pub filetypes: &'static [FileType],
    pub scope: Scope,
    pub identifier: ObjectId,
    pub sources: Vec<SourceConfig>,
}

impl Message for FetchObject {
    type Result = Result<Arc<Object>, ObjectError>;
}

impl Handler<FetchObject> for ObjectsActor {
    type Result = ResponseFuture<Arc<Object>, ObjectError>;

    fn handle(&mut self, request: FetchObject, _ctx: &mut Self::Context) -> Self::Result {
        let FetchObject {
            filetypes,
            scope,
            identifier,
            sources,
        } = request;
        let mut requests = vec![];

        for &filetype in filetypes {
            for source in sources.iter().cloned() {
                requests.push(FetchObjectInternal {
                    source,
                    filetype,
                    scope: scope.clone(),
                    identifier: identifier.clone(),
                    threadpool: self.threadpool.clone(),
                })
            }
        }

        let cache = self.cache.clone();

        Box::new(
            join_all(requests.into_iter().enumerate().map(move |(i, request)| {
                cache
                    .send(ComputeMemoized(request))
                    .map_err(|e| e.context(ObjectErrorKind::Mailbox).into())
                    .and_then(move |response| {
                        Ok((
                            i,
                            response.map_err(|e| ArcFail(e).context(ObjectErrorKind::Caching))?,
                        ))
                    })
            }))
            .and_then(move |responses| {
                let (_, response) = responses
                    .into_iter()
                    .min_by_key(|(ref i, response)| {
                        (
                            // Prefer object files with debug info over object files without
                            // Prefer files that contain an object over unparseable files
                            match response.get_object().map(|x| x.has_debug_info()) {
                                Ok(true) => 0,
                                Ok(false) => 1,
                                Err(_) => 2,
                            },
                            *i,
                        )
                    })
                    .ok_or_else(|| ObjectError::from(ObjectErrorKind::NotFound))?;
                Ok(response)
            }),
        )
    }
}

fn get_directory_path(
    directory_layout: DirectoryLayout,
    filetype: FileType,
    identifier: &ObjectId,
) -> Option<String> {
    use DirectoryLayout::*;
    use FileType::*;

    match (directory_layout, filetype) {
        (_, PDB) => {
            // PDB (Microsoft Symbol Server)
            let debug_name = identifier.debug_name.as_ref()?;
            let debug_id = identifier.debug_id.as_ref()?;
            // XXX: Calling `breakpad` here is kinda wrong. We really only want to have no hyphens.
            Some(format!(
                "{}/{}/{}",
                debug_name,
                debug_id.breakpad(),
                debug_name
            ))
        }
        (_, PE) => {
            // PE (Microsoft Symbol Server)
            let code_name = identifier.code_name.as_ref()?;
            let code_id = identifier.code_id.as_ref()?;
            Some(format!("{}/{}/{}", code_name, code_id, code_name))
        }
        (Symstore, ELFDebug) => {
            // ELF debug files (Microsoft Symbol Server)
            let code_id = identifier.code_id.as_ref()?;
            Some(format!("_.debug/elf-buildid-sym-{}/_.debug", code_id))
        }
        (Native, ELFDebug) => {
            // ELF debug files (GDB format = "native")
            let code_id = identifier.code_id.as_ref()?;
            Some(format!("{}.debug", chunk_gdb(code_id.as_str())?))
        }
        (Symstore, ELFCode) => {
            // ELF code files (Microsoft Symbol Server)
            let code_id = identifier.code_id.as_ref()?;
            let code_name = identifier.code_name.as_ref()?;
            Some(format!(
                "{}/elf-buildid-{}/{}",
                code_name, code_id, code_name
            ))
        }
        (Native, ELFCode) => {
            // ELF code files (GDB format = "native")
            let code_id = identifier.code_id.as_ref()?;
            chunk_gdb(code_id.as_str())
        }
        (Symstore, MachDebug) => {
            // Mach debug files (Microsoft Symbol Server)
            let code_id = identifier.code_id.as_ref()?;
            Some(format!("_.dwarf/mach-uuid-sym-{}/_.dwarf", code_id))
        }
        (Native, MachDebug) => {
            // Mach debug files (LLDB format = "native")
            let code_id = identifier.code_id.as_ref()?;
            chunk_lldb(code_id.as_str())
        }
        (Symstore, MachCode) => {
            // Mach code files (Microsoft Symbol Server)
            let code_id = identifier.code_id.as_ref()?;
            let code_name = identifier.code_name.as_ref()?;
            Some(format!("{}/mach-uuid-{}/{}", code_name, code_id, code_name))
        }
        (Native, MachCode) => {
            // Mach code files (LLDB format = "native")
            let code_id = identifier.code_id.as_ref()?;
            Some(format!("{}.app", chunk_lldb(code_id.as_str())?))
        }
        (_, Breakpad) => {
            // Breakpad
            let debug_name = identifier.debug_name.as_ref()?;
            let debug_id = identifier.debug_id.as_ref()?;

            let new_debug_name = if debug_name.ends_with(".exe")
                || debug_name.ends_with(".dll")
                || debug_name.ends_with(".pdb")
            {
                &debug_name[..debug_name.len() - 4]
            } else {
                &debug_name[..]
            };

            Some(format!(
                "{}.sym/{}/{}",
                new_debug_name,
                debug_id.breakpad(),
                debug_name
            ))
        }
    }
}

fn download_from_source(
    source: SourceConfig,
    filetype: FileType,
    identifier: &ObjectId,
) -> Box<
    dyn Future<
        Item = Option<Box<dyn Stream<Item = Bytes, Error = ObjectError>>>,
        Error = ObjectError,
    >,
> {
    match source {
        SourceConfig::Http {
            url,
            layout,
            filetypes,
            ..
        } => {
            if !filetypes.contains(&filetype) {
                return Box::new(Ok(None).into_future());
            }

            // XXX: Probably should send an error if the URL turns out to be invalid
            let download_url = match get_directory_path(layout, filetype, identifier)
                .and_then(|x| url.join(&x).ok())
            {
                Some(x) => x,
                None => return Box::new(Ok(None).into_future()),
            };

            let response = follow_redirects(
                client::get(&download_url)
                    .header("User-Agent", USER_AGENT)
                    .finish()
                    .unwrap(),
                10,
            )
            .then(move |result| match result {
                Ok(response) => {
                    if response.status().is_success() {
                        log::info!("Success hitting {}", url);
                        Ok(Some(Box::new(
                            response
                                .payload()
                                .map_err(|e| e.context(ObjectErrorKind::Io).into()),
                        )
                            as Box<dyn Stream<Item = _, Error = _>>))
                    } else {
                        log::debug!("Unexpected status code from {}: {}", url, response.status());
                        Ok(None)
                    }
                }
                Err(e) => {
                    log::warn!("Skipping response from {}: {}", url, e);
                    Ok(None)
                }
            });

            Box::new(response)
        }
    }
}

fn chunk_gdb(code_id: &str) -> Option<String> {
    // this is just a panic guard. It is not meant to validate the GNU build id
    if code_id.len() > 2 {
        Some(format!("{}/{}", &code_id[..2], &code_id[2..]))
    } else {
        None
    }
}

fn chunk_lldb(code_id: &str) -> Option<String> {
    // this is just a panic guard. It is not meant to validate the UUID
    if code_id.len() > 20 {
        Some(format!(
            "{}/{}/{}/{}/{}/{}",
            &code_id[0..4],
            &code_id[4..8],
            &code_id[8..12],
            &code_id[12..16],
            &code_id[16..20],
            &code_id[20..]
        ))
    } else {
        None
    }
}
