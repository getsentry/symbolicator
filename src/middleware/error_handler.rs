// TODO(ja): Implement this
// /// Registers the default error handlers.
// pub struct ErrorHandlers;

// use serde::{Deserialize, Serialize};

// /// An error response from an api.
// #[derive(Serialize, Deserialize, Default, Debug)]
// pub struct ApiErrorResponse {
//     detail: Option<String>,
//     #[serde(skip_serializing_if = "Option::is_none")]
//     causes: Option<Vec<String>>,
// }

// impl ApiErrorResponse {
//     /// Creates an error response with a detail message
//     pub fn with_detail<S: AsRef<str>>(s: S) -> ApiErrorResponse {
//         ApiErrorResponse {
//             detail: Some(s.as_ref().to_string()),
//             causes: None,
//         }
//     }

//     /// Creates an error response from a fail.
//     pub fn from_fail(fail: &dyn Fail) -> ApiErrorResponse {
//         let mut messages = vec![];

//         for cause in Fail::iter_chain(fail) {
//             let msg = cause.to_string();
//             if !messages.contains(&msg) {
//                 messages.push(msg);
//             }
//         }

//         ApiErrorResponse {
//             detail: Some(messages.remove(0)),
//             causes: if messages.is_empty() {
//                 None
//             } else {
//                 Some(messages)
//             },
//         }
//     }
// }

// impl<S> Middleware<S> for ErrorHandlers {
//     fn response(&self, _: &HttpRequest<S>, resp: HttpResponse) -> Result<Response, Error> {
//         if (resp.status().is_server_error() || resp.status().is_client_error())
//             && resp.body() == &Body::Empty
//         {
//             let error_response = if let Some(error) = resp.error() {
//                 ApiErrorResponse::from_fail(error.as_fail())
//             } else {
//                 let reason = resp.status().canonical_reason().unwrap_or("unknown error");
//                 ApiErrorResponse::with_detail(reason)
//             };
//             Ok(Response::Done(resp.into_builder().json(error_response)))
//         } else {
//             Ok(Response::Done(resp))
//         }
//     }
// }
