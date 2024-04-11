use std::collections::HashMap;

use futures::future;
use symbolicator_service::types::{Scope, ScrapingConfig};
use symbolicator_service::utils::http::is_valid_origin;
use symbolicator_sources::HttpRemoteFile;

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

        // Map collected source contexts to frames and collect URLs for remote source links.
        let mut remote_sources: HashMap<(Scope, url::Url), Vec<&mut RawFrame>> = HashMap::new();
        {
            let debug_sessions = module_lookup.prepare_debug_sessions();

            for trace in stacktraces {
                for frame in &mut trace.frames {
                    let Some(url) =
                        module_lookup.try_set_source_context(&debug_sessions, &mut frame.raw)
                    else {
                        continue;
                    };
                    if let Some(vec) = remote_sources.get_mut(&url) {
                        vec.push(&mut frame.raw)
                    } else {
                        remote_sources.insert(url, vec![&mut frame.raw]);
                    }
                }
            }
        }

        // Download remote sources and update contexts.
        if !remote_sources.is_empty() {
            let cache = self.sourcefiles_cache.as_ref();
            let futures =
                remote_sources
                    .into_iter()
                    .map(|((source_scope, url), frames)| async move {
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

                        if let Ok(source) = cache
                            .fetch_file(&source_scope, remote_file.into(), true)
                            .await
                        {
                            for frame in frames {
                                ModuleLookup::set_source_context(&source, frame);
                            }
                        }
                    });
            future::join_all(futures).await;
        }
    }
}
