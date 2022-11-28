use std::collections::HashMap;
use std::fmt;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use remote::EventKey;
use symbolic::common::split_path;
use symbolicator_service::services::symbolication::SymbolicationActor;
use symbolicator_service::types::{CompletedSymbolicationResponse, FrameTrust, Scope};
use symbolicator_sources::{SentrySourceConfig, SourceConfig, SourceId};

use anyhow::Context;
use prettytable::format::consts::FORMAT_CLEAN;
use prettytable::{cell, row, Row, Table};
use reqwest::header;
use tempfile::{NamedTempFile, TempPath};

mod settings;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let settings::Settings {
        event_id,
        project,
        org,
        auth_token,
        base_url,
        symbolicator_config,
        output_format,
    } = settings::Settings::get()?;

    tracing_subscriber::fmt::init();

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

    let res = match Payload::parse(&event_id)? {
        Some(local) => {
            tracing::info!("successfully parsed local event");
            local.process(&symbolication, scope, sources).await?
        }
        None => {
            tracing::info!("event not found in local file system");
            let key = EventKey {
                base_url: &base_url,
                org: &org,
                project: &project,
                event_id: &event_id,
            };
            let remote = Payload::get_remote(&client, key).await?;
            remote.process(&symbolication, scope, sources).await?
        }
    };

    match output_format {
        settings::OutputFormat::Json => println!("{}", serde_json::to_string(&res).unwrap()),
        settings::OutputFormat::Compact => print_compact(res),
        settings::OutputFormat::Pretty => print_pretty(res),
    }

    Ok(())
}

fn print_compact(mut response: CompletedSymbolicationResponse) {
    if response.stacktraces.is_empty() {
        return;
    }

    let module_addr_by_code_file: HashMap<_, _> = response
        .modules
        .into_iter()
        .filter_map(|module| Some((module.raw.code_file.clone()?, module)))
        .collect();

    let crashing_thread_idx = response
        .stacktraces
        .iter()
        .position(|s| s.is_requesting.unwrap_or(false))
        .unwrap_or(0);

    let thread = response.stacktraces.swap_remove(crashing_thread_idx);

    let mut frames = thread.frames.into_iter().peekable();

    let mut table = Table::new();
    table.set_format(*FORMAT_CLEAN);
    table.set_titles(row![b => "Trust", "Instruction", "Module File", "", "Function", "", "File"]);
    while let Some(frame) = frames.next() {
        let mut row = Row::empty();
        let is_inline = Some(frame.raw.instruction_addr)
            == frames
                .peek()
                .map(|next_frame| next_frame.raw.instruction_addr);

        let trust = if is_inline {
            "inline"
        } else {
            match frame.raw.trust {
                FrameTrust::None => "none",
                FrameTrust::Scan => "scan",
                FrameTrust::CfiScan => "cfiscan",
                FrameTrust::Fp => "fp",
                FrameTrust::Cfi => "cfi",
                FrameTrust::PreWalked => "prewalked",
                FrameTrust::Context => "context",
            }
        };

        let instruction_addr = frame.raw.instruction_addr.0;

        row.add_cell(cell!(trust));
        row.add_cell(cell!(r->format!("{instruction_addr:#x}")));

        match frame.raw.package {
            Some(module_file) => {
                let module_addr = module_addr_by_code_file[&module_file].raw.image_addr.0;
                let module_file = split_path(&module_file).1;
                let module_rel_addr = instruction_addr - module_addr;

                row.add_cell(cell!(module_file));
                row.add_cell(cell!(r->format!("+{module_rel_addr:#x}")));
            }
            None => row.add_cell(cell!("").with_hspan(2)),
        }

        match frame.raw.function.or(frame.raw.symbol) {
            Some(func) => {
                let sym_rel_addr = frame
                    .raw
                    .sym_addr
                    .map(|sym_addr| format!(" +{:#x}", instruction_addr - sym_addr.0))
                    .unwrap_or_default();

                row.add_cell(cell!(func));
                row.add_cell(cell!(r->sym_rel_addr));
            }
            None => row.add_cell(cell!("").with_hspan(2)),
        }

        if let Some(file) = frame.raw.filename {
            let line = frame.raw.lineno.unwrap_or(0);

            row.add_cell(cell!(format!("{file}:{line}")));
        }

        table.add_row(row);
    }

    table.printstd();
}

