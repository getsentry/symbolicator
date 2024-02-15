use std::sync::Arc;

use serde::{Deserialize, Serialize};
use symbolic::common::DebugId;
use symbolicator_service::types::{RawObjectInfo, Scope};
use symbolicator_sources::SourceConfig;

#[derive(Debug, Clone)]
pub struct SymbolicateJvmStacktraces {
    pub scope: Scope,
    pub sources: Arc<[SourceConfig]>,
    pub exceptions: Vec<JvmException>,
    pub modules: Vec<JvmModule>,
    /// Whether to apply source context for the stack frames.
    pub apply_source_context: bool,
}

#[derive(Debug, Default, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct JvmFrame {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,

    pub abs_path: String,

    pub lineno: u32,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pre_context: Vec<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_line: Option<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub post_context: Vec<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_app: Option<bool>,
}

#[derive(Debug, Default, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct JvmException {
    #[serde(rename = "type")]
    ty: String,
    module: String,
    stacktrace: JvmStacktrace,
}

#[derive(Debug, Default, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct JvmStacktrace {
    frames: Vec<JvmFrame>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct JvmModule {
    uuid: DebugId,
}
