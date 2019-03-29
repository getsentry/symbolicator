use std::sync::Arc;

use actix::Addr;
use actix_web::http::header::HeaderName;
use actix_web::{client, HttpMessage};
use failure::Fail;
use futures::{future, Future, IntoFuture, Stream};
use tokio_threadpool::ThreadPool;

use crate::actors::cache::{CacheActor, ComputeMemoized};
use crate::actors::objects::{
    DownloadStream, FetchFile, FileId, ObjectError, ObjectErrorKind, PrioritizedDownloads,
    USER_AGENT,
};
use crate::http;
use crate::types::{
    ArcFail, DirectoryLayout, FileType, HttpSourceConfig, ObjectId, Scope, SourceConfig,
};

pub fn prepare_downloads(
    source: &HttpSourceConfig,
    scope: Scope,
    filetypes: &'static [FileType],
    object_id: &ObjectId,
    threadpool: Arc<ThreadPool>,
    cache: Addr<CacheActor<FetchFile>>,
) -> Box<Future<Item = PrioritizedDownloads, Error = ObjectError>> {
    let mut requests = vec![];

    for &filetype in filetypes {
        requests.push(FetchFile {
            source: SourceConfig::Http(source.clone()),
            scope: scope.clone(),
            file_id: FileId::Http {
                filetype,
                object_id: object_id.clone(),
            },
            threadpool: threadpool.clone(),
        });
    }

    Box::new(future::join_all(requests.into_iter().map(move |request| {
        cache
            .send(ComputeMemoized(request))
            .map_err(|e| e.context(ObjectErrorKind::Mailbox).into())
            .and_then(move |response| {
                Ok(response.map_err(|e| ArcFail(e).context(ObjectErrorKind::Caching).into()))
            })
    })))
}

pub fn download_from_source(
    source: &HttpSourceConfig,
    file_id: &FileId,
) -> Box<Future<Item = Option<DownloadStream>, Error = ObjectError>> {
    let (object_id, filetype) = match file_id {
        FileId::Http {
            object_id,
            filetype,
        } => (object_id, *filetype),
        _ => unreachable!(), // XXX(markus): fugly
    };

    if !source.filetypes.contains(&filetype) {
        return Box::new(Ok(None).into_future());
    }

    // XXX: Probably should send an error if the URL turns out to be invalid
    let download_url = match get_directory_path(source.layout, filetype, object_id)
        .and_then(|x| source.url.join(&x).ok())
    {
        Some(x) => x,
        None => return Box::new(Ok(None).into_future()),
    };

    let response = http::follow_redirects(
        {
            let mut builder = client::get(&download_url);
            for (key, value) in source.headers.iter() {
                if let Ok(key) = HeaderName::from_bytes(key.as_bytes()) {
                    builder.header(key, value.as_str());
                }
            }
            builder.header("user-agent", USER_AGENT);
            builder.finish().unwrap()
        },
        10,
    )
    .then(move |result| match result {
        Ok(response) => {
            if response.status().is_success() {
                log::info!("Success hitting {}", download_url);
                Ok(Some(Box::new(
                    response
                        .payload()
                        .map_err(|e| e.context(ObjectErrorKind::Io).into()),
                )
                    as Box<dyn Stream<Item = _, Error = _>>))
            } else {
                log::debug!(
                    "Unexpected status code from {}: {}",
                    download_url,
                    response.status()
                );
                Ok(None)
            }
        }
        Err(e) => {
            log::warn!("Skipping response from {}: {}", download_url, e);
            Ok(None)
        }
    });

    Box::new(response)
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