fn print_pretty(mut response: CompletedSymbolicationResponse) {
    if response.stacktraces.is_empty() {
        return;
    }

    let module_addr_by_code_file: HashMap<_, _> = response
        .modules
        .into_iter()
        .filter_map(|module| Some((module.raw.code_file.clone()?, module)))
        .collect();

    let crashing_thread_idx = response
        .stacktraces
        .iter()
        .position(|s| s.is_requesting.unwrap_or(false))
        .unwrap_or(0);

    let thread = response.stacktraces.swap_remove(crashing_thread_idx);

    let mut table = Table::new();
    table.set_format(*prettytable::format::consts::FORMAT_CLEAN);
    let mut frames = thread.frames.into_iter().enumerate().peekable();
    while let Some((i, frame)) = frames.next() {
        let is_inline = Some(frame.raw.instruction_addr)
            == frames
                .peek()
                .map(|(_, next_frame)| next_frame.raw.instruction_addr);

        let trust = if is_inline {
            "inline"
        } else {
            match frame.raw.trust {
                FrameTrust::None => "none",
                FrameTrust::Scan => "scan",
                FrameTrust::CfiScan => "cfiscan",
                FrameTrust::Fp => "fp",
                FrameTrust::Cfi => "cfi",
                FrameTrust::PreWalked => "prewalked",
                FrameTrust::Context => "context",
            }
        };

        let title_cell = cell!(lb->format!("Frame #{i}")).with_hspan(2);
        table.add_row(Row::new(vec![title_cell]));

        let instruction_addr = frame.raw.instruction_addr.0;

        table.add_row(row![r->"  Trust:", trust]);
        table.add_row(row![r->"  Instruction:", format!("{instruction_addr:#0x}")]);
        if let Some(module_file) = frame.raw.package {
            let module_addr = module_addr_by_code_file[&module_file].raw.image_addr.0;
            let module_file = split_path(&module_file).1;
            let module_rel_addr = instruction_addr - module_addr;

            table.add_row(row![
                r->"  Module:",
                format!("{module_file} +{module_rel_addr:#0x}")
            ]);
        }

        if let Some(func) = frame.raw.function.or(frame.raw.symbol) {
            let sym_addr = frame
                .raw
                .sym_addr
                .map(|sym_addr| format!(" + {:#x}", instruction_addr - sym_addr.0))
                .unwrap_or_default();

            table.add_row(row![r->"  Function:", format!("{func}{sym_addr}")]);
        }

        if let Some(file) = frame.raw.filename {
            let line = frame.raw.lineno.unwrap_or(0);

            table.add_row(row![r->"  File:", format!("{file}:{line}")]);
        }

        table.add_empty_row();
    }
    table.printstd();
}

#[derive(Debug)]
enum Payload {
    Event(event::Event),
    Minidump(TempPath),
}

impl Payload {
    async fn process(
        self,
        symbolication: &SymbolicationActor,
        scope: Scope,
        sources: Arc<[SourceConfig]>,
    ) -> anyhow::Result<CompletedSymbolicationResponse> {
        match self {
            Payload::Event(event) => {
                let symbolication_request =
                    event::create_symbolication_request(scope, sources, event)
                        .context("Event cannot be symbolicated")?;

                tracing::info!("symbolicating event");
                symbolication.symbolicate(symbolication_request).await
            }
            Payload::Minidump(minidump_path) => {
                tracing::info!("symbolicating minidump");
                symbolication
                    .process_minidump(scope, minidump_path, sources)
                    .await
            }
        }
    }

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

