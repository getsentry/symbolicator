use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use symbolicator_js::interface::{JsModule, JsStacktrace, SymbolicateJsStacktraces};
use symbolicator_js::SourceMapService;
use symbolicator_native::interface::{RawStacktrace, StacktraceOrigin, SymbolicateStacktraces};
use symbolicator_native::SymbolicationActor;
use symbolicator_service::download::SourceConfig;
use symbolicator_service::types::{RawObjectInfo, Scope};

#[derive(Debug, Deserialize, Serialize)]
pub struct WorkloadsConfig {
    pub workloads: Vec<Workload>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Workload {
    pub concurrency: usize,
    #[serde(flatten)]
    pub payload: Payload,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Payload {
    Minidump(PathBuf),
    Event(PathBuf),
    Js { source: PathBuf, event: PathBuf },
}

#[derive(Debug, Deserialize)]
struct EventFile {
    stacktraces: Vec<RawStacktrace>,
    modules: Vec<RawObjectInfo>,
}

#[derive(Debug, Deserialize)]
struct JsEventFile {
    stacktraces: Vec<JsStacktrace>,
    modules: Vec<JsModule>,
}

#[derive(Clone)]
pub struct MinidumpPayload {
    scope: Scope,
    minidump_file: PathBuf,
    sources: Arc<[SourceConfig]>,
}

pub enum ParsedPayload {
    Minidump(MinidumpPayload),
    Event(SymbolicateStacktraces),
    Js(symbolicator_test::Server, SymbolicateJsStacktraces),
}

pub fn prepare_payload(
    scope: Scope,
    sources: Arc<[SourceConfig]>,
    payload: Payload,
) -> ParsedPayload {
    match payload {
        Payload::Minidump(path) => ParsedPayload::Minidump(MinidumpPayload {
            scope,
            sources,

            minidump_file: path,
        }),
        Payload::Event(path) => {
            let EventFile {
                stacktraces,
                modules,
            } = read_json(path);
            let modules = modules.into_iter().map(From::from).collect();

            ParsedPayload::Event(SymbolicateStacktraces {
                platform: None,
                scope,
                signal: None,
                sources,
                origin: StacktraceOrigin::Symbolicate,
                apply_source_context: true,
                scraping: Default::default(),
                stacktraces,
                modules,
            })
        }
        Payload::Js { source, event } => {
            let parent = source.parent().unwrap();
            let source: serde_json::Value = read_json(&source);
            let (srv, source) = symbolicator_test::sourcemap_server(parent, move |url, _query| {
                let lookup = source
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|entry| {
                        let mut entry = entry.clone();
                        let map = entry.as_object_mut().unwrap();
                        let url = map["url"].as_str().unwrap().replace("{url}", url);
                        map["url"] = serde_json::Value::String(url);
                        entry
                    })
                    .collect();
                serde_json::Value::Array(lookup)
            });

            let JsEventFile {
                stacktraces,
                modules,
            } = read_json(event);

            ParsedPayload::Js(
                srv,
                SymbolicateJsStacktraces {
                    platform: None,
                    scope,
                    source: Arc::new(source),
                    release: Some("some-release".into()),
                    dist: None,
                    scraping: Default::default(),
                    apply_source_context: true,
                    stacktraces,
                    modules,
                },
            )
        }
    }
}

pub fn read_json<T: DeserializeOwned>(path: impl AsRef<Path>) -> T {
    let file = std::fs::File::open(path).unwrap();
    let reader = BufReader::new(file);
    serde_json::from_reader(reader).unwrap()
}

pub async fn process_payload(
    symbolication: &(SymbolicationActor, SourceMapService),
    workload: &ParsedPayload,
) {
    match workload {
        ParsedPayload::Minidump(payload) => {
            let MinidumpPayload {
                scope,
                minidump_file,
                sources,
            } = payload;

            // processing a minidump requires a tempfile that can be persisted -_-
            // so that means we have to make a copy of our minidump
            let mut temp_file = tempfile::Builder::new();
            temp_file.prefix("minidump").suffix(".dmp");
            let temp_file = temp_file.tempfile().unwrap();
            let (temp_file, temp_path) = temp_file.into_parts();
            let mut temp_file = tokio::fs::File::from_std(temp_file);

            let mut minidump_file = tokio::fs::File::open(minidump_file).await.unwrap();

            tokio::io::copy(&mut minidump_file, &mut temp_file)
                .await
                .unwrap();

            symbolication
                .0
                .process_minidump(
                    None,
                    scope.clone(),
                    temp_path,
                    Arc::clone(sources),
                    Default::default(),
                )
                .await
                .unwrap();
        }
        ParsedPayload::Event(payload) => {
            symbolication.0.symbolicate(payload.clone()).await.unwrap();
        }
        ParsedPayload::Js(_srv, payload) => {
            symbolication.1.symbolicate_js(payload.clone()).await;
        }
    };
}
