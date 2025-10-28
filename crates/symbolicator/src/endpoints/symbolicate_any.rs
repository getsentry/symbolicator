use std::sync::Arc;

use axum::extract;
use axum::response::Json;
use serde::Deserialize;

use symbolicator_native::interface::{AttachmentFile, ProcessMinidump, RewriteRules};
use symbolicator_service::types::Platform;
use symbolicator_sources::SourceConfig;

use crate::service::{
    RequestOptions, RequestQueryParams, RequestService, ScrapingConfig, SymbolicationResponse,
};
use crate::utils::sentry::ConfigureScope;

use super::ResponseError;

/// JSON body of the symbolication request.
///
/// This includes a bunch of common request options across the various symbolication requests.
#[derive(Deserialize)]
pub struct SymbolicateAnyRequestBody {
    pub platform: Option<Platform>,
    #[serde(default)]
    pub options: RequestOptions,
    #[serde(default)]
    pub scraping: ScrapingConfig,
    #[serde(default)]
    pub sources: Arc<[SourceConfig]>,

    pub symbolicate: SymbolicationRequest,
}

/// The actual symbolication request.
///
/// This currently includes minidump and apple-crashreport based on stored attachments.
/// In the future, we could move all of native, js and jvm symbolication to this enum.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SymbolicationRequest {
    Minidump {
        storage_url: String,
        rewrite_first_module: RewriteRules,
    },
    AppleCrashreport {
        storage_url: String,
    },
    // TODO: it should be possible to also support native, js and jvm requests here as well
}

pub async fn symbolicate_any(
    extract::State(service): extract::State<RequestService>,
    extract::Query(params): extract::Query<RequestQueryParams>,
    extract::Json(body): extract::Json<SymbolicateAnyRequestBody>,
) -> Result<Json<SymbolicationResponse>, ResponseError> {
    sentry::start_session();

    params.configure_scope();

    let request_id = match body.symbolicate {
        SymbolicationRequest::Minidump {
            storage_url,
            rewrite_first_module,
        } => service.process_minidump(
            ProcessMinidump {
                platform: body.platform,
                scope: params.scope,
                minidump_file: AttachmentFile::Remote(storage_url),
                sources: body.sources,
                scraping: body.scraping,
                rewrite_first_module,
            },
            body.options,
        )?,
        SymbolicationRequest::AppleCrashreport { storage_url } => service
            .process_apple_crash_report(
                body.platform,
                params.scope,
                AttachmentFile::Remote(storage_url),
                body.sources,
                body.scraping,
                body.options,
            )?,
    };

    match service.get_response(request_id, params.timeout).await {
        Some(response) => Ok(Json(response)),
        None => Err("symbolication request did not start".into()),
    }
}

#[cfg(test)]
mod tests {
    use reqwest::{Client, StatusCode};

    use crate::test;

    #[tokio::test]
    async fn test_body_limit() {
        test::setup();

        let server = test::server_with_default_service();

        let mut buf = vec![b'.'; 4 * 1024 * 1024];
        buf[0] = b'"';
        *buf.last_mut().unwrap() = b'"';

        let response = Client::new()
            .post(server.url("/symbolicate-any"))
            .header("Content-Type", "application/json")
            .body(buf)
            .send()
            .await
            .unwrap();

        // the JSON does not fit our schema :-)
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let mut buf = vec![b'.'; 8 * 1024 * 1024];
        buf[0] = b'"';
        *buf.last_mut().unwrap() = b'"';

        let response = Client::new()
            .post(server.url("/symbolicate-any"))
            .header("Content-Type", "application/json")
            .body(buf)
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }
}
