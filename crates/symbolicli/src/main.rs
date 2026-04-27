#![recursion_limit = "256"]
use std::fs::File;
use std::io::{Read, Seek};
use std::path::Path;
use std::sync::Arc;
use std::{fmt, io};

use event::{create_js_symbolication_request, create_native_symbolication_request};
use output::{print_compact, print_pretty};
use remote::EventKey;

use settings::{Mode, SymbolsPath};
use symbolicator_js::SourceMapService;
use symbolicator_native::SymbolicationActor;
use symbolicator_native::interface::{AttachmentFile, ProcessMinidump};
use symbolicator_service::config::Config;
use symbolicator_service::services::SharedServices;
use symbolicator_service::types::Scope;
use symbolicator_sources::{
    CommonSourceConfig, DirectoryLayout, FilesystemSourceConfig, SentrySourceConfig, SentryToken,
    SourceConfig, SourceId,
};

use anyhow::{Context, Result};
use reqwest::header;
use tracing_subscriber::filter;
use tracing_subscriber::prelude::*;

use crate::output::CompletedResponse;

mod event;
mod js_local_source;
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

    // We depend on `rustls` with both the `aws-lc-rs` and
    // `ring` features enabled. This means that `rustls` can't automatically
    // decide which provider to use and we have to initialize it manually.
    if rustls::crypto::ring::default_provider()
        .install_default()
        .is_err()
    {
        anyhow::bail!("Failed to initialize crypto provider");
    }

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
                anyhow::bail!(
                    "Event not found in local file system and `symbolicli` is in offline mode. Stopping."
                );
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
                token: SentryToken(auth_token.clone()),
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
        Payload::Minidump(minidump_file) => {
            let dsym_sources = prepare_dsym_sources(mode, &symbolicator_config, symbols);
            tracing::info!("symbolicating minidump");
            let res = native
                .process_minidump(ProcessMinidump {
                    platform: None,
                    scope,
                    minidump_file: AttachmentFile::Local(minidump_file),
                    sources: dsym_sources,
                    scraping: Default::default(),
                    rewrite_first_module: Default::default(),
                })
                .await?;
            CompletedResponse::NativeSymbolication(res)
        }
        Payload::AppleCrashReport(file) => {
            let dsym_sources = prepare_dsym_sources(mode, &symbolicator_config, symbols);
            tracing::info!("symbolicating apple crash report");
            let res = native
                .process_apple_crash_report(
                    None,
                    scope,
                    AttachmentFile::Local(file),
                    dsym_sources,
                    Default::default(),
                )
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
    local_symbols: Option<SymbolsPath>,
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
            token: SentryToken(auth_token.clone()),
            url: base_url
                .join(&format!("projects/{org}/{project}/files/dsyms/"))
                .unwrap(),
        }));

        dsym_sources.push(project_source);
    }

    dsym_sources.extend(symbolicator_config.sources.iter().cloned());
    if let Some(symbols_path) = local_symbols {
        let local_source = FilesystemSourceConfig {
            id: SourceId::new("local:cli"),
            path: symbols_path.path,
            files: CommonSourceConfig {
                filters: Default::default(),
                layout: DirectoryLayout {
                    ty: symbols_path.layout_type,
                    casing: Default::default(),
                },
                is_public: false,
                has_index: false,
            },
        };
        dsym_sources.push(SourceConfig::Filesystem(local_source.into()));
    }
    Arc::from(dsym_sources.into_boxed_slice())
}

#[derive(Debug)]
enum Payload {
    Event(event::Event),
    Minidump(File),
    AppleCrashReport(File),
}

impl Payload {
    fn parse<P: AsRef<Path> + fmt::Debug>(path: &P) -> Result<Option<Self>> {
        match File::open(path) {
            Ok(file) => Self::parse_file(file).map(Option::Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context(format!("Could not open event file at {path:?}")),
        }
    }

    fn parse_file(mut file: File) -> Result<Self> {
        let mut magic = [0; 4];
        file.read_exact(&mut magic)?;
        file.rewind()?;

        if &magic == b"MDMP" || &magic == b"PMDM" {
            Ok(Payload::Minidump(file))
        } else if &magic == b"Inci" || &magic == b"----" {
            Ok(Payload::AppleCrashReport(file))
        } else {
            let event = serde_json::from_reader(file).context("failed to parse event json file")?;
            Ok(Payload::Event(event))
        }
    }

    async fn get_remote(client: &reqwest::Client, key: EventKey<'_>) -> Result<Self> {
        tracing::info!("trying to resolve event remotely");
        let event = remote::download_event(client, key).await?;
        tracing::info!("event json file downloaded");

        match remote::get_attached_crash(client, key).await? {
            Some(url) => {
                tracing::info!("crash attachment found");
                let file = remote::download_crash_file(client, url).await?;
                tracing::info!(path = ?file, "crash file downloaded");

                Payload::parse_file(file)
            }

            None => {
                tracing::info!("no crash attachment found");
                Ok(Self::Event(event))
            }
        }
    }
}

mod remote {
    use std::io::Write;
    use std::{fs::File, io::Seek};

    use anyhow::{Context, Result, bail};
    use reqwest::{StatusCode, Url};
    use serde::Deserialize;

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

    pub async fn get_attached_crash(
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
            .find(|attachment| {
                attachment.r#type == "event.minidump"
                    || attachment.r#type == "event.applecrashreport"
            })
            .map(|attachment| &attachment.id)
        else {
            return Ok(None);
        };

        let mut download_url = attachments_url.join(&format!("{minidump_id}/")).unwrap();
        download_url.query_pairs_mut().append_pair("download", "1");

        Ok(Some(download_url))
    }

    pub async fn download_crash_file(client: &reqwest::Client, download_url: Url) -> Result<File> {
        tracing::info!(url = %download_url, "downloading crash file");

        let response = client
            .get(download_url)
            .send()
            .await
            .context("Failed to send request")?;

        let crash_file = if response.status().is_success() {
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

        let mut temp_file = tempfile::tempfile().unwrap();
        temp_file
            .write_all(&crash_file)
            .context("Failed to write crash file to disk")?;

        temp_file.rewind()?;

        Ok(temp_file)
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
