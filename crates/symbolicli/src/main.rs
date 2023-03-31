use std::fmt;
use std::io::{Read, Seek};
use std::path::Path;
use std::sync::Arc;

use output::{print_compact, print_pretty};
use remote::EventKey;

use settings::Mode;
use symbolicator_service::services::symbolication::SymbolicationActor;
use symbolicator_service::types::{CompletedSymbolicationResponse, Scope};
use symbolicator_sources::{SentrySourceConfig, SourceConfig, SourceId};

use anyhow::{Context, Result};
use reqwest::header;
use tempfile::{NamedTempFile, TempPath};
use tracing_subscriber::filter;
use tracing_subscriber::prelude::*;

mod settings;

#[tokio::main]
async fn main() -> Result<()> {
    let settings::Settings {
        event_id,
        symbolicator_config,
        output_format,
        log_level,
        mode,
    } = settings::Settings::get()?;

    let filter = filter::Targets::new().with_targets(vec![
        ("symbolicator_service", log_level),
        ("symbolicli", log_level),
    ]);

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(filter)
        .init();

    let runtime = tokio::runtime::Handle::current();
    let (symbolication, _objects) =
        symbolicator_service::services::create_service(&symbolicator_config, runtime)
            .context("failed to start symbolication service")?;

    let mut sources = vec![];
    if let Mode::Online {
        ref org,
        ref project,
        ref base_url,
        ref auth_token,
    } = mode
    {
        let project_source = SourceConfig::Sentry(Arc::new(SentrySourceConfig {
            id: SourceId::new("sentry:project"),
            token: auth_token.clone(),
            url: base_url
                .join(&format!("projects/{org}/{project}/files/dsyms/"))
                .unwrap(),
        }));

        sources.push(project_source);
    }
    sources.extend(symbolicator_config.sources.iter().cloned());
    let sources = Arc::from(sources.into_boxed_slice());

    let scope = match mode {
        Mode::Online { ref project, .. } => Scope::Scoped(project.clone()),
        Mode::Offline => Scope::Global,
    };

    let res = match Payload::parse(&event_id)? {
        Some(local) => {
            tracing::info!("successfully parsed local event");
            local.process(&symbolication, scope, sources).await?
        }
        None => {
            tracing::info!("event not found in local file system");
            let Mode::Online { base_url, org, project, auth_token } = mode else {
                anyhow::bail!("Event not found in local file system and `symbolicli` is in offline mode. Stopping.");
            };

            let mut headers = header::HeaderMap::new();
            headers.insert(
                header::AUTHORIZATION,
                header::HeaderValue::from_str(&format!("Bearer {auth_token}")).unwrap(),
            );

            let client = reqwest::Client::builder()
                .default_headers(headers)
                .build()
                .unwrap();

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
    ) -> Result<CompletedSymbolicationResponse> {
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

    fn parse<P: AsRef<Path> + fmt::Debug>(path: &P) -> Result<Option<Self>> {
        match std::fs::File::open(path) {
            Ok(mut file) => {
                let mut magic = [0; 4];
                file.read_exact(&mut magic)?;
                file.rewind()?;

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
            Err(e) => Err(e).context(format!("Could not open event file at {path:?}")),
        }
    }

    async fn get_remote<'a>(client: &reqwest::Client, key: EventKey<'a>) -> Result<Self> {
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

mod output {
    use std::{collections::HashMap, iter::Peekable, vec::IntoIter};

    use prettytable::{cell, format::consts::FORMAT_CLEAN, row, Row, Table};
    use symbolic::common::split_path;
    use symbolicator_service::types::{
        CompleteObjectInfo, CompletedSymbolicationResponse, FrameTrust, SymbolicatedFrame,
    };

    #[derive(Clone, Debug)]
    struct FrameData {
        instruction_addr: u64,
        trust: &'static str,
        module: Option<(String, u64)>,
        func: Option<(String, u64)>,
        file: Option<(String, u32)>,
    }

    #[derive(Clone, Debug)]
    struct Frames {
        inner: Peekable<IntoIter<SymbolicatedFrame>>,
        modules: HashMap<String, CompleteObjectInfo>,
    }

    impl Iterator for Frames {
        type Item = FrameData;

        fn next(&mut self) -> Option<Self::Item> {
            let frame = self.inner.next()?;
            let is_inline = Some(frame.raw.instruction_addr)
                == self
                    .inner
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

            let module = frame.raw.package.map(|module_file| {
                let module_addr = self.modules[&module_file].raw.image_addr.0;
                let module_file = split_path(&module_file).1.into();
                let module_rel_addr = instruction_addr - module_addr;

                (module_file, module_rel_addr)
            });

            let func = frame.raw.function.or(frame.raw.symbol).map(|func| {
                let sym_rel_addr = frame
                    .raw
                    .sym_addr
                    .map(|sym_addr| instruction_addr - sym_addr.0)
                    .unwrap_or_default();

                (func, sym_rel_addr)
            });

            let file = frame.raw.filename.map(|file| {
                let line = frame.raw.lineno.unwrap_or(0);

                (file, line)
            });

            Some(FrameData {
                instruction_addr,
                trust,
                module,
                func,
                file,
            })
        }
    }

    fn get_crashing_thread_frames(mut response: CompletedSymbolicationResponse) -> Frames {
        let modules: HashMap<_, _> = response
            .modules
            .into_iter()
            .filter_map(|module| Some((module.raw.code_file.clone()?, module)))
            .collect();

        let crashing_thread_idx = response
            .stacktraces
            .iter()
            .position(|s| s.is_requesting.unwrap_or(false))
            .unwrap_or(0);

        let crashing_thread = response.stacktraces.swap_remove(crashing_thread_idx);
        Frames {
            inner: crashing_thread.frames.into_iter().peekable(),
            modules,
        }
    }

    pub fn print_compact(response: CompletedSymbolicationResponse) {
        if response.stacktraces.is_empty() {
            return;
        }

        let mut table = Table::new();
        table.set_format(*FORMAT_CLEAN);
        table.set_titles(
            row![b => "Trust", "Instruction", "Module File", "", "Function", "", "File"],
        );

        for frame in get_crashing_thread_frames(response) {
            let mut row = Row::empty();
            let FrameData {
                instruction_addr,
                trust,
                module,
                func,
                file,
            } = frame;

            row.add_cell(cell!(trust));
            row.add_cell(cell!(r->format!("{instruction_addr:#x}")));

            match module {
                Some((module_file, module_offset)) => {
                    row.add_cell(cell!(module_file));
                    row.add_cell(cell!(r->format!("+{module_offset:#x}")));
                }
                None => row.add_cell(cell!("").with_hspan(2)),
            }

            match func {
                Some((func, func_offset)) => {
                    row.add_cell(cell!(func));
                    row.add_cell(cell!(r->format!(" + {func_offset:#x}")));
                }
                None => row.add_cell(cell!("").with_hspan(2)),
            }

            match file {
                Some((name, line)) => row.add_cell(cell!(format!("{name}:{line}"))),
                None => row.add_cell(cell!("")),
            }

            table.add_row(row);
        }

        table.printstd();
    }

    pub fn print_pretty(response: CompletedSymbolicationResponse) {
        if response.stacktraces.is_empty() {
            return;
        }

        let mut table = Table::new();
        table.set_format(*prettytable::format::consts::FORMAT_CLEAN);

        for (i, frame) in get_crashing_thread_frames(response).enumerate() {
            let FrameData {
                instruction_addr,
                trust,
                module,
                func,
                file,
            } = frame;

            let title_cell = cell!(lb->format!("Frame #{i}")).with_hspan(2);
            table.add_row(Row::new(vec![title_cell]));

            table.add_row(row![r->"  Trust:", trust]);
            table.add_row(row![r->"  Instruction:", format!("{instruction_addr:#0x}")]);

            if let Some((module_file, module_offset)) = module {
                table.add_row(row![
                    r->"  Module:",
                    format!("{module_file} +{module_offset:#0x}")
                ]);
            }

            if let Some((func, func_offset)) = func {
                table.add_row(row![r->"  Function:", format!("{func} + {func_offset:#x}")]);
            }

            if let Some((name, line)) = file {
                table.add_row(row![r->"  File:", format!("{name}:{line}")]);
            }
        }
        table.printstd();
    }
}

mod remote {
    use std::io::Write;

    use anyhow::{bail, Context, Result};
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
    ) -> Result<Option<Url>> {
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
    ) -> Result<TempPath> {
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

    pub async fn download_event<'a>(client: &reqwest::Client, key: EventKey<'a>) -> Result<Event> {
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

        source_link: Option<String>,

        in_app: Option<bool>,

        #[serde(default)]
        trust: FrameTrust,
    }

    fn to_raw_frame(value: Frame) -> Option<RawFrame> {
        Some(RawFrame {
            addr_mode: value.addr_mode,
            instruction_addr: value.instruction_addr?,
            adjust_instruction_addr: None,
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
            source_link: value.source_link,
            in_app: value.in_app,
            trust: value.trust,
        })
    }
}
