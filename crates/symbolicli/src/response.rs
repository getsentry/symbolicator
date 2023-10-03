/// A wrapper around possible completed endpoint responses.
///
/// This allows us to support multiple independent types of symbolication.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum CompletedResponse {
    NativeSymbolication(CompletedSymbolicationResponse),
    JsSymbolication(CompletedJsSymbolicationResponse),
}

impl From<CompletedSymbolicationResponse> for CompletedResponse {
    fn from(response: CompletedSymbolicationResponse) -> Self {
        Self::NativeSymbolication(response)
    }
}

impl From<CompletedJsSymbolicationResponse> for CompletedResponse {
    fn from(response: CompletedJsSymbolicationResponse) -> Self {
        Self::JsSymbolication(response)
    }
}
