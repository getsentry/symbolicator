use serde::Serialize;
use symbolicator_native::interface::CompletedSymbolicationResponse;

use crate::service::{CompletedResponse, SymbolicationResponse};

pub mod get_metadata;

// Errors common to most invocations.
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommonError {
    UnexpectedResponse,
    RequestTimeout,
    SymbolicationFailed,
    InternalError,
}

// The error that a tool invocation can return--either a CommonError which most
// symbolication requests can encounter, or a specific type parameterized by
// the tool call.
#[derive(Serialize)]
#[serde(untagged)]
pub enum ToolError<E> {
    Common(CommonError),
    Tool(E),
}

impl<E> From<CommonError> for ToolError<E> {
    fn from(e: CommonError) -> Self {
        ToolError::Common(e)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InvokeResponse<R, E> {
    Success(R),
    Error(ToolError<E>),
}

impl<R, E> From<Result<R, ToolError<E>>> for InvokeResponse<R, E> {
    fn from(r: Result<R, ToolError<E>>) -> Self {
        match r {
            Ok(v) => InvokeResponse::Success(v),
            Err(e) => InvokeResponse::Error(e),
        }
    }
}

// Maps common errors to CommonError, otherwise returning a CompletedResponse.
pub fn completed(resp: SymbolicationResponse) -> Result<CompletedResponse, CommonError> {
    match resp {
        SymbolicationResponse::Completed(c) => Ok(*c),
        SymbolicationResponse::Pending { .. } | SymbolicationResponse::Timeout => {
            Err(CommonError::RequestTimeout)
        }
        SymbolicationResponse::Failed { .. } => Err(CommonError::SymbolicationFailed),
        SymbolicationResponse::InternalError => Err(CommonError::InternalError),
    }
}

// For the given SymbolicationResponse, returns either the contained Native response,
// or returns an UnexpectedResponse.
pub fn select_native(
    resp: SymbolicationResponse,
) -> Result<CompletedSymbolicationResponse, CommonError> {
    match completed(resp)? {
        CompletedResponse::Native(n) => Ok(n),
        _ => Err(CommonError::UnexpectedResponse),
    }
}