                    Ok(Some(Payload::Minidump(temp_file.into_temp_path())))
                } else {
                    let event =
                        serde_json::from_reader(file).context("failed to parse event json file")?;
                    Ok(Some(Payload::Event(event)))
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context(format!("Could not open event file at {:?}", path)),
        }
    }

    async fn get_remote<'a>(client: &reqwest::Client, key: EventKey<'a>) -> anyhow::Result<Self> {
        tracing::info!("trying to resolve event remotely");
        let event = remote::download_event(client, key).await?;
        tracing::info!("event json file downloaded");

        match remote::get_attached_minidump(client, key).await? {
            Some(minidump_url) => {
                tracing::info!("minidump attachment found");
                let minidump_path = remote::download_minidump(client, minidump_url).await?;
                tracing::info!(path = ?minidump_path, "minidump file downloaded");

                Ok(Payload::Minidump(minidump_path))
            }

            None => {
                tracing::info!("no minidump attachment found");
                Ok(Self::Event(event))
            }
        }
    }
}

mod remote {
    use std::io::Write;

    use anyhow::{bail, Context};
    use reqwest::{StatusCode, Url};
    use serde::Deserialize;
    use tempfile::{NamedTempFile, TempPath};

    use crate::event::Event;

    #[derive(Clone, Copy, Debug)]
    pub struct EventKey<'a> {
        pub base_url: &'a Url,
        pub org: &'a str,
        pub project: &'a str,
        pub event_id: &'a str,
    }

    #[derive(Debug, Clone, Deserialize)]
    struct Attachment {
        r#type: String,
        id: String,
    }

    pub async fn get_attached_minidump<'a>(
        client: &reqwest::Client,
        key: EventKey<'a>,
    ) -> anyhow::Result<Option<Url>> {
        let EventKey {
            base_url,
            org,
            project,
            event_id,
        } = key;
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

    pub async fn download_event<'a>(
        client: &reqwest::Client,
        key: EventKey<'a>,
    ) -> anyhow::Result<Event> {
        let EventKey {
            base_url,
            org,
            project,
            event_id,
        } = key;
        let event_url = base_url
            .join(&format!("projects/{org}/{project}/events/{event_id}/json/"))
            .unwrap();

        tracing::info!(url = %event_url, "downloading event json");
        let response = client
            .get(event_url.clone())
            .send()
            .await
            .context("Failed to send request")?;

        if response.status().is_success() {
            response.json().await.context("Failed to decode event")
        } else if response.status() == StatusCode::NOT_FOUND {
            bail!("Invalid event ID");
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
}

mod event {
    use std::sync::Arc;

    use anyhow::bail;
    use serde::Deserialize;
    use symbolic::common::Language;
    use symbolicator_service::services::symbolication::{StacktraceOrigin, SymbolicateStacktraces};
    use symbolicator_service::types::{
        CompleteObjectInfo, FrameTrust, RawFrame, RawObjectInfo, RawStacktrace, Scope, Signal,
    };
    use symbolicator_service::utils::{addr::AddrMode, hex::HexValue};
    use symbolicator_sources::SourceConfig;

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
    struct Stacktrace {
        frames: Vec<Frame>,
        #[serde(default)]
        is_requesting: bool,
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
    struct Frame {
        #[serde(default)]
        addr_mode: AddrMode,

        instruction_addr: Option<HexValue>,

        #[serde(default)]
        function_id: Option<HexValue>,

        #[serde(default)]
        package: Option<String>,

        lang: Option<Language>,

        symbol: Option<String>,

        sym_addr: Option<HexValue>,

        function: Option<String>,

        filename: Option<String>,

        abs_path: Option<String>,

        lineno: Option<u32>,

        #[serde(default)]
        pre_context: Vec<String>,

        context_line: Option<String>,

        #[serde(default)]
        post_context: Vec<String>,

        #[serde(default)]
        trust: FrameTrust,
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
}
