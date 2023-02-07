use std::collections::{HashMap, HashSet};

use reqwest::Url;
use sourcemap::locate_sourcemap_reference;
use symbolic::sourcemapcache::{ScopeLookupResult, SourcePosition};
use tempfile::NamedTempFile;
use url::Position;

use crate::services::{download::sentry::SearchArtifactResult, sourcemap::OwnedSourceMapCache};
use crate::types::{
    CompleteJsStacktrace, FrameStatus, JsProcessingCompletedSymbolicationResponse,
    JsProcessingRawFrame, JsProcessingRawStacktrace, JsProcessingSymbolicatedFrame,
};

use super::{JsProcessingSymbolicateStacktraces, SymbolicationActor};

// TODO(sourcemap): Use our generic caching solution for all Artifacts.
// TODO(sourcemap): Rename all `JsProcessing_` and `js_processing_` prefixed names to something we agree on.

// TODO(sourcemap): Move this and related functions to its own file, similar to how `ModuleLookup` works.
impl SymbolicationActor {
    // TODO(sourcemap): Handle 3rd party servers fetching (payload should decide about it, as it's user-configurable in the UI)
    async fn collect_stacktrace_artifacts(
        &self,
        request: &JsProcessingSymbolicateStacktraces,
    ) -> HashMap<String, OwnedSourceMapCache> {
        // TODO(sourcemap): Fetch all files concurrently using futures join
        let mut unique_abs_paths = HashSet::new();
        for stacktrace in &request.stacktraces {
            for frame in &stacktrace.frames {
                unique_abs_paths.insert(frame.abs_path.clone());
            }
        }

        let mut collected_artifacts = HashMap::new();
        let release_archive = self.sourcemaps.list_artifacts(request.source.clone()).await;

        for abs_path in unique_abs_paths {
            let Ok(abs_path_url) = Url::parse(&abs_path) else {
                continue
            };

            let mut source_file = None;
            let mut source_artifact = None;

            for candidate in get_release_file_candidate_urls(&abs_path_url) {
                if let Some(sa) = release_archive.get(&candidate) {
                    if let Some(sf) = self
                        .sourcemaps
                        .fetch_artifact(request.source.clone(), sa.id.clone())
                        .await
                    {
                        source_file = Some(sf);
                        source_artifact = Some(sa.to_owned());
                        break;
                    }
                }
            }

            // TODO(sourcemap): Report missing source error
            let (Some(source_file), Some(source_artifact)) = (source_file, source_artifact) else {
                continue;
            };

            let Some(sourcemap_url) = resolve_sourcemap_url(&abs_path_url, &source_artifact, &source_file) else {
                continue;
            };

            let mut sourcemap_file = None;
            for candidate in get_release_file_candidate_urls(&sourcemap_url) {
                if let Some(sourcemap_artifact) = release_archive.get(&candidate) {
                    if let Some(sf) = self
                        .sourcemaps
                        .fetch_artifact(request.source.clone(), sourcemap_artifact.id.clone())
                        .await
                    {
                        sourcemap_file = Some(sf);
                        break;
                    }
                }
            }

            // TODO(sourcemap): Report missing source error
            let Some(sourcemap_file) = sourcemap_file else {
                continue;
            };

            collected_artifacts.insert(
                abs_path,
                self.sourcemaps
                    .fetch_cache(&source_file, &sourcemap_file)
                    .await
                    // TODO: properly report errors here
                    .unwrap(),
            );
        }

        collected_artifacts
    }

    #[tracing::instrument(skip_all)]
    pub async fn js_processing_symbolicate(
        &self,
        request: JsProcessingSymbolicateStacktraces,
    ) -> Result<JsProcessingCompletedSymbolicationResponse, anyhow::Error> {
        let artifacts = self.collect_stacktrace_artifacts(&request).await;

        let stacktraces: Vec<_> = request
            .stacktraces
            .into_iter()
            .map(|trace| js_processing_symbolicate_stacktrace(trace, &artifacts))
            .collect();

        Ok(JsProcessingCompletedSymbolicationResponse { stacktraces })
    }
}

