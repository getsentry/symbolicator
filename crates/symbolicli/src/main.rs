use std::io::{Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{fmt, io};

use event::{create_js_symbolication_request, create_native_symbolication_request};
use output::{print_compact, print_pretty};
use remote::EventKey;

use settings::Mode;
use symbolicator_js::SourceMapService;
use symbolicator_native::SymbolicationActor;
use symbolicator_service::config::Config;
use symbolicator_service::services::SharedServices;
use symbolicator_service::types::Scope;
use symbolicator_sources::{
    CommonSourceConfig, DirectoryLayout, DirectoryLayoutType, FilesystemSourceConfig,
    SentrySourceConfig, SourceConfig, SourceId,
};

use anyhow::{Context, Result};
use reqwest::header;
use tempfile::{NamedTempFile, TempPath};
use tracing_subscriber::filter;
use tracing_subscriber::prelude::*;

use crate::output::CompletedResponse;

mod output;
mod settings;

#[tokio::main]
async fn main() -> Result<()> {
    let settings::Settings {
        event_id,
        symbolicator_config,
        output_format,
        log_level,
        mode,
        symbols,
    } = settings::Settings::get()?;

    let filter = filter::Targets::new().with_targets(vec![
        ("symbolicator_service", log_level),
        ("symbolicli", log_level),
    ]);

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_writer(io::stderr))
        .with(filter)
        .init();

    let runtime = tokio::runtime::Handle::current();

    let shared_services = SharedServices::new(symbolicator_config, runtime)
        .context("failed to start symbolication service")?;
    let native = SymbolicationActor::new(&shared_services);
    let js = SourceMapService::new(&shared_services);
    let symbolicator_config = shared_services.config;

    let scope = match mode {
        Mode::Online { ref project, .. } => Scope::Scoped(Arc::from(project.as_str())),
        Mode::Offline => Scope::Global,
    };

    let payload = match Payload::parse(&event_id)? {
        Some(local) => local,

        None => {
            tracing::info!("event not found in local file system");
            let Mode::Online {
                ref base_url,
                ref org,
                ref project,
                ref auth_token,
                ..
            } = mode
            else {
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
                base_url,
                org,
                project,
                event_id: &event_id,
            };
            Payload::get_remote(&client, key).await?
        }
    };

    let res = match payload {
        Payload::Event(event) if event.platform.is_js() => {
            let Mode::Online {
                ref org,
                ref project,
                ref base_url,
                ref auth_token,
                scraping_enabled,
            } = mode
            else {
                anyhow::bail!("JavaScript symbolication is not supported in offline mode.");
            };

            let source = Arc::new(SentrySourceConfig {
                id: SourceId::new("sentry:project"),
                token: auth_token.clone(),
                url: base_url
                    .join(&format!("projects/{org}/{project}/artifact-lookup/"))
                    .unwrap(),
            });

            let request = create_js_symbolication_request(scope, source, event, scraping_enabled)
                .context("Event cannot be symbolicated")?;

            tracing::info!("symbolicating event");

            CompletedResponse::JsSymbolication(js.symbolicate_js(request).await)
        }

        Payload::Event(event) if event.platform.is_native() => {
            let dsym_sources = prepare_dsym_sources(mode, &symbolicator_config, symbols);
            let request = create_native_symbolication_request(scope, dsym_sources, event)
                .context("Event cannot be symbolicated")?;

            tracing::info!("symbolicating event");

            let res = native.symbolicate(request).await?;
            CompletedResponse::NativeSymbolication(res)
        }
        Payload::Minidump(minidump_path) => {
            let dsym_sources = prepare_dsym_sources(mode, &symbolicator_config, symbols);
            tracing::info!("symbolicating minidump");
            let res = native
                .process_minidump(None, scope, minidump_path, dsym_sources, Default::default())
                .await?;
            CompletedResponse::NativeSymbolication(res)
        }
        Payload::Event(event) => anyhow::bail!(
            "Cannot symbolicate event: invalid platform {}",
            event.platform
        ),
    };

    match output_format {
        settings::OutputFormat::Json => match res {
            CompletedResponse::NativeSymbolication(res) => {
                println!("{}", serde_json::to_string(&res).unwrap())
            }
            CompletedResponse::JsSymbolication(res) => {
                println!("{}", serde_json::to_string(&res).unwrap())
            }
        },
        settings::OutputFormat::Compact => print_compact(res),
        settings::OutputFormat::Pretty => print_pretty(res),
    }

    Ok(())
}

