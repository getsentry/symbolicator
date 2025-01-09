//! Tool to run Minidumps or Sentry Events through a local Symbolicator.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek};
use std::path::PathBuf;

use ::reqwest::blocking::multipart;
use clap::Parser;
use reqwest::blocking as reqwest;
use serde_json::{to_string, Map, Value};
use symbolic::common::split_path;

#[path = "../../symbolicator-service/src/utils/hex.rs"]
mod hex;

use hex::HexValue;

/// Runs Minidumps or Sentry Events through Symbolicator.
#[derive(Debug, Parser)]
struct Cli {
    /// Path to the input Minidump or Event JSON.
    input: PathBuf,

    /// The URL of the Symbolicator to use.
    ///
    /// Defaults to `http://127.0.0.1:3021` if not provided.
    #[arg(short, long)]
    symbolicator: Option<String>,

    /// Whether to include DIF candidate information.
    #[arg(short, long)]
    dif_candidates: bool,

    /// Pretty-print the crashing thread in a human readable format.
    #[arg(short, long)]
    pretty: bool,
}

fn main() -> anyhow::Result<()> {
    let Cli {
        input,
        symbolicator,
        dif_candidates,
        pretty,
    } = Cli::parse();

    let client = reqwest::Client::new();
    let symbolicator = symbolicator.as_deref().unwrap_or("http://127.0.0.1:3021");

    let mut file = File::open(&input)?;

    let mut magic = [0; 4];
    file.read_exact(&mut magic)?;
    file.rewind()?;

    let req = if &magic == b"MDMP" || &magic == b"PMDM" {
        let req = client.post(format!("{symbolicator}/minidump"));

        let mut options = Map::new();
        options.insert("dif_candidates".into(), Value::Bool(dif_candidates));

        let mut form = multipart::Form::new();
        form = form.file("upload_file_minidump", input)?;
        form = form.text("options", to_string(&options).unwrap());

        req.multipart(form).send()
    } else {
        let req = client.post(format!("{symbolicator}/symbolicate"));

        let event = serde_json::from_reader(file)?;
        let mut json = event::massage_event_json(event);
        if dif_candidates {
            if let Some(obj) = json.as_object_mut() {
                obj.insert(
                    String::from("options"),
                    serde_json::json!({ "dif_candidates": true }),
                );
            }
        }

        req.json(&json).send()
    };

    if pretty {
        let payload: event::Payload = req?.json()?;
        let module_addr_by_code_file: HashMap<_, _> = payload
            .modules
            .into_iter()
            .filter_map(|module| {
                Some((
                    module.code_file?,
                    module
                        .image_addr
                        .and_then(|addr| addr.parse::<HexValue>().ok())?
                        .0,
                ))
            })
            .collect();

        let crashing_thread = payload.stacktraces.into_iter().find(|s| s.is_requesting);

        if let Some(thread) = crashing_thread {
            let mut frames = thread.frames.into_iter().peekable();
            while let Some(frame) = frames.next() {
                let is_inline = frame.instruction_addr.as_ref()
                    == frames
                        .peek()
                        .and_then(|next_frame| next_frame.instruction_addr.as_ref());
                let trust = if is_inline {
                    "inline"
                } else {
                    frame.trust.as_deref().unwrap()
                };

                let instruction_addr = frame.instruction_addr.unwrap().0;

                print!("{trust:<8} {instruction_addr:#018x}");

                if let Some(module_file) = frame.package {
                    let module_addr = module_addr_by_code_file[&module_file];
                    let module_file = split_path(&module_file).1;
                    let module_rel_addr = instruction_addr - module_addr;

                    print!(" {:<30}", format!("{module_file:} +{module_rel_addr:#x}"));
                }

                if let Some(func) = frame.function.or(frame.symbol) {
                    print!(" {func}");

                    if let Some(sym_addr) = frame
                        .sym_addr
                        .and_then(|addr| addr.parse::<HexValue>().ok())
                    {
                        let sym_rel_addr = instruction_addr - sym_addr.0;

                        print!(" +{sym_rel_addr:#x}");
                    }
                }

                if let Some(file) = frame.filename {
                    let line = frame.lineno.unwrap_or(0);
                    print!(" ({file}:{line})");
                }

                println!();
            }
        }
    } else {
        req?.copy_to(&mut std::io::stdout())?;
    }

    Ok(())
}

mod event {
    use super::HexValue;
    use serde::{Deserialize, Serialize};

    /// Brings a Sentry JSON into the form suitable for Symbolicator.
    ///
    /// The `debug_meta.images` is renamed to `modules`, and it gathers all the `stacktrace`s of
    /// exceptions and threads into the `stacktraces`. The stack traces are reversed, to match the
    /// minidump output.
    pub fn massage_event_json(event: Event) -> serde_json::Value {
        let Event {
            debug_meta,
            exception,
            threads,
            ..
        } = event;

        let mut stacktraces = vec![];
        if let Some(mut excs) = exception.map(|excs| excs.values) {
            stacktraces.extend(excs.iter_mut().filter_map(|exc| exc.stacktrace.take()));
        }
        if let Some(mut threads) = threads.map(|threads| threads.values) {
            stacktraces.extend(
                threads
                    .iter_mut()
                    .filter_map(|thread| thread.stacktrace.take()),
            );
        }

        for stacktrace in &mut stacktraces {
            let frames = std::mem::take(&mut stacktrace.frames);
            stacktrace.frames = frames
                .into_iter()
                .rev()
                .filter(|frame| frame.instruction_addr.is_some())
                .collect();
        }

        serde_json::json!({
            "modules": debug_meta.images,
            "stacktraces": stacktraces,
        })
    }

    #[derive(Deserialize, Serialize)]
    pub struct Payload {
        pub modules: Vec<Image>,
        pub stacktraces: Vec<Stacktrace>,
    }

    #[derive(Deserialize)]
    pub struct Event {
        debug_meta: DebugMeta,
        exception: Option<Exceptions>,
        threads: Option<Threads>,
    }

    #[derive(Deserialize)]
    struct DebugMeta {
        images: Vec<Image>,
    }

    #[derive(Deserialize, Serialize)]
    pub struct Image {
        #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
        pub ty: Option<String>,

        #[serde(skip_serializing_if = "Option::is_none")]
        pub image_addr: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub image_size: Option<u64>,

        #[serde(skip_serializing_if = "Option::is_none")]
        pub code_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub code_file: Option<String>,

        #[serde(skip_serializing_if = "Option::is_none")]
        pub debug_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub debug_file: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub debug_checksum: Option<String>,
    }

    #[derive(Deserialize)]
    struct Exceptions {
        values: Vec<Exception>,
    }

    #[derive(Deserialize)]
    struct Exception {
        stacktrace: Option<Stacktrace>,
    }

    #[derive(Deserialize)]
    struct Threads {
        values: Vec<Thread>,
    }

    #[derive(Deserialize)]
    struct Thread {
        stacktrace: Option<Stacktrace>,
    }

    #[derive(Deserialize, Serialize)]
    pub struct Stacktrace {
        pub frames: Vec<Frame>,
        #[serde(default)]
        pub is_requesting: bool,
    }

    #[derive(Deserialize, Serialize)]
    pub struct Frame {
        #[serde(skip_serializing_if = "Option::is_none")]
        pub function: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub symbol: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub package: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub instruction_addr: Option<HexValue>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub function_id: Option<HexValue>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub addr_mode: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub trust: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub sym_addr: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub filename: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub lineno: Option<u32>,
    }
}
