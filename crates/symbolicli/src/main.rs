use std::{path::PathBuf, sync::Arc};

use symbolicator_service::config::Config;
use symbolicator_service::types::Scope;
use symbolicator_sources::{SentrySourceConfig, SourceConfig, SourceId};

use anyhow::{bail, Context};
use clap::Parser;
use reqwest::{header, Url};
use serde::Deserialize;

#[tokio::main]
#[allow(unreachable_code)]
#[allow(unused)]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let args = Cli::parse();

    let Some(auth_token) = args.auth_token.or_else(|| std::env::var("SENTRY_AUTH_TOKEN").ok()) else {
        bail!("No auth token provided. Pass it either via the `--auth-token` option or via the `SENTRY_AUTH_TOKEN` environment variable.");
    };

    let sentry_url = Url::parse(&args.sentry_url).context("Invalid sentry URL")?;
    let base_url = if sentry_url.as_str().ends_with('/') {
        sentry_url.join("api/0/").unwrap()
    } else {
        sentry_url.join("/api/0/").unwrap()
    };

    let config = Config::get(args.config.as_deref())?;

    let runtime = tokio::runtime::Handle::current();
    let (symbolication, _objects) =
        symbolicator_service::services::create_service(&config, runtime)
            .await
            .context("failed to start symbolication service")?;

    let org = args.org;
    let project = args.project;
    let event_id = args.event;

    let mut headers = header::HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        header::HeaderValue::from_str(&format!("Bearer {auth_token}")).unwrap(),
    );

    let client = reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap();

    let uploaded_difs = SourceConfig::Sentry(Arc::new(SentrySourceConfig {
        id: SourceId::new("sentry:project"),
        token: auth_token,
        url: base_url
            .join(&format!("projects/{org}/{project}/files/dsyms/"))
            .unwrap(),
    }));

    let res = match minidump::get_attached_minidump(&client, &base_url, &org, &project, &event_id)
        .await?
    {
        Some(minidump_url) => {
            tracing::info!("minidump attachment found");
            let minidump_path = minidump::download_minidump(&client, minidump_url).await?;
            tracing::info!(path = ?minidump_path, "minidump file downloaded");

            let mut sources = vec![uploaded_difs.clone()];
            sources.extend(config.sources.iter().cloned());
            let sources = Arc::from(sources.into_boxed_slice());

            let scope = Scope::Scoped(project.clone());

            symbolication
                .process_minidump(scope, minidump_path, sources)
                .await?
        }
        None => {
            let event = event::get_event(&client, &base_url, &org, &project, &event_id).await?;

            let symbolication_request =
                event::create_symbolication_request(&project, uploaded_difs, event)
                    .context("Event cannot be symbolicated")?;

            dbg!(&symbolication_request.sources);

            symbolication.symbolicate(symbolication_request).await?
        }
    };

    println!("{}", serde_json::to_string(&res).unwrap());

    Ok(())
}

/// A utility that provides local symbolication of Sentry events.
///
/// Currently, only events with an attached minidump are supported.
///
/// A valid auth token needs to be provided via the `--auth-token` option
/// or the `SENTRY_AUTH_TOKEN` environment variable. The option takes precedence.
///
/// The symbolication result will be returned as JSON.
#[derive(Parser, Debug)]
#[command(author, version, about, long_about)]
struct Cli {
    /// The ID of the event to symbolicate.
    event: String,

    /// The organization slug.
    #[arg(long, short)]
    org: String,

    /// The project slug.
    #[arg(long, short)]
    project: String,

    /// A symbolicator configuration file.
    ///
    /// Use this to configure caches and additional DIF sources.
    #[arg(long, short)]
    config: Option<PathBuf>,

    /// The URL of the sentry instance to connect to.
    #[arg(long, default_value_t = String::from("https://sentry.io/"))]
    sentry_url: String,

    /// The Sentry auth token to use to access the event and DIFs.
    ///
    /// This can alternatively be passed via the `SENTRY_AUTH_TOKEN` environment variable.
    #[arg(long = "auth-token")]
    auth_token: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct Attachment {
    r#type: String,
    id: String,
}

mod minidump {
    use std::io::Write;

    use anyhow::{bail, Context};
    use reqwest::Url;
    use tempfile::{NamedTempFile, TempPath};

    use crate::Attachment;

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
}

mod event {
    use std::sync::Arc;

    use symbolic::common::Language;
    use symbolicator_service::{
        services::symbolication::StacktraceOrigin,
        types::{
            CompleteObjectInfo, FrameTrust, RawFrame, RawObjectInfo, RawStacktrace, Scope, Signal,
        },
        utils::{addr::AddrMode, hex::HexValue},
    };

    use anyhow::{bail, Context};
    use reqwest::Url;
    use serde::Deserialize;
    use symbolicator_service::services::symbolication::SymbolicateStacktraces;
    use symbolicator_sources::SourceConfig;

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
        project: &str,
        project_source: SourceConfig,
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
            scope: Scope::Scoped(project.to_string()),
            signal,
            sources: Arc::new([project_source]),
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
}
