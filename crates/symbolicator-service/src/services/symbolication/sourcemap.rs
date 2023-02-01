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

impl SymbolicationActor {
    async fn collect_stacktrace_artifacts(
        &self,
        request: &JsProcessingSymbolicateStacktraces,
    ) -> HashMap<String, OwnedSourceMapCache> {
        let mut unique_abs_paths = HashSet::new();
        for stacktrace in &request.stacktraces {
            for frame in &stacktrace.frames {
                unique_abs_paths.insert(frame.abs_path.clone());
            }
        }

        let mut collected_artifacts = HashMap::new();
        let release_archive = self.sourcemaps.list_artifacts(request.source.clone()).await;

        dbg!(&release_archive);
        dbg!(&unique_abs_paths);

        for abs_path in unique_abs_paths {
            let Ok(abs_path_url) = Url::parse(&abs_path) else {
                continue
            };

            let mut source_file = None;
            let mut source_artifact = None;

            for candidate in get_release_file_candidate_urls(&abs_path_url) {
                dbg!(&candidate);
                if let Some(sa) = release_archive.get(&candidate) {
                    dbg!("found candidate", &candidate);

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

            let (Some(mut source_file), Some(source_artifact)) = (source_file, source_artifact) else {
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

            let Some(mut sourcemap_file) = sourcemap_file else {
                continue;
            };

            collected_artifacts.insert(
                abs_path,
                smcache_from_files(&mut source_file, &mut sourcemap_file),
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

        Ok(JsProcessingCompletedSymbolicationResponse {
            stacktraces,
            ..Default::default()
        })
    }
}

fn js_processing_symbolicate_stacktrace(
    stacktrace: JsProcessingRawStacktrace,
    artifacts: &HashMap<String, OwnedSourceMapCache>,
) -> CompleteJsStacktrace {
    let mut symbolicated_frames = vec![];
    let unsymbolicated_frames_iter = stacktrace.frames.into_iter();

    for mut frame in unsymbolicated_frames_iter {
        match js_processing_symbolicate_frame(&mut frame, &artifacts) {
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
        let token = smcache
            .get()
            .lookup(SourcePosition::new(
                frame.lineno.unwrap() - 1,
                frame.colno.unwrap() - 1,
            ))
            .unwrap();

        // dbg!(&token);

        let mut result = JsProcessingSymbolicatedFrame {
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
                pre_context: vec![],
                context_line: None,
                post_context: vec![],
                ..frame.clone()
            },
        };

        if let Some(file) = token.file() {
            let current_line = token.line();

            result.raw.context_line = Some(token.line_contents().unwrap_or_default().to_string());

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
    source_file: &NamedTempFile,
) -> Option<Url> {
    if let Some(header) = source_artifact.headers.get("Sourcemap") {
        abs_path.join(header).ok()
    } else if let Some(header) = source_artifact.headers.get("X-SourceMap") {
        abs_path.join(header).ok()
    } else {
        let sm_ref = locate_sourcemap_reference(BufReader::new(source_file.as_file())).ok()??;
        abs_path.join(sm_ref.get_url()).ok()
    }
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
}
