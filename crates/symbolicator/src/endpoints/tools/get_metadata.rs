use std::sync::Arc;

use serde::{Deserialize, Serialize};
use symbolicator_native::interface::{AttachmentFile, ProcessMinidump, SystemInfo};
use symbolicator_service::{types::Platform, utils::sentry::ConfigureScope};
use symbolicator_sources::SourceConfig;

use crate::{
    endpoints::{
        ResponseError,
        symbolicate_any::SymbolicationRequest,
        tools::{InvokeResponse, ToolError, select_native},
    },
    service::{RequestOptions, RequestQueryParams, RequestService, SymbolicationResponse},
};

const INVOCATION_TIMEOUT_SECONDS: u64 = 100;

/// The payload for a GetMetadata request.
#[derive(Deserialize)]
pub struct Request {
    pub minidump_or_report: SymbolicationRequest,

    pub storage_id: String,

    #[serde(default)]
    pub platform: Option<Platform>,

    #[serde(default)]
    pub sources: Arc<[SourceConfig]>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Error {
    NoMetadata,
}

// Given the SymbolicationResponse, either return the contained SystemInfo, or a ToolError
// indicating the error we encountered.
fn to_system_info(resp: SymbolicationResponse) -> Result<SystemInfo, ToolError<Error>> {
    select_native(resp)?
        .system_info
        .ok_or(ToolError::Tool(Error::NoMetadata))
}

/// A tool to retrieve metadata about a minidump or Apple crash report.
pub async fn invoke(
    service: RequestService,
    params: RequestQueryParams,
    platform: Option<Platform>,
    req: SymbolicationRequest,
    sources: Arc<[SourceConfig]>,
    storage_id: String,
) -> Result<InvokeResponse<SystemInfo, Error>, ResponseError> {
    params.configure_scope();

    let request_id = match req {
        SymbolicationRequest::Minidump {
            storage_url,
            storage_token,
            rewrite_first_module,
        } => {
            let pm = ProcessMinidump {
                platform,
                scope: params.scope,
                minidump_file: AttachmentFile::Remote {
                    storage_url,
                    storage_token,
                    storage_id: Some(storage_id),
                },
                sources,
                rewrite_first_module,
                extract_variables: false,
                apply_source_server_info: false,
                cache_minidump: true,
            };
            service.process_minidump(pm, RequestOptions::default())?
        }
        SymbolicationRequest::AppleCrashreport {
            storage_url,
            storage_token,
        } => service.process_apple_crash_report(
            platform,
            params.scope,
            AttachmentFile::Remote {
                storage_url,
                storage_token,
                storage_id: Some(storage_id),
            },
            sources,
            RequestOptions::default(),
            true,
        )?,
    };

    let result = match service
        .get_response(request_id, Some(INVOCATION_TIMEOUT_SECONDS))
        .await
    {
        Some(response) => Ok::<SymbolicationResponse, ResponseError>(response),
        None => Err("symbolication request did not start".into()),
    }?;

    let e = to_system_info(result);
    Ok(e.into())
}
