use actix::Message;
use failure::{Backtrace, Fail};
use std::{collections::BTreeMap, fmt, sync::Arc};
use symbolic::common::Arch;
use url::Url;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Deserialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SourceConfig {
    Http {
        id: String,
        #[serde(with = "url_serde")]
        url: Url,

        #[serde(default)]
        is_public: bool,
    },
}

impl SourceConfig {
    pub fn get_base_url(&self) -> &Url {
        match *self {
            SourceConfig::Http { ref url, .. } => &url,
        }
    }

    pub fn is_public(&self) -> bool {
        match *self {
            SourceConfig::Http { is_public, .. } => is_public,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(untagged)]
pub enum Scope {
    #[serde(rename = "global")]
    Global,
    Scoped(String),
}

impl AsRef<str> for Scope {
    fn as_ref(&self) -> &str {
        match *self {
            Scope::Scoped(ref s) => &s,
            Scope::Global => "global",
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Frame {
    pub addr: HexValue,

    pub module: Option<String>, // NOTE: This is "package" in Sentry
    pub name: Option<String>,   // Only present if ?demangle was set
    pub language: Option<String>,
    pub symbol: Option<String>,
    pub symbol_address: Option<HexValue>,
    pub file: Option<String>,
    pub line: Option<u64>,
    pub line_address: Option<HexValue>, // NOTE: This does not exist in Sentry
}

#[derive(Deserialize)]
pub struct ObjectInfo {
    pub debug_id: String,
    pub code_id: Option<String>,

    #[serde(default)]
    pub debug_name: Option<String>,

    #[serde(default)]
    pub code_name: Option<String>,

    pub address: HexValue,

    #[serde(default)]
    pub size: Option<u64>,
}

#[derive(Clone, Debug, Copy)]
pub struct HexValue(pub u64);

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

impl<'d> fmt::Display for HexValue {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}

impl Serialize for HexValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.to_string().serialize(serializer)
    }
}

#[derive(Deserialize)]
pub struct SymbolicateFramesRequest {
    pub meta: Meta,
    #[serde(default)]
    pub sources: Vec<SourceConfig>,
    #[serde(default)]
    pub threads: Vec<Thread>,
    #[serde(default)]
    pub modules: Vec<ObjectInfo>,
}

#[derive(Clone, Deserialize)]
pub struct Meta {
    #[serde(default)]
    pub signal: Option<u32>,
    #[serde(default)]
    pub arch: Arch,
    pub scope: Scope,
}

#[derive(Deserialize)]
pub struct Thread {
    pub registers: BTreeMap<String, HexValue>,
    #[serde(flatten)]
    pub stacktrace: Stacktrace,
}

#[derive(Serialize, Deserialize)]
pub struct Stacktrace {
    pub frames: Vec<Frame>,
}

#[derive(Serialize, Deserialize)]
pub struct ErrorResponse(pub String);

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum SymbolicateFramesResponse {
    //Pending {
    //retry_after: usize,
    //},
    Completed {
        stacktraces: Vec<Stacktrace>,
        errors: Vec<ErrorResponse>,
    },
}

impl Message for SymbolicateFramesRequest {
    type Result = Result<SymbolicateFramesResponse, SymbolicationError>;
}

#[derive(Debug, Fail)]
pub enum SymbolicationErrorKind {
    #[fail(display = "failed sending message to symcache actor")]
    Mailbox,

    #[fail(display = "failed to get symcache")]
    SymCache,

    #[fail(display = "failed to parse symcache during symbolication")]
    Parse,

    #[fail(display = "symbol not found")]
    NotFound,

    #[fail(display = "failed to look into cache")]
    Caching,
}

symbolic::common::derive_failure!(
    SymbolicationError,
    SymbolicationErrorKind,
    doc = "Errors during symbolication"
);

/// This type only exists to have a working impl of `Fail` for `Arc<T> where T: Fail`. We cannot
/// contribute a blanket impl upstream because it would conflict with at least this blanket impl
/// from failure: `impl<E: StdError + Send + Sync + 'static> Fail for E`
#[derive(Debug, Clone)]
pub struct ArcFail<T>(pub Arc<T>);

impl<T: Fail> Fail for ArcFail<T> {
    fn cause(&self) -> Option<&Fail> {
        self.0.cause()
    }

    fn backtrace(&self) -> Option<&Backtrace> {
        self.0.backtrace()
    }
}

impl<T: fmt::Display> fmt::Display for ArcFail<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        self.0.fmt(f)
    }
}
