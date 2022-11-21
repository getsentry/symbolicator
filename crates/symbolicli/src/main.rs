use std::fmt;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;

use symbolic::common::Language;
use symbolicator_service::services::symbolication::{
    StacktraceOrigin, SymbolicateStacktraces, SymbolicationActor,
};
use symbolicator_service::types::{
    CompleteObjectInfo, CompletedSymbolicationResponse, FrameTrust, RawFrame, RawObjectInfo,
    RawStacktrace, Scope, Signal,
};
use symbolicator_service::utils::addr::AddrMode;
use symbolicator_service::utils::hex::HexValue;
use symbolicator_sources::{SentrySourceConfig, SourceConfig, SourceId};

use anyhow::{bail, Context};
use reqwest::{header, Url};
use serde::Deserialize;
use tempfile::{NamedTempFile, TempPath};

mod settings;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let settings::Settings {
        event_id,
        project,
        org,
        auth_token,
        base_url,
        symbolicator_config,
    } = settings::Settings::get()?;

    let runtime = tokio::runtime::Handle::current();
    let (symbolication, _objects) =
        symbolicator_service::services::create_service(&symbolicator_config, runtime)
            .await
            .context("failed to start symbolication service")?;

    let mut headers = header::HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        header::HeaderValue::from_str(&format!("Bearer {auth_token}")).unwrap(),
    );

    let client = reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap();

    let project_source = SourceConfig::Sentry(Arc::new(SentrySourceConfig {
        id: SourceId::new("sentry:project"),
        token: auth_token,
        url: base_url
            .join(&format!("projects/{org}/{project}/files/dsyms/"))
            .unwrap(),
    }));
    let mut sources = vec![project_source.clone()];
    sources.extend(symbolicator_config.sources.iter().cloned());
    let sources = Arc::from(sources.into_boxed_slice());

    let scope = Scope::Scoped(project.clone());

    let res = match Local::parse(&event_id)? {
        Some(local) => local.process(&symbolication, scope, sources).await?,
        None => match get_attached_minidump(&client, &base_url, &org, &project, &event_id).await? {
            Some(minidump_url) => {
                tracing::info!("minidump attachment found");
                let minidump_path = download_minidump(&client, minidump_url).await?;
                tracing::info!(path = ?minidump_path, "minidump file downloaded");

                symbolication
                    .process_minidump(scope, minidump_path, sources)
                    .await?
            }

            None => {
                let event = get_event(&client, &base_url, &org, &project, &event_id).await?;

                let symbolication_request = create_symbolication_request(scope, sources, event)
                    .context("Event cannot be symbolicated")?;

                symbolication.symbolicate(symbolication_request).await?
            }
        },
    };

    println!("{}", serde_json::to_string(&res).unwrap());

    Ok(())
}

#[derive(Debug)]
enum Local {
    Event(Event),
    Minidump(TempPath),
}

impl Local {
    async fn process(
        self,
        symbolication: &SymbolicationActor,
        scope: Scope,
        sources: Arc<[SourceConfig]>,
    ) -> anyhow::Result<CompletedSymbolicationResponse> {
        match self {
            Local::Event(event) => {
                tracing::info!("event is a local event json file");
                let symbolication_request = create_symbolication_request(scope, sources, event)
                    .context("Event cannot be symbolicated")?;

                symbolication.symbolicate(symbolication_request).await
            }
            Local::Minidump(minidump_path) => {
                tracing::info!("event is a local minidump file");
                symbolication
                    .process_minidump(scope, minidump_path, sources)
                    .await
            }
        }
    }
}

