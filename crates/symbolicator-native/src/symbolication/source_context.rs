use std::collections::HashMap;

use futures::future;
use symbolicator_service::caching::{CacheEntry, CacheError};
use symbolicator_service::objects::{
    ObjectCandidate, ObjectDownloadInfo, ObjectFeatures, ObjectUseInfo,
};
use symbolicator_service::types::{Scope, ScrapingConfig};
use symbolicator_service::utils::http::is_valid_origin;
use symbolicator_sources::{HttpRemoteFile, RemoteFileUri, SourceId};

use crate::interface::{CompleteStacktrace, RawFrame};

use super::module_lookup::ModuleLookup;
use super::symbolicate::SymbolicationActor;

impl SymbolicationActor {
    pub async fn apply_source_context(
        &self,
        module_lookup: &mut ModuleLookup,
        stacktraces: &mut [CompleteStacktrace],
        scraping: &ScrapingConfig,
    ) {
        module_lookup
            .fetch_sources(self.objects.clone(), stacktraces)
            .await;

        // Map collected source contexts to the index of the module
        // and the list of frames they belong to and collect URLs
        // for remote source links.
        let mut remote_sources: HashMap<(Scope, url::Url), (usize, Vec<&mut RawFrame>)> =
            HashMap::new();
        {
            let debug_sessions = module_lookup.prepare_debug_sessions();

            for trace in stacktraces {
                for frame in &mut trace.frames {
                    let Some((scope, url, module_idx)) =
                        module_lookup.try_set_source_context(&debug_sessions, &mut frame.raw)
                    else {
                        continue;
                    };
                    let (_idx, frames) = remote_sources
                        .entry((scope, url))
                        .or_insert((module_idx, vec![]));
                    frames.push(&mut frame.raw);
                }
            }
        }

        // Download remote sources and update contexts.
        if !remote_sources.is_empty() {
            let cache = self.sourcefiles_cache.as_ref();
            let futures = remote_sources.into_iter().map(
                |((source_scope, url), (module_idx, frames))| async move {
                    let mut remote_file =
                        HttpRemoteFile::from_url(url.clone(), scraping.verify_ssl);

                    if scraping.enabled && is_valid_origin(&url, &scraping.allowed_origins) {
                        remote_file.headers.extend(
                            scraping
                                .headers
                                .iter()
                                .map(|(key, value)| (key.clone(), value.clone())),
                        );
                    }

                    let uri = remote_file.uri();
                    let res = cache
                        .fetch_file(&source_scope, remote_file.into(), true)
                        .await;

                    if let Ok(source) = res.as_ref() {
                        for frame in frames {
                            ModuleLookup::set_source_context(source, frame);
                        }
                    }

                    (module_idx, Self::object_candidate_for_sourcelink(uri, res))
                },
            );

            let candidates = future::join_all(futures).await;

            // Merge the candidates resulting from source downloads into the
            // respective objects.
            for (module_idx, candidate) in candidates {
                module_lookup.add_candidate(module_idx, &candidate);
            }
        }
    }

    // Creates an `ObjectCandidate` based on trying to download a
    // source file from a link.
    fn object_candidate_for_sourcelink<T>(
        location: RemoteFileUri,
        res: CacheEntry<T>,
    ) -> ObjectCandidate {
        let source = SourceId::new("sourcelink");
        let unwind = ObjectUseInfo::None;
        let debug = ObjectUseInfo::None;
        let download = match res {
            Ok(_) => ObjectDownloadInfo::Ok {
                features: ObjectFeatures {
                    has_sources: true,
                    ..Default::default()
                },
            },
            // Same logic as in `create_candidate_info`.
            Err(error) => match error {
                CacheError::NotFound => ObjectDownloadInfo::NotFound,
                CacheError::PermissionDenied(details) => ObjectDownloadInfo::NoPerm { details },
                CacheError::Malformed(_) => ObjectDownloadInfo::Malformed,
                err => ObjectDownloadInfo::Error {
                    details: err.to_string(),
                },
            },
        };

        ObjectCandidate {
            source,
            location,
            download,
            unwind,
            debug,
        }
    }
}
