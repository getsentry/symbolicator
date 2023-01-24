use std::{
    collections::{HashMap, HashSet},
    io::BufReader,
};

use reqwest::Url;
use sourcemap::locate_sourcemap_reference;
use symbolic::{
    common::{ByteView, SelfCell},
    sourcemapcache::{ScopeLookupResult, SourceMapCache, SourceMapCacheWriter, SourcePosition},
};
use symbolicator_sources::{SentryFileType, SentryRemoteFile};
use tempfile::NamedTempFile;
use url::Position;

use crate::{
    services::download::sentry::SearchArtifactResult,
    types::{
        CompleteJsStacktrace, FrameStatus, JsProcessingCompletedSymbolicationResponse,
        JsProcessingRawFrame, JsProcessingRawStacktrace, JsProcessingSymbolicatedFrame,
    },
};

use super::{JsProcessingSymbolicateStacktraces, SymbolicationActor};

/*
    1. Iterate over stacktrace-frames to gather all necessary abs_path (just like we do with
    preprocess_step in monolith)
    2. Fetch artifacts list from Sentry
        2a. TODO: Reach to 3rd party if scraping is allowed
    3. Cache all fetched artifacts/files
    4. Construct and cache SourceMapCaches
    5. Symbolicate stacktraces using preconstructed SMcaches
*/

/*

// Take a request:
{
    dist: null,
    stacktraces: [{
        frames: [
            {
                abs_path: 'http://example.com/assets/bundle.min.js'
            },
            {
                abs_path: 'http://example.com/assets/bundle.min.js'
            },
            {
                abs_path: 'http://example.com/assets/vendor.min.js'
            }
        ]
    }]
}

// Extract unique abs_path:
=> ['http://example.com/assets/bundle.min.js', 'http://example.com/assets/vendor.min.js']

// Transform abs_path to use wildcards:
=> ['~/assets/bundle.min.js', '~/assets/vendor.min.js']

// Check whether transformed abs_path already lives inside the cache
// SmCaches.get('~/assets/bundle.min.js')

// Ask the endpoint for all available artifacts:
[
    {
        dist: null,
        name: '~/assets/bundle.min.js',
        id: 1337,
        sha1: 87324,
        headers: {
            'X-SourceMap': 'bundle.min.js.map'
        }
    },
    {
        dist: null,
        name: '~/assets/bundle.min.js.map',
        id: 42,
        sha1: 023875,
        headers: {}
    },
    {
        dist: 'foo',
        name: '~/assets/bundle.min.js',
        id: 123,
        sha1: 0238758,
        headers: {}
    }
]

// Match artifact from the list based on the `dist and `name`: (`null` and `~/assets/bundle.min.js`)
{
    dist: null,
    name: '~/assets/bundle.min.js',
    id: 1337,
    headers: {
        'X-SourceMap': 'bundle.min.js.map'
    }
}

// Find sourcemap reference:
    - either SourceMap header
    - or X-SourceMap header
    - or find pragma inside the file contents itself

// Resolve sourcemap reference on top of abs_path of the frame we are processing
abs_path: 'http://example.com/assets/bundle.min.js
sm_ref: `bundle.min.js.map`
=> 'http://example.com/assets/bundle.min.js.map`

// Transform resolved sm_ref to use wildcards:
=> '~/assets/bundle.min.js.map'

// Match sourcemap artifact from the list based on the `dist` and resolved sourcemap ref: (`null` and '~/assets/bundle.min.js.map')
{
    dist: null,
    name: '~/assets/bundle.min.js.map',
    id: 42,
    headers: {}
}

// Create SourceMapCache from the source and sourcemap files
let smcache_writer = SourceMapCacheWriter::new(source_file, sourcemap_file).unwrap();
let mut smcache_buf = vec![];
smcache_writer.serialize(&mut smcache_buf).unwrap();

// Cache the SourceMapCache using `sha1` of the abs_path artifact as the key `87324`

// Process JS frame.

*/

pub type OwnedSourceMapCache = SelfCell<ByteView<'static>, SourceMapCache<'static>>;

