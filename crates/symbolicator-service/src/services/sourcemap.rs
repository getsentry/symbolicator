//! Service for retrieving SourceMap artifacts.

use std::collections::HashMap;
use std::sync::Arc;

use symbolicator_sources::{SentryFileId, SentryFileType, SentryRemoteFile, SentrySourceConfig};
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

    pub async fn fetch_artifact(
        &self,
        source: Arc<SentrySourceConfig>,
        file_id: SentryFileId,
    ) -> Option<NamedTempFile> {
        let mut temp_file = NamedTempFile::new().unwrap();
        fetch_file(
            self.download_svc.clone(),
            SentryRemoteFile::new(source, file_id, SentryFileType::ReleaseArtifact).into(),
            &mut temp_file,
        )
        .await
        .map(|_| temp_file)
        .ok()
    }
}
