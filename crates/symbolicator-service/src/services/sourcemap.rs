//! Service for retrieving SourceMap artifacts.

use std::collections::HashMap;
use std::sync::Arc;

use symbolicator_sources::{SentryRemoteFile, SentrySourceConfig};
use tempfile::NamedTempFile;

use crate::services::download::sentry::SearchArtifactResult;
use crate::services::download::DownloadService;

use super::fetch_file;

#[derive(Debug, Clone)]
pub struct SourceMapService {
    download_svc: Arc<DownloadService>,
}

impl SourceMapService {
    pub fn new(download_svc: Arc<DownloadService>) -> Self {
        Self { download_svc }
    }

    pub async fn list_artifacts(
        &self,
        source: Arc<SentrySourceConfig>,
    ) -> HashMap<String, SearchArtifactResult> {
        self.download_svc
            .list_artifacts(source)
            .await
            .into_iter()
            .map(|artifact| (artifact.name.clone(), artifact))
            .collect()
    }

    pub async fn fetch_artifact(&self, file_id: SentryRemoteFile) -> Option<NamedTempFile> {
        let mut temp_file = NamedTempFile::new().unwrap();
        // FIXME: Not sure why I cannot infere the correct type here
        // and not let me return `fetch_file` instead.
        fetch_file(self.download_svc.clone(), file_id.into(), &mut temp_file)
            .await
            .map(|_| temp_file)
            .ok()
    }
}