impl Local {
    fn parse<P: AsRef<Path> + fmt::Debug>(path: &P) -> anyhow::Result<Option<Self>> {
        match std::fs::File::open(path) {
            Ok(mut file) => {
                let mut magic = [0; 4];
                file.read_exact(&mut magic)?;
                file.seek(SeekFrom::Start(0))?;

                if &magic == b"MDMP" || &magic == b"PMDM" {
                    let mut temp_file = NamedTempFile::new().unwrap();
                    std::io::copy(&mut file, &mut temp_file)
                        .context("Failed to write minidump to disk")?;

                    Ok(Some(Local::Minidump(temp_file.into_temp_path())))
                } else {
                    let event =
                        serde_json::from_reader(file).context("failed to parse event json file")?;
                    Ok(Some(Local::Event(event)))
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context(format!("Could not open event file at {:?}", path)),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct Attachment {
    r#type: String,
    id: String,
}

pub async fn get_attached_minidump(
    client: &reqwest::Client,
    base_url: &Url,
    org: &str,
    project: &str,
    event_id: &str,
) -> anyhow::Result<Option<Url>> {
    let attachments_url = base_url
        .join(&format!(
            "projects/{org}/{project}/events/{event_id}/attachments/"
        ))
        .unwrap();

    tracing::info!(url = %attachments_url, "fetching attachments");

    let response = client
        .get(attachments_url.clone())
        .send()
        .await
        .context("Failed to send request")?;

    let attachments: Vec<Attachment> = if response.status().is_success() {
        response
            .json()
            .await
            .context("Failed to decode attachments")?
    } else {
        bail!(format!(
            "Response from server: {}",
            response
                .status()
                .canonical_reason()
                .unwrap_or("unknown error")
        ));
    };

    let Some(minidump_id) = attachments
            .iter()
            .find(|attachment| attachment.r#type == "event.minidump")
            .map(|attachment| &attachment.id) else {
                return Ok(None);
        };

    let mut download_url = attachments_url.join(&format!("{minidump_id}/")).unwrap();
    download_url.query_pairs_mut().append_pair("download", "1");

    Ok(Some(download_url))
}

pub async fn download_minidump(
    client: &reqwest::Client,
    download_url: Url,
) -> anyhow::Result<TempPath> {
    tracing::info!(url = %download_url, "downloading minidump file");

    let response = client
        .get(download_url)
        .send()
        .await
        .context("Failed to send request")?;

    let minidump = if response.status().is_success() {
        response
            .bytes()
            .await
            .context("Failed to extract response body")?
    } else {
        bail!(format!(
            "Response from server: {}",
            response
                .status()
                .canonical_reason()
                .unwrap_or("unknown error")
        ));
    };

    let mut temp_file = NamedTempFile::new().unwrap();
    temp_file
        .write_all(&minidump)
        .context("Failed to write minidump to disk")?;

    Ok(temp_file.into_temp_path())
}

pub async fn get_event(
    client: &reqwest::Client,
    base_url: &Url,
    org: &str,
    project: &str,
    event_id: &str,
) -> anyhow::Result<Event> {
    let event_url = base_url
        .join(&format!("projects/{org}/{project}/events/{event_id}/json/"))
        .unwrap();

    let response = client
        .get(event_url.clone())
        .send()
        .await
        .context("Failed to send request")?;

    if response.status().is_success() {
        response.json().await.context("Failed to decode event")
    } else {
        bail!(format!(
            "Response from server: {}",
            response
                .status()
                .canonical_reason()
                .unwrap_or("unknown error")
        ));
    }
}

pub fn create_symbolication_request(
    scope: Scope,
    sources: Arc<[SourceConfig]>,
    event: Event,
) -> anyhow::Result<SymbolicateStacktraces> {
    let Event {
        debug_meta,
        exception,
        threads,
        signal,
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

    let stacktraces: Vec<_> = stacktraces
        .into_iter()
        .map(RawStacktrace::from)
        .filter(|stacktrace| !stacktrace.frames.is_empty())
        .collect();

    let modules: Vec<_> = debug_meta
        .images
        .into_iter()
        .map(CompleteObjectInfo::from)
        .collect();

    if modules.is_empty() {
        bail!("Event has no debug images");
    };

    if stacktraces.is_empty() {
        bail!("Event has no usable frames");
    };

    Ok(SymbolicateStacktraces {
        scope,
        signal,
        sources,
        origin: StacktraceOrigin::Symbolicate,
        stacktraces,
        modules,
    })
}

#[derive(Debug, Deserialize)]
pub struct Event {
    #[serde(default)]
    debug_meta: DebugMeta,
    exception: Option<Exceptions>,
    threads: Option<Threads>,
    signal: Option<Signal>,
}

#[derive(Debug, Deserialize, Default)]
struct DebugMeta {
    images: Vec<RawObjectInfo>,
}

#[derive(Debug, Deserialize)]
pub struct Image {
    #[serde(rename = "type")]
    pub ty: Option<String>,
    pub image_addr: Option<String>,
    pub image_size: Option<u64>,
    pub code_id: Option<String>,
    pub code_file: Option<String>,
    pub debug_id: Option<String>,
    pub debug_file: Option<String>,
    pub debug_checksum: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Exceptions {
    values: Vec<Exception>,
}

#[derive(Debug, Deserialize)]
struct Exception {
    stacktrace: Option<Stacktrace>,
}

#[derive(Debug, Deserialize)]
struct Threads {
    values: Vec<Thread>,
}

#[derive(Debug, Deserialize)]
struct Thread {
    stacktrace: Option<Stacktrace>,
}

#[derive(Debug, Deserialize)]
pub struct Stacktrace {
    pub frames: Vec<Frame>,
    #[serde(default)]
    pub is_requesting: bool,
}

impl From<Stacktrace> for RawStacktrace {
    fn from(stacktrace: Stacktrace) -> Self {
        let frames = stacktrace
            .frames
            .into_iter()
            .filter_map(to_raw_frame)
            .rev()
            .collect();

        Self {
            is_requesting: Some(stacktrace.is_requesting),
            frames,
            ..Default::default()
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct Frame {
    #[serde(default)]
    pub addr_mode: AddrMode,

    pub instruction_addr: Option<HexValue>,

    #[serde(default)]
    pub function_id: Option<HexValue>,

    #[serde(default)]
    pub package: Option<String>,

    pub lang: Option<Language>,

    pub symbol: Option<String>,

    pub sym_addr: Option<HexValue>,

    pub function: Option<String>,

    pub filename: Option<String>,

    pub abs_path: Option<String>,

    pub lineno: Option<u32>,

    #[serde(default)]
    pub pre_context: Vec<String>,

    pub context_line: Option<String>,

    #[serde(default)]
    pub post_context: Vec<String>,

    #[serde(default)]
    pub trust: FrameTrust,
}

fn to_raw_frame(value: Frame) -> Option<RawFrame> {
    Some(RawFrame {
        addr_mode: value.addr_mode,
        instruction_addr: value.instruction_addr?,
        function_id: value.function_id,
        package: value.package,
        lang: value.lang,
        symbol: value.symbol,
        sym_addr: value.sym_addr,
        function: value.function,
        filename: value.filename,
        abs_path: value.abs_path,
        lineno: value.lineno,
        pre_context: value.pre_context,
        context_line: value.context_line,
        post_context: value.post_context,
        trust: value.trust,
    })
}