fn prepare_dsym_sources(
    mode: Mode,
    symbolicator_config: &Config,
    local_symbols: Option<PathBuf>,
) -> Arc<[SourceConfig]> {
    let mut dsym_sources = vec![];
    if let Mode::Online {
        ref org,
        ref project,
        ref base_url,
        ref auth_token,
        ..
    } = mode
    {
        let project_source = SourceConfig::Sentry(Arc::new(SentrySourceConfig {
            id: SourceId::new("sentry:project"),
            token: auth_token.clone(),
            url: base_url
                .join(&format!("projects/{org}/{project}/files/dsyms/"))
                .unwrap(),
        }));

        dsym_sources.push(project_source);
    }

    dsym_sources.extend(symbolicator_config.sources.iter().cloned());
    if let Some(path) = local_symbols {
        let local_source = FilesystemSourceConfig {
            id: SourceId::new("local:cli"),
            path,
            files: CommonSourceConfig {
                filters: Default::default(),
                layout: DirectoryLayout {
                    ty: DirectoryLayoutType::Unified,
                    casing: Default::default(),
                },
                is_public: false,
            },
        };
        dsym_sources.push(SourceConfig::Filesystem(local_source.into()));
    }
    Arc::from(dsym_sources.into_boxed_slice())
}

#[derive(Debug)]
enum Payload {
    Event(event::Event),
    Minidump(TempPath),
}

impl Payload {
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