fn smcache_from_files(
    source: &mut NamedTempFile,
    sourcemap: &mut NamedTempFile,
) -> OwnedSourceMapCache {
    use std::io::Read;

    let mut source_buf = String::new();
    source.as_file_mut().read_to_string(&mut source_buf).ok();

    let mut sourcemap_buf = String::new();
    sourcemap
        .as_file_mut()
        .read_to_string(&mut sourcemap_buf)
        .ok();

    let smcache_writer = SourceMapCacheWriter::new(&source_buf, &sourcemap_buf).unwrap();
    let mut smcache_buf = vec![];
    smcache_writer.serialize(&mut smcache_buf).unwrap();

    let byteview = ByteView::from_vec(smcache_buf);
    SelfCell::try_new::<symbolic::sourcemapcache::SourceMapCacheError, _>(byteview, |data| unsafe {
        SourceMapCache::parse(&*data)
    })
    .unwrap()
}

#[derive(Default, Debug)]
struct FrameSourceMapping {
    pub source_file: Option<NamedTempFile>,
    pub source_artifact: Option<SearchArtifactResult>,

    pub sourcemap_file: Option<NamedTempFile>,
    pub sourcemap_artifact: Option<SearchArtifactResult>,

    pub smcache: Option<OwnedSourceMapCache>,
}

