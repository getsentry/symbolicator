use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Scope {
    #[serde(rename = "global")]
    Global,
    Scoped(String),
}
