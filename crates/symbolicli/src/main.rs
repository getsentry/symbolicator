use std::io::Write;
use std::{path::PathBuf, sync::Arc};

use symbolicator_service::config::Config;
use symbolicator_service::types::Scope;
use symbolicator_sources::{SentrySourceConfig, SourceConfig, SourceId};

use anyhow::{bail, Context};
use clap::Parser;
use reqwest::{header, Url};
use serde::Deserialize;
use tempfile::{NamedTempFile, TempPath};

#[tokio::main]
#[allow(unreachable_code)]
#[allow(unused)]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let args = Cli::parse();

    let auth_token = std::env::var("SENTRY_AUTH_TOKEN").context("No auth token provided")?;

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

    let minidump_path = download_minidump(&client, &base_url, &org, &project, &event_id).await?;

    tracing::info!(path = ?minidump_path, "minidump file downloaded");

    let uploaded_difs = SourceConfig::Sentry(Arc::new(SentrySourceConfig {
        id: SourceId::new("sentry:project"),
        token: auth_token,
        url: base_url
            .join(&format!("projects/{org}/{project}/files/dsyms/"))
            .unwrap(),
    }));

    let mut sources = vec![uploaded_difs];
    sources.extend(config.sources.iter().cloned());
    let sources = Arc::from(sources.into_boxed_slice());

    let scope = Scope::Scoped(project.clone());

    let _res = symbolication
        .process_minidump(scope, minidump_path, sources)
        .await?;

    Ok(())
}

async fn download_minidump(
    client: &reqwest::Client,
    base_url: &Url,
    org: &str,
    project: &str,
    event_id: &str,
) -> anyhow::Result<TempPath> {
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

    let Some(minidump) = attachments
        .iter()
        .find(|attachment| attachment.r#type == "event.minidump") else {
        bail!("Event has no minidump attached");
    };

    let minidump_id = &minidump.id;

    let mut download_url = attachments_url.join(&format!("{minidump_id}/")).unwrap();
    download_url.query_pairs_mut().append_pair("download", "1");

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

/// A utility that provides local symbolication of Sentry events.
///
/// Currently, only events with an attached minidump are supported.
///
/// A valid auth token needs to be provided via the `SENTRY_AUTH_TOKEN` environment variable.
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
}

#[derive(Debug, Clone, Deserialize)]
struct Attachment {
    r#type: String,
    id: String,
}