impl SymbolicationActor {
    async fn collect_stacktrace_artifacts(
        &self,
        request: &JsProcessingSymbolicateStacktraces,
    ) -> HashMap<String, FrameSourceMapping> {
        let mut unique_abs_paths: HashSet<_> = HashSet::new();
        for stacktrace in &request.stacktraces {
            for frame in &stacktrace.frames {
                unique_abs_paths.insert(frame.abs_path.clone());
            }
        }

        let mut collected_artifacts = HashMap::new();
        let release_archive = self.sourcemaps.list_artifacts(request.source.clone()).await;

        for abs_path in unique_abs_paths {
            let mut frame_source_mapping = FrameSourceMapping {
                ..Default::default()
            };

            for candidate in get_release_file_candidate_urls(&abs_path) {
                if let Some(source_artifact) = release_archive.get(&candidate) {
                    if let Some(source_file) = self
                        .sourcemaps
                        .fetch_artifact(SentryRemoteFile::new(
                            request.source.clone(),
                            source_artifact.id.clone(),
                            SentryFileType::ReleaseArtifact,
                        ))
                        .await
                    {
                        frame_source_mapping.source_file = Some(source_file);
                        frame_source_mapping.source_artifact = Some(source_artifact.to_owned());
                        continue;
                    }
                }
            }

            if let Some(sourcemap_url) = get_sourcemap_reference(&frame_source_mapping) {
                if let Ok(resolved_sourcemap_url) = resolve_sourcemap_url(&abs_path, &sourcemap_url)
                {
                    for candidate in get_release_file_candidate_urls(&resolved_sourcemap_url) {
                        if let Some(sourcemap_artifact) = release_archive.get(&candidate) {
                            if let Some(sourcemap_file) = self
                                .sourcemaps
                                .fetch_artifact(SentryRemoteFile::new(
                                    request.source.clone(),
                                    sourcemap_artifact.id.clone(),
                                    SentryFileType::ReleaseArtifact,
                                ))
                                .await
                            {
                                frame_source_mapping.sourcemap_file = Some(sourcemap_file);
                                frame_source_mapping.sourcemap_artifact =
                                    Some(sourcemap_artifact.to_owned());
                                continue;
                            }
                        }
                    }
                }
            }

            if let Some(source_file) = frame_source_mapping.source_file.as_mut() {
                if let Some(sourcemap_file) = frame_source_mapping.sourcemap_file.as_mut() {
                    frame_source_mapping.smcache =
                        Some(smcache_from_files(source_file, sourcemap_file));
                }
            }

            collected_artifacts.insert(abs_path, frame_source_mapping);
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

        Ok(JsProcessingCompletedSymbolicationResponse {
            stacktraces,
            ..Default::default()
        })
    }
}

fn js_processing_symbolicate_stacktrace(
    stacktrace: JsProcessingRawStacktrace,
    artifacts: &HashMap<String, FrameSourceMapping>,
) -> CompleteJsStacktrace {
    let mut symbolicated_frames = vec![];
    let unsymbolicated_frames_iter = stacktrace.frames.into_iter();

    for mut frame in unsymbolicated_frames_iter {
        match js_processing_symbolicate_frame(&mut frame, &artifacts) {
            // TODO: Should frames filtering (eg. unsupported platform, or non-in_app)
            // be done here or by monolith?
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
    artifacts: &HashMap<String, FrameSourceMapping>,
) -> Result<JsProcessingSymbolicatedFrame, FrameStatus> {
    if let Some(artifact) = artifacts.get(&frame.abs_path) {
        if let Some(smcache) = &artifact.smcache {
            let token = smcache
                .get()
                .lookup(SourcePosition::new(
                    frame.lineno.unwrap() - 1,
                    frame.colno.unwrap() - 1,
                ))
                .unwrap();

            let file = token.file().unwrap();
            let current_line = token.line();

            let context_line = token.line_contents().unwrap_or_default();

            let pre_line = current_line.saturating_sub(5);
            let pre_context: Vec<_> = (pre_line..current_line)
                .filter_map(|line| file.line(line as usize))
                .map(|v| v.to_string())
                .collect();

            let post_line = current_line.saturating_add(5);
            let post_context: Vec<_> = (current_line + 1..=post_line)
                .filter_map(|line| file.line(line as usize))
                .map(|v| v.to_string())
                .collect();

            let result = JsProcessingSymbolicatedFrame {
                status: FrameStatus::Symbolicated,
                raw: JsProcessingRawFrame {
                    function: match token.scope() {
                        ScopeLookupResult::NamedScope(name) => Some(name.to_string()),
                        ScopeLookupResult::AnonymousScope => Some("<anonymous>".to_string()),
                        ScopeLookupResult::Unknown => Some("<unknown>".to_string()),
                    },
                    filename: Some(token.name().unwrap_or_default().to_string()),
                    abs_path: token.file_name().unwrap_or_default().to_string(),
                    lineno: Some(token.line()),
                    colno: Some(token.column()),
                    pre_context,
                    context_line: Some(context_line.to_string()),
                    post_context,
                    ..frame.clone()
                },
            };

            Ok(result)
        } else {
            Err(FrameStatus::Missing)
        }
    } else {
        Err(FrameStatus::Missing)
    }
}

// Transforms a full absolute url into 2 or 4 generalized options. Based on `ReleaseFile.normalize`.
// https://github.com/getsentry/sentry/blob/master/src/sentry/models/releasefile.py
fn get_release_file_candidate_urls(url: &str) -> Vec<String> {
    let mut urls = vec![];

    if let Ok(url) = Url::parse(url) {
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
    }

    urls
}

// Joins together frames `abs_path` and discovered sourcemap reference.
fn resolve_sourcemap_url(abs_path: &str, sourcemap_location: &str) -> anyhow::Result<String> {
    let base = Url::parse(abs_path)?;
    base.join(sourcemap_location)
        .map(|url| url.to_string())
        .map_err(|e| e.into())
}

fn get_sourcemap_reference(frame_source_mapping: &FrameSourceMapping) -> Option<String> {
    if let Some(source_artifact) = &frame_source_mapping.source_artifact {
        if let Some(header) = source_artifact.headers.get("Sourcemap") {
            return Some(header.to_owned());
        }

        if let Some(header) = source_artifact.headers.get("X-SourceMap") {
            return Some(header.to_owned());
        }
    }

    if let Some(source_file) = &frame_source_mapping.source_file {
        return locate_sourcemap_reference(BufReader::new(source_file.as_file()))
            .map(|v| v.map(|s| s.get_url().to_string()))
            .unwrap_or_default();
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_release_file_candidate_urls() {
        let url = "https://example.com/assets/bundle.min.js";
        let expected = vec![
            "https://example.com/assets/bundle.min.js",
            "~/assets/bundle.min.js",
        ];
        assert_eq!(get_release_file_candidate_urls(url), expected);

        let url = "https://example.com/assets/bundle.min.js?foo=1&bar=baz";
        let expected = vec![
            "https://example.com/assets/bundle.min.js?foo=1&bar=baz",
            "https://example.com/assets/bundle.min.js",
            "~/assets/bundle.min.js?foo=1&bar=baz",
            "~/assets/bundle.min.js",
        ];
        assert_eq!(get_release_file_candidate_urls(url), expected);

        let url = "https://example.com/assets/bundle.min.js#wat";
        let expected = vec![
            "https://example.com/assets/bundle.min.js",
            "~/assets/bundle.min.js",
        ];
        assert_eq!(get_release_file_candidate_urls(url), expected);

        let url = "https://example.com/assets/bundle.min.js?foo=1&bar=baz#wat";
        let expected = vec![
            "https://example.com/assets/bundle.min.js?foo=1&bar=baz",
            "https://example.com/assets/bundle.min.js",
            "~/assets/bundle.min.js?foo=1&bar=baz",
            "~/assets/bundle.min.js",
        ];
        assert_eq!(get_release_file_candidate_urls(url), expected);
    }
}
