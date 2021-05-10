//! Tool to run Minidumps or Sentry Events through a local Symbolicator.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use ::reqwest::blocking::multipart;
use reqwest::blocking as reqwest;
use structopt::StructOpt;

/// Runs Minidumps or Sentry Events through Symbolicator.
#[derive(Debug, StructOpt)]
struct Cli {
    /// Path to the input Minidump or Event JSON.
    input: PathBuf,

    /// The URL of the Symbolicator to use.
    ///
    /// Defaults to `http://127.0.0.1:3021` if not provided.
    #[structopt(short, long)]
    symbolicator: Option<String>,

    /// Whether to include DIF candidate information.
    #[structopt(short, long)]
    dif_candidates: bool,
}

fn main() -> Result<(), anyhow::Error> {
    let Cli {
        input,
        symbolicator,
        dif_candidates,
    } = Cli::from_args();

    let client = reqwest::Client::new();
    let symbolicator = symbolicator.as_deref().unwrap_or("http://127.0.0.1:3021");

    let mut file = File::open(&input)?;

    let mut magic = [0; 4];
    file.read_exact(&mut magic)?;
    file.seek(SeekFrom::Start(0))?;

    let req = if &magic == b"MDMP" {
        let req = client.post(&format!("{}/minidump", symbolicator));
        let mut form = multipart::Form::new();

        if dif_candidates {
            form = form.text("options", r#"{"dif_candidates":true}"#);
        }
        form = form.file("upload_file_minidump", input)?;

        req.multipart(form).send()
    } else {
        let req = client.post(&format!("{}/symbolicate", symbolicator));

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

    req?.copy_to(&mut std::io::stdout())?;

    Ok(())
}

mod event {
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
    struct Image {
        #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
        ty: Option<String>,

        #[serde(skip_serializing_if = "Option::is_none")]
        image_addr: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        image_size: Option<u64>,

        #[serde(skip_serializing_if = "Option::is_none")]
        code_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        code_file: Option<String>,

        #[serde(skip_serializing_if = "Option::is_none")]
        debug_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        debug_file: Option<String>,
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
    struct Stacktrace {
        frames: Vec<Frame>,
    }

    #[derive(Deserialize, Serialize)]
    struct Frame {
        #[serde(skip_serializing_if = "Option::is_none")]
        function: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        symbol: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        package: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        instruction_addr: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        addr_mode: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        trust: Option<String>,
    }
}
