use crate::actors::symcaches::SymCacheError;
use actix::{MailboxError, Message};
use failure::Fail;
use std::{collections::BTreeMap, fmt};
use symbolic::{common::Arch, symcache};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::actors::objects::SourceConfig;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Scope {
    #[serde(rename = "global")]
    Global,
    Scoped(String),
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
    #[serde(default)]
    pub meta: Meta,
    #[serde(default)]
    pub sources: Vec<SourceConfig>,
    #[serde(default)]
    pub threads: Vec<Thread>,
    #[serde(default)]
    pub modules: Vec<ObjectInfo>,
}

#[derive(Clone, Deserialize, Default)]
pub struct Meta {
    #[serde(default)]
    pub signal: Option<u32>,
    #[serde(default)]
    pub arch: Arch,
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

#[derive(Debug, Fail, derive_more::From)]
pub enum SymbolicationError {
    #[fail(display = "Failed sending message to symcache actor: {}", _0)]
    Mailbox(#[fail(cause)] MailboxError),

    #[fail(display = "Failed to get symcache: {}", _0)]
    SymCache(#[fail(cause)] SymCacheError),

    #[fail(display = "Failed to parse symcache during symbolication: {}", _0)]
    Parse(#[fail(cause)] symcache::SymCacheError),

    #[fail(display = "Symbol not found")]
    NotFound,
}