fn js_processing_symbolicate_stacktrace(
    stacktrace: JsProcessingRawStacktrace,
    artifacts: &HashMap<String, OwnedSourceMapCache>,
) -> CompleteJsStacktrace {
    let mut symbolicated_frames = vec![];
    let unsymbolicated_frames_iter = stacktrace.frames.into_iter();

    for mut frame in unsymbolicated_frames_iter {
        match js_processing_symbolicate_frame(&mut frame, artifacts) {
            Ok(frame) => symbolicated_frames.push(frame),
            Err(_status) => {
                symbolicated_frames.push(JsProcessingSymbolicatedFrame {
                    status: FrameStatus::Missing,
                    raw: frame,
                });
            }
        }
    }

    CompleteJsStacktrace {
        frames: symbolicated_frames,
    }
}

fn js_processing_symbolicate_frame(
    frame: &mut JsProcessingRawFrame,
    artifacts: &HashMap<String, OwnedSourceMapCache>,
) -> Result<JsProcessingSymbolicatedFrame, FrameStatus> {
    if let Some(smcache) = artifacts.get(&frame.abs_path) {
        // TODO(sourcemap): Report invalid source location error
        let sp = SourcePosition::new(frame.lineno.unwrap() - 1, frame.colno.unwrap() - 1);
        let token = smcache.get().lookup(sp).unwrap();

        let function_name = match token.scope() {
            ScopeLookupResult::NamedScope(name) => name.to_string(),
            ScopeLookupResult::AnonymousScope => "<anonymous>".to_string(),
            ScopeLookupResult::Unknown => {
                // Fallback to minified function name
                frame.function.clone().unwrap_or("<unknown>".to_string())
            }
        };

        let mut result = JsProcessingSymbolicatedFrame {
            status: FrameStatus::Symbolicated,
            raw: JsProcessingRawFrame {
                function: Some(fold_function_name(&function_name)),
                filename: frame.filename.clone(),
                abs_path: token.file_name().unwrap_or("<unknown>").to_string(),
                // TODO: Decide where to do off-by-1 calculations
                lineno: Some(token.line().saturating_add(1)),
                colno: Some(token.column().saturating_add(1)),
                pre_context: vec![],
                context_line: None,
                post_context: vec![],
            },
        };

        if let Some(file) = token.file() {
            result.raw.filename = file.name().map(ToString::to_string);

            let current_line = token.line();

            result.raw.context_line = token.line_contents().map(ToString::to_string);

            let pre_line = current_line.saturating_sub(5);
            result.raw.pre_context = (pre_line..current_line)
                .filter_map(|line| file.line(line as usize))
                .map(|v| v.to_string())
                .collect();

            let post_line = current_line.saturating_add(5);
            result.raw.post_context = (current_line + 1..=post_line)
                .filter_map(|line| file.line(line as usize))
                .map(|v| v.to_string())
                .collect();
        }

        Ok(result)
    } else {
        Err(FrameStatus::Missing)
    }
}

// Transforms a full absolute url into 2 or 4 generalized options. Based on `ReleaseFile.normalize`.
// https://github.com/getsentry/sentry/blob/master/src/sentry/models/releasefile.py
fn get_release_file_candidate_urls(url: &Url) -> Vec<String> {
    let mut urls = vec![];

    // Absolute without fragment
    urls.push(url[..Position::AfterQuery].to_string());

    // Absolute without query
    if url.query().is_some() {
        urls.push(url[..Position::AfterPath].to_string())
    }

    // Relative without fragment
    urls.push(format!(
        "~{}",
        &url[Position::BeforePath..Position::AfterQuery]
    ));

    // Relative without query
    if url.query().is_some() {
        urls.push(format!(
            "~{}",
            &url[Position::BeforePath..Position::AfterPath]
        ));
    }

    urls
}

// Joins together frames `abs_path` and discovered sourcemap reference.
fn resolve_sourcemap_url(
    abs_path: &Url,
    source_artifact: &SearchArtifactResult,
    mut source_file: &NamedTempFile,
) -> Option<Url> {
    if let Some(header) = source_artifact.headers.get("Sourcemap") {
        abs_path.join(header).ok()
    } else if let Some(header) = source_artifact.headers.get("X-SourceMap") {
        abs_path.join(header).ok()
    } else {
        use std::io::Seek;

        let sm_ref = locate_sourcemap_reference(source_file.as_file()).ok()??;
        source_file.rewind().ok()?;
        abs_path.join(sm_ref.get_url()).ok()
    }
}

