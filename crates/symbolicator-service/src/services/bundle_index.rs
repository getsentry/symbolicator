use std::collections::HashMap;

use serde::Deserialize;
use symbolic::common::DebugId;

#[derive(Debug, Deserialize)]
struct BundleMeta {
    bundle_id: String,
}

#[derive(Debug, Deserialize)]
pub struct BundleIndex {
    bundles: Vec<BundleMeta>,

    #[serde(default)]
    files_by_url: HashMap<String, Vec<usize>>,
    #[serde(default)]
    files_by_debug_id: HashMap<DebugId, Vec<usize>>,
}

impl BundleIndex {
    pub fn get_bundle_id_by_url(&self, url: &str) -> Option<&str> {
        let idx = self.files_by_url.get(url)?.last()?;
        let meta = self.bundles.get(*idx)?;
        Some(&meta.bundle_id)
    }
    pub fn get_bundle_id_by_debug_id(&self, debug_id: DebugId) -> Option<&str> {
        let idx = self.files_by_debug_id.get(&debug_id)?.last()?;
        let meta = self.bundles.get(*idx)?;
        Some(&meta.bundle_id)
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[test]
    fn deserialize_from_json() {
        let bundle_json = r#"{
            "bundles": [
                {"bundle_id": "bundle_1"},
                {"bundle_id": "bundle_2"},
                {"bundle_id": "bundle_3"}
            ],
            "files_by_url": {
                "~/url/in/bundle": [1, 0]
            },
            "files_by_debug_id": {
                "5b65abfb-2338-4f0b-b3b9-64c8f734d43f": [2]
            }
        }"#;

        let index: BundleIndex = serde_json::from_str(bundle_json).unwrap();

        let bundle_id = index.get_bundle_id_by_url("~/url/in/bundle");
        assert_eq!(bundle_id, Some("bundle_1"));

        let bundle_id = index.get_bundle_id_by_debug_id(
            DebugId::from_str("5b65abfb-2338-4f0b-b3b9-64c8f734d43f").unwrap(),
        );
        assert_eq!(bundle_id, Some("bundle_3"));
    }
}
