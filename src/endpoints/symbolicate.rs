use crate::actors::debugsymbols::DebugInfoId;
use crate::actors::debugsymbols::FetchDebugInfo;
use crate::app::ServiceState;
use actix::ResponseFuture;
use futures::future::join_all;
use futures::future::Future;

use actix_web::http::Method;
use actix_web::Json;
use actix_web::State;

use failure::Error;

use serde::Deserialize;
use serde::Deserializer;

use crate::actors::debugsymbols::FileType;
use crate::actors::debugsymbols::SourceConfig;
use crate::app::ServiceApp;

use symbolic::common::split_path;

#[derive(Deserialize)]
struct Request {
    sources: Vec<SourceConfig>,
    frames: Vec<Frame>,
    modules: Vec<DebugInfo>,
}

#[derive(Serialize, Deserialize)]
struct Frame {}

#[derive(Serialize, Deserialize)]
struct ErrorResponse {}

#[derive(Deserialize)]
struct DebugInfo {
    debug_id: String,
    code_id: String,

    #[serde(default)]
    debug_name: Option<String>,

    #[serde(default)]
    code_name: Option<String>,
    //address: HexValue,
    //size: u64,

    //#[serde(default)]
    //module: Option<String>,

    //#[serde(default)]
    //name: Option<String>,

    //#[serde(default)]
    //symbol: Option<String>,

    //#[serde(default)]
    //symbol_address: Option<HexValue>,

    //#[serde(default)]
    //file: Option<String>,

    //#[serde(default)]
    //line: Option<u64>,

    //#[serde(default)]
    //line_address: Option<HexValue>,
}

struct HexValue(u64);

impl<'de> Deserialize<'de> for HexValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let string: &str = Deserialize::deserialize(deserializer)?;
        if string.starts_with("0x") || string.starts_with("0X") {
            if let Ok(x) = u64::from_str_radix(&string[2..], 16) {
                return Ok(HexValue(x));
            }
        }

        Err(serde::de::Error::invalid_value(
            serde::de::Unexpected::Str(string),
            &"a hex string starting with 0x",
        ))
    }
}

#[derive(Serialize)]
#[serde(tag = "status")]
enum Response {
    //Pending {
    //retry_after: usize,
    //},
    Completed {
        frames: Vec<Frame>,
        errors: Vec<ErrorResponse>,
    },
}

fn symbolicate_frames(
    state: State<ServiceState>,
    request: Json<Request>,
) -> ResponseFuture<Json<Response>, Error> {
    let debug_symbols = state.debug_symbols.clone();
    let request = request.into_inner();
    let sources = request.sources.clone();

    let results = join_all(request.modules.into_iter().map(move |debug_info| {
        debug_symbols
            .send(FetchDebugInfo {
                identifier: DebugInfoId {
                    debug_id: debug_info.debug_id.parse().ok(),
                    code_id: Some(debug_info.code_id),
                    debug_name: debug_info
                        .debug_name
                        .as_ref()
                        .map(|x| split_path(x).1.to_owned()), // TODO
                    code_name: debug_info
                        .code_name
                        .as_ref()
                        .map(|x| split_path(x).1.to_owned()), // TODO
                    filetype: FileType::Debug,
                },
                configs: sources.clone(),
            })
            .map_err(From::from)
    }));

    Box::new(results.and_then(|_| {
        Ok(Json(Response::Completed {
            frames: vec![],
            errors: vec![],
        }))
    }))
}

pub fn register(app: ServiceApp) -> ServiceApp {
    app.resource("/symbolicate", |r| {
        r.method(Method::POST).with(symbolicate_frames);
    })
}