/// Fold multiple consecutive occurences of the same property name into a single group, excluding the last component.
///
/// foo | foo
/// foo.foo | foo.foo
/// foo.foo.foo | {foo#2}.foo
/// bar.foo.foo | bar.foo.foo
/// bar.foo.foo.foo | bar.{foo#2}.foo
/// bar.foo.foo.onError | bar.{foo#2}.onError
/// bar.bar.bar.foo.foo.onError | {bar#3}.{foo#2}.onError
/// bar.foo.foo.bar.bar.onError | bar.{foo#2}.{bar#2}.onError
fn fold_function_name(function_name: &str) -> String {
    let mut parts: Vec<_> = function_name.split('.').collect();

    if parts.len() == 1 {
        return function_name.to_string();
    }

    // unwrap: `parts` has at least a single item.
    let tail = parts.pop().unwrap();
    let mut grouped: Vec<Vec<&str>> = vec![vec![]];

    for part in parts {
        // unwrap: we initialized `grouped` with at least a single slice.
        let current_group = grouped.last_mut().unwrap();
        if current_group.is_empty() || current_group.last() == Some(&part) {
            current_group.push(part);
        } else {
            grouped.push(vec![part]);
        }
    }

    let folded = grouped
        .iter()
        .map(|group| {
            // unwrap: each group contains at least a single item.
            if group.len() == 1 {
                group.first().unwrap().to_string()
            } else {
                format!("{{{}#{}}}", group.first().unwrap(), group.len())
            }
        })
        .collect::<Vec<_>>()
        .join(".");

    format!("{folded}.{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_release_file_candidate_urls() {
        let url = "https://example.com/assets/bundle.min.js".parse().unwrap();
        let expected = vec![
            "https://example.com/assets/bundle.min.js",
            "~/assets/bundle.min.js",
        ];
        assert_eq!(get_release_file_candidate_urls(&url), expected);

        let url = "https://example.com/assets/bundle.min.js?foo=1&bar=baz"
            .parse()
            .unwrap();
        let expected = vec![
            "https://example.com/assets/bundle.min.js?foo=1&bar=baz",
            "https://example.com/assets/bundle.min.js",
            "~/assets/bundle.min.js?foo=1&bar=baz",
            "~/assets/bundle.min.js",
        ];
        assert_eq!(get_release_file_candidate_urls(&url), expected);

        let url = "https://example.com/assets/bundle.min.js#wat"
            .parse()
            .unwrap();
        let expected = vec![
            "https://example.com/assets/bundle.min.js",
            "~/assets/bundle.min.js",
        ];
        assert_eq!(get_release_file_candidate_urls(&url), expected);

        let url = "https://example.com/assets/bundle.min.js?foo=1&bar=baz#wat"
            .parse()
            .unwrap();
        let expected = vec![
            "https://example.com/assets/bundle.min.js?foo=1&bar=baz",
            "https://example.com/assets/bundle.min.js",
            "~/assets/bundle.min.js?foo=1&bar=baz",
            "~/assets/bundle.min.js",
        ];
        assert_eq!(get_release_file_candidate_urls(&url), expected);
    }

    #[test]
    fn test_fold_function_name() {
        assert_eq!(fold_function_name("foo"), "foo");
        assert_eq!(fold_function_name("foo.foo"), "foo.foo");
        assert_eq!(fold_function_name("foo.foo.foo"), "{foo#2}.foo");
        assert_eq!(fold_function_name("bar.foo.foo"), "bar.foo.foo");
        assert_eq!(fold_function_name("bar.foo.foo.foo"), "bar.{foo#2}.foo");
        assert_eq!(
            fold_function_name("bar.foo.foo.onError"),
            "bar.{foo#2}.onError"
        );
        assert_eq!(
            fold_function_name("bar.bar.bar.foo.foo.onError"),
            "{bar#3}.{foo#2}.onError"
        );
        assert_eq!(
            fold_function_name("bar.foo.foo.bar.bar.onError"),
            "bar.{foo#2}.{bar#2}.onError"
        );
    }
}