    async fn get_remote(client: &reqwest::Client, key: EventKey<'_>) -> Result<Self> {
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

    pub async fn get_attached_minidump(
        client: &reqwest::Client,
        key: EventKey<'_>,
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
            .map(|attachment| &attachment.id)
        else {
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

    pub async fn download_event(client: &reqwest::Client, key: EventKey<'_>) -> Result<Event> {
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
    use symbolicator_js::interface::{
        JsFrame, JsFrameData, JsModule, JsStacktrace, SymbolicateJsStacktraces,
    };
    use symbolicator_native::interface::{
        AddrMode, CompleteObjectInfo, FrameTrust, RawFrame, RawStacktrace, Signal,
        StacktraceOrigin, SymbolicateStacktraces,
    };
    use symbolicator_service::types::{Platform, RawObjectInfo, Scope, ScrapingConfig};
    use symbolicator_service::utils::hex::HexValue;
    use symbolicator_sources::{SentrySourceConfig, SourceConfig};

    pub fn create_js_symbolication_request(
        scope: Scope,
        source: Arc<SentrySourceConfig>,
        event: Event,
        scraping_enabled: bool,
    ) -> anyhow::Result<SymbolicateJsStacktraces> {
        let Event {
            platform,
            debug_meta,
            exception,
            threads,
            release,
            dist,
            ..
        } = event;

        let mut stacktraces = vec![];
        if let Some(mut excs) = exception.map(|excs| excs.values) {
            stacktraces.extend(
                excs.iter_mut()
                    .filter_map(|exc| exc.raw_stacktrace.take().or_else(|| exc.stacktrace.take())),
            );
        }
        if let Some(mut threads) = threads.map(|threads| threads.values) {
            stacktraces.extend(threads.iter_mut().filter_map(|thread| {
                thread
                    .raw_stacktrace
                    .take()
                    .or_else(|| thread.stacktrace.take())
            }));
        }

        let stacktraces: Vec<_> = stacktraces
            .into_iter()
            .map(JsStacktrace::from)
            .filter(|stacktrace| !stacktrace.frames.is_empty())
            .collect();

        let modules: Vec<_> = debug_meta
            .images
            .into_iter()
            .filter_map(|module| match module {
                Module::Sourcemap(m) => Some(m),
                _ => None,
            })
            .collect();

        if stacktraces.is_empty() {
            bail!("Event has no usable frames");
        };

        Ok(SymbolicateJsStacktraces {
            platform: Some(platform),
            scope,
            source,
            release,
            dist,
            scraping: ScrapingConfig {
                enabled: scraping_enabled,
                ..Default::default()
            },
            apply_source_context: true,

            stacktraces,
            modules,
        })
    }

    pub fn create_native_symbolication_request(
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
            stacktraces.extend(
                excs.iter_mut()
                    .filter_map(|exc| exc.raw_stacktrace.take().or_else(|| exc.stacktrace.take())),
            );
        }
        if let Some(mut threads) = threads.map(|threads| threads.values) {
            stacktraces.extend(threads.iter_mut().filter_map(|thread| {
                thread
                    .raw_stacktrace
                    .take()
                    .or_else(|| thread.stacktrace.take())
            }));
        }

        let stacktraces: Vec<_> = stacktraces
            .into_iter()
            .map(RawStacktrace::from)
            .filter(|stacktrace| !stacktrace.frames.is_empty())
            .collect();

        let modules: Vec<_> = debug_meta
            .images
            .into_iter()
            .filter_map(|module| match module {
                Module::Object(m) => Some(CompleteObjectInfo::from(m)),
                _ => None,
            })
            .collect();

        if modules.is_empty() {
            bail!("Event has no debug images");
        };

        if stacktraces.is_empty() {
            bail!("Event has no usable frames");
        };

        Ok(SymbolicateStacktraces {
            platform: Some(event.platform),
            scope,
            signal,
            sources,
            origin: StacktraceOrigin::Symbolicate,
            stacktraces,
            modules,
            apply_source_context: true,
            scraping: Default::default(),
        })
    }

    #[derive(Debug, Deserialize)]
    pub struct Event {
        #[serde(default)]
        pub platform: Platform,
        #[serde(default)]
        debug_meta: DebugMeta,
        exception: Option<Exceptions>,
        threads: Option<Threads>,
        signal: Option<Signal>,
        release: Option<String>,
        dist: Option<String>,
    }

    #[derive(Debug, Deserialize, Default)]
    struct DebugMeta {
        images: Vec<Module>,
    }

    #[derive(Debug, Clone, Deserialize)]
    #[serde(untagged)]
    pub enum Module {
        Object(RawObjectInfo),
        Sourcemap(JsModule),
    }

    #[derive(Debug, Deserialize)]
    struct Exceptions {
        values: Vec<Exception>,
    }

    #[derive(Debug, Deserialize)]
    struct Exception {
        raw_stacktrace: Option<Stacktrace>,
        stacktrace: Option<Stacktrace>,
    }

    #[derive(Debug, Deserialize)]
    struct Threads {
        values: Vec<Thread>,
    }

    #[derive(Debug, Deserialize)]
    struct Thread {
        raw_stacktrace: Option<Stacktrace>,
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

    impl From<Stacktrace> for JsStacktrace {
        fn from(stacktrace: Stacktrace) -> Self {
            let frames = stacktrace
                .frames
                .into_iter()
                .filter_map(to_js_frame)
                .rev()
                .collect();

            Self { frames }
        }
    }

    #[derive(Debug, Deserialize)]
    struct Frame {
        platform: Option<Platform>,
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

        colno: Option<u32>,

        #[serde(default)]
        pre_context: Vec<String>,

        context_line: Option<String>,

        #[serde(default)]
        post_context: Vec<String>,

        module: Option<String>,

        source_link: Option<String>,

        in_app: Option<bool>,

        #[serde(default)]
        trust: FrameTrust,

        #[serde(default)]
        data: JsFrameData,
    }

    fn to_raw_frame(value: Frame) -> Option<RawFrame> {
        Some(RawFrame {
            platform: value.platform,
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

    fn to_js_frame(value: Frame) -> Option<JsFrame> {
        Some(JsFrame {
            platform: value.platform,
            function: value.function,
            filename: value.filename,
            module: value.module,
            abs_path: value.abs_path?,
            lineno: value.lineno?,
            colno: value.colno,
            pre_context: value.pre_context,
            context_line: value.context_line,
            post_context: value.post_context,
            token_name: None,
            in_app: value.in_app,
            data: value.data,
        })
    }
}
