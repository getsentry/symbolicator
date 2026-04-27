use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use symbolic::common::DebugId;
use symbolic::debuginfo::sourcebundle::{SourceBundle, SourceFileInfo, SourceFileType};
use symbolicator_js::interface::ResolvedWith;
use walkdir::WalkDir;
use zip::ZipArchive;

#[derive(Debug, Clone)]
struct LookupKey {
    release: Option<Arc<str>>,
    dist: Option<Arc<str>>,
    url: Option<Arc<str>>,
    debug_id: Option<DebugId>,
}

#[derive(Debug, Clone)]
struct FoundBundle {
    resolved_with: ResolvedWith,
    id: usize,
    path: PathBuf,
}

#[derive(Debug, Clone)]
struct LookupResponse(Box<[FoundBundle]>);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ReleaseDistUrl {
    release: Option<Arc<str>>,
    dist: Option<Arc<str>>,
    url: Arc<str>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct SourceBundleManifest {
    #[serde(default)]
    pub files: HashMap<String, SourceFileInfo>,
    #[serde(default)]
    pub attributes: HashMap<String, Arc<str>>,
}

#[derive(Debug, Clone, Default)]
struct Index {
    by_debug_id: HashMap<DebugId, HashSet<usize>>,
    by_url: HashMap<ReleaseDistUrl, HashSet<usize>>,
    bundles: Vec<PathBuf>,
}

impl Index {
    fn build(path: impl AsRef<Path>) -> Result<Self> {
        let mut out = Self::default();
        for entry in WalkDir::new(path) {
            let entry = entry.context("Accessing files")?;

            if !entry.file_type().is_file()
                || entry.path().extension().is_none_or(|ext| ext != "zip")
            {
                continue;
            }

            let archive = std::fs::File::open(entry.path())?;
            let mut archive = ZipArchive::new(archive)?;

            let manifest = archive.by_name("manifest.json")?;
            let manifest: SourceBundleManifest = serde_json::from_reader(manifest)?;

            dbg!(&manifest);
            let release = manifest.attributes.get("release").cloned();
            let dist = manifest.attributes.get("dist").cloned();

            for file in manifest.files.values() {
                if file.ty() != Some(SourceFileType::MinifiedSource) {
                    continue;
                }

                let Some(url) = file.url() else {
                    continue;
                };

                out.by_url
                    .entry(ReleaseDistUrl {
                        release: release.clone(),
                        dist: dist.clone(),
                        url: url.into(),
                    })
                    .or_default()
                    .insert(out.bundles.len());

                if let Some(debug_id) = file.debug_id() {
                    out.by_debug_id
                        .entry(debug_id.to_owned())
                        .or_default()
                        .insert(out.bundles.len());
                }
            }

            out.bundles.push(entry.path().to_owned());
        }

        Ok(out)
    }

    fn lookup(&self, key: LookupKey) -> LookupResponse {
        let mut out = Vec::new();

        if let Some(debug_id) = key.debug_id {
            for id in self
                .by_debug_id
                .get(&debug_id)
                .into_iter()
                .flat_map(|s| s.iter().copied())
            {
                out.push(FoundBundle {
                    resolved_with: ResolvedWith::DebugId,
                    id,
                    path: self.bundles[id].clone(),
                });
            }
        }

        if let Some(url) = key.url {
            for id in self
                .by_url
                .get(&ReleaseDistUrl {
                    release: key.release.clone(),
                    dist: key.dist.clone(),
                    url: url.clone(),
                })
                .into_iter()
                .flat_map(|s| s.iter().copied())
            {
                out.push(FoundBundle {
                    resolved_with: ResolvedWith::Release,
                    id,
                    path: self.bundles[id].clone(),
                });
            }
        }

        LookupResponse(out.into())
    }
}

#[test]
fn test_existing_bundle() {
    let index = Index::build(
        "/Users/sebastian/code/symbolicator/tests/fixtures/sourcemaps/e2e_node_debugid",
    )
    .unwrap();

    dbg!(index.lookup(LookupKey {
        release: None,
        dist: None,
        url: None,
        debug_id: Some("2f259f80-58b7-44cb-d7cd-de1505e7e718".parse().unwrap()),
    }));
}
