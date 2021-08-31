use axum::extract::multipart::MultipartError;
use axum::http::{Error as HttpError, Response, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::services::symbolication::MaxRequestsError;

#[derive(Debug)]
pub struct ResponseError {
    status: StatusCode,
    err: anyhow::Error,
}

impl From<MultipartError> for ResponseError {
    fn from(err: MultipartError) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            err: err.into(),
        }
    }
}

impl From<serde_json::Error> for ResponseError {
    fn from(err: serde_json::Error) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            err: err.into(),
        }
    }
}

impl From<MaxRequestsError> for ResponseError {
    fn from(_: MaxRequestsError) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            err: anyhow::anyhow!("maximum number of concurrent requests reached"),
        }
    }
}

impl From<&'static str> for ResponseError {
    fn from(msg: &'static str) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            err: anyhow::anyhow!(msg),
        }
    }
}

impl From<(StatusCode, &'static str)> for ResponseError {
    fn from((code, msg): (StatusCode, &'static str)) -> Self {
        Self {
            status: code,
            err: anyhow::anyhow!(msg),
        }
    }
}

impl From<anyhow::Error> for ResponseError {
    fn from(err: anyhow::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            err,
        }
    }
}

impl From<std::io::Error> for ResponseError {
    fn from(err: std::io::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            err: err.into(),
        }
    }
}

impl From<HttpError> for ResponseError {
    fn from(err: HttpError) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            err: err.into(),
        }
    }
}

impl IntoResponse for ResponseError {
    type Body = <Json<ApiErrorResponse> as IntoResponse>::Body;
    type BodyError = <Self::Body as axum::body::HttpBody>::Error;

    fn into_response(self) -> Response<Self::Body> {
        let mut response = Json(ApiErrorResponse::from(self.err)).into_response();
        *response.status_mut() = self.status;
        response
    }
}

/// An error response from an api.
#[derive(Serialize, Deserialize, Default, Debug)]
pub struct ApiErrorResponse {
    detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    causes: Option<Vec<String>>,
}

impl From<anyhow::Error> for ApiErrorResponse {
    fn from(err: anyhow::Error) -> Self {
        let mut chain = err.chain().map(|err| err.to_string());
        let detail = chain.next();
        let causes: Vec<_> = chain.collect();
        let causes = if causes.is_empty() {
            None
        } else {
            Some(causes)
        };

        ApiErrorResponse { detail, causes }
    }
}
