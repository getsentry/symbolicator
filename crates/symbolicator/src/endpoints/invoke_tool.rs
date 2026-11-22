use axum::{
    Json, extract,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use std::sync::Arc;
use symbolicator_native::interface::{AttachmentFile, ProcessMinidump, SystemInfo};
use symbolicator_service::types::ScrapingConfig;
use symbolicator_sources::SourceConfig;
use tracing::info;

use crate::{
    endpoints::{ResponseError, symbolicate_any::SymbolicationRequest},
    service::{RequestOptions, RequestQueryParams, RequestService, SymbolicationResponse},
};

#[derive(Deserialize)]
pub enum ToolCall {
    GetMetadata,
}

#[derive(Deserialize)]
pub struct ToolRequest {
    minidump: SymbolicationRequest,

    #[serde(default)]
    sources: Arc<[SourceConfig]>,

    args: ToolCall,
}

pub async fn invoke_tool(
    extract::State(service): extract::State<RequestService>,
    extract::Query(params): extract::Query<RequestQueryParams>,
    extract::Json(body): extract::Json<ToolRequest>,
) -> Result<Response, ResponseError> {
    info!("invoke_tool");
    match body.args {
        ToolCall::GetMetadata => get_metadata(service, body.minidump, body.sources)
            .await
            .map(IntoResponse::into_response),
    }
}

async fn get_metadata(
    service: RequestService,
    req: SymbolicationRequest,
    sources: Arc<[SourceConfig]>,
) -> Result<Json<SystemInfo>, ResponseError> {
    let (storage_url, storage_token, rewrite_first_module) = match req {
        SymbolicationRequest::Minidump {
            storage_url,
            storage_token,
            rewrite_first_module,
        } => (storage_url, storage_token, rewrite_first_module),
        SymbolicationRequest::AppleCrashreport {
            storage_url,
            storage_token,
        } => todo!(),
    };
    let pm = ProcessMinidump {
        platform: None, //?
        scope: symbolicator_service::types::Scope::Global,
        minidump_file: AttachmentFile::Remote {
            storage_url,
            storage_token,
        },
        sources: sources,
        scraping: ScrapingConfig::default(),
        rewrite_first_module,
        extract_variables: false,
    };
    let request_id = service
        .process_minidump(pm, RequestOptions::default())
        .unwrap();

    let r: Result<SymbolicationResponse, ResponseError> =
        match service.get_response(request_id, None).await {
            Some(response) => Ok(response),
            None => Err("symbolication request did not start".into()),
        };

    let r2 = r.unwrap();

    match r2 {
        SymbolicationResponse::Completed(completed_response) => match *completed_response {
            crate::service::CompletedResponse::Native(completed_symbolication_response) => {
                completed_symbolication_response
                    .system_info
                    .map(|si| Json(si))
                    .ok_or(anyhow::anyhow!("no system info").into())
            }
            crate::service::CompletedResponse::Js(completed_js_symbolication_response) => todo!(),
            crate::service::CompletedResponse::Jvm(completed_jvm_symbolication_response) => todo!(),
        },
        SymbolicationResponse::Pending {
            request_id,
            retry_after,
        } => todo!(),
        SymbolicationResponse::Failed { message } => todo!(),
        SymbolicationResponse::Timeout => todo!(),
        SymbolicationResponse::InternalError => todo!(),
    }
}
