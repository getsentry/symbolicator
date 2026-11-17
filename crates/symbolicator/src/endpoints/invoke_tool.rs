use axum::{
    Json, extract,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use std::sync::Arc;
use symbolicator_native::interface::{AttachmentFile, ProcessMinidump, RewriteRules, SystemInfo};
use symbolicator_service::{types::Platform, utils::sentry::ConfigureScope};
use symbolicator_sources::{AttachmentRemoteFile, RemoteFile, SentryToken, SourceConfig};

use crate::{
    endpoints::{ResponseError, symbolicate_any::SymbolicationRequest},
    service::{RequestOptions, RequestQueryParams, RequestService, SymbolicationResponse},
};

/// An enum of all possible tools invocations for Symbolicator.
#[derive(Deserialize)]
pub enum ToolRequestKind {
    GetMetadata {
        minidump: SymbolicationRequest,

        #[serde(default)]
        platform: Option<Platform>,

        #[serde(default)]
        sources: Arc<[SourceConfig]>,
    },
}

/// A wrapper for Symbolicator tool invocations, containing common data.
#[derive(Deserialize)]
pub struct ToolRequest {
    request_kind: ToolRequestKind,
}

/// The entrypopint for all tool invocations.
pub async fn invoke_tool(
    extract::State(service): extract::State<RequestService>,
    extract::Query(params): extract::Query<RequestQueryParams>,
    extract::Json(body): extract::Json<ToolRequest>,
) -> Result<Response, ResponseError> {
    match body.request_kind {
        ToolRequestKind::GetMetadata {
            minidump,
            sources,
            platform,
        } => get_metadata(service, platform, minidump, params, sources)
            .await
            .map(IntoResponse::into_response),
    }
}

/// A tool to retrieve metadata about a minidump.
async fn get_metadata(
    service: RequestService,
    platform: Option<Platform>,
    req: SymbolicationRequest,
    params: RequestQueryParams,
    sources: Arc<[SourceConfig]>,
) -> Result<Json<SystemInfo>, ResponseError> {
    params.configure_scope();
    let (storage_url, storage_token, rewrite_first_module) = match req {
        SymbolicationRequest::Minidump {
            storage_url,
            storage_token,
            rewrite_first_module,
        } => (storage_url, storage_token, rewrite_first_module),
        SymbolicationRequest::AppleCrashreport {
            storage_url,
            storage_token,
        } => (storage_url, storage_token, RewriteRules::default()),
    };

    // Make sure we download _and_ cache the attachment, as a download can happen more than once with
    // tool calls.
    let byteview = match service
        .download_attachment(
            &params.scope,
            RemoteFile::Attachment(AttachmentRemoteFile {
                url: storage_url,
                token: storage_token.map(SentryToken),
            }),
        )
        .await
    {
        Ok(bv) => bv,
        Err(e) => return Err(anyhow::anyhow!("Could not download attachment: {e}").into()),
    };

    let pm = ProcessMinidump {
        platform,
        scope: params.scope,
        minidump_file: AttachmentFile::Mapped(byteview),
        sources,
        rewrite_first_module,
        extract_variables: false,
        apply_source_server_info: false,
    };
    let request_id = service.process_minidump(pm, RequestOptions::default())?;

    let result = match service.get_response(request_id, None).await {
        Some(response) => Ok::<SymbolicationResponse, ResponseError>(response),
        None => Err("symbolication request did not start".into()),
    }?;

    match result {
        SymbolicationResponse::Completed(completed_response) => match *completed_response {
            crate::service::CompletedResponse::Native(completed_symbolication_response) => {
                completed_symbolication_response
                    .system_info
                    .map(Json)
                    .ok_or("No metadata found".into())
            }
            _ => Err("unexpected symbolication response".into()),
        },
        SymbolicationResponse::Pending {
            request_id: _,
            retry_after: _,
        } => Err("Symbolication request took too long".into()),
        SymbolicationResponse::Failed { message } => {
            Err(anyhow::anyhow!("Symbolication request failed: {message}").into())
        }
        SymbolicationResponse::Timeout => Err("Symbolication timeout".into()),
        SymbolicationResponse::InternalError => Err("Symbolication internal error".into()),
    }
}
