//! Symbolication of JS/SourceMap requests.
//!
//! # Metrics
//!
//! - `js.unsymbolicated_frames`: The number of unsymbolicated frames, per event.
//!   Should be `0` in the best case, as we obviously should symbolicate :-)
//!
//! - `js.missing_sourcescontent`: The number of frames, per event, that have no embedded sources.
//!   Should be `0` in the best case, as the SourceMaps we use should have embedded sources.
//!   If they don’t, we have to fall back to applying source context from elsewhere.
//!
//! - `js.api_requests`: The number of (potentially cached) API requests, per event.
//!   Should be `1` in the best case, as `prefetch_artifacts` should provide us with everything we need.
//!
//! - `js.queried_bundles` / `js.fetched_bundles`: The number of artifact bundles the API gave us,
//!   and the ones we ended up using.
//!   Should both be `1` in the best case, as a single bundle should ideally serve all our needs.
//!   Otherwise `queried` and `fetched` should be the same, as a difference between the two means
//!   that multiple API requests gave us duplicated bundles.
//!
//! - `js.queried_artifacts` / `js.fetched_artifacts`: The number of individual artifacts the API
//!   gave us, and the ones we ended up using.
//!   Should both be `0` as we should not be using individual artifacts but rather bundles.
//!   Otherwise, `queried` should be close to `fetched`. If they differ, it means the API is sending
//!   us a lot of candidate artifacts that we don’t end up using, or multiple API requests give us
//!   duplicated artifacts.
//!
//! - `js.scraped_files`: The number of files that were scraped from the Web.
//!   Should be `0`, as we should find/use files from within bundles or as individual artifacts.

use std::sync::Arc;

use symbolic::sourcemapcache::{ScopeLookupResult, SourcePosition};
use symbolicator_sources::SentrySourceConfig;

use crate::caching::{CacheEntry, CacheError};
use crate::services::sourcemap_lookup::{CachedFile, SourceMapLookup};
use crate::types::{
    CompletedJsSymbolicationResponse, JsFrame, JsFrameStatus, JsStacktrace, RawObjectInfo, Scope,
    SymbolicatedJsFrame, SymbolicatedJsStacktrace,
};

use super::source_context::get_context_lines;
use super::SymbolicationActor;

#[derive(Debug, Clone)]
pub struct SymbolicateJsStacktraces {
    pub scope: Scope,
    pub source: Arc<SentrySourceConfig>,
    pub dist: Option<String>,
    pub stacktraces: Vec<JsStacktrace>,
    pub modules: Vec<RawObjectInfo>,
    pub allow_scraping: bool,
}

impl SymbolicationActor {
    #[tracing::instrument(skip_all)]
    pub async fn symbolicate_js(
        &self,
        mut request: SymbolicateJsStacktraces,
    ) -> Result<CompletedJsSymbolicationResponse, anyhow::Error> {
        let mut raw_stacktraces = std::mem::take(&mut request.stacktraces);
        let mut lookup = SourceMapLookup::new(self.sourcemaps.clone(), request);
        lookup.prefetch_artifacts(&raw_stacktraces).await;

        let mut unsymbolicated_frames = 0;
        let mut missing_sourcescontent = 0;

        let num_stacktraces = raw_stacktraces.len();
        let mut stacktraces = Vec::with_capacity(num_stacktraces);

        for raw_stacktrace in &mut raw_stacktraces {
            let num_frames = raw_stacktrace.frames.len();
            let mut symbolicated_frames = Vec::with_capacity(num_frames);

            for raw_frame in &mut raw_stacktrace.frames {
                match symbolicate_js_frame(&mut lookup, raw_frame, &mut missing_sourcescontent)
                    .await
                {
                    Ok(frame) => {
                        symbolicated_frames.push(frame);
                    }
                    Err(status) => {
                        unsymbolicated_frames += 1;
                        symbolicated_frames.push(SymbolicatedJsFrame {
                            status,
                            raw: raw_frame.clone(),
                        });
                    }
                }
            }

            stacktraces.push(SymbolicatedJsStacktrace {
                frames: symbolicated_frames,
            });
        }

        lookup.record_metrics();
        metric!(time_raw("js.unsymbolicated_frames") = unsymbolicated_frames);
        metric!(time_raw("js.missing_sourcescontent") = missing_sourcescontent);

        Ok(CompletedJsSymbolicationResponse {
            stacktraces,
            raw_stacktraces,
        })
    }
}

async fn symbolicate_js_frame(
    lookup: &mut SourceMapLookup,
    raw_frame: &mut JsFrame,
    missing_sourcescontent: &mut u64,
) -> Result<SymbolicatedJsFrame, JsFrameStatus> {
    let module = lookup.get_module(&raw_frame.abs_path).await;

    if !module.is_valid() {
        return Err(JsFrameStatus::InvalidAbsPath);
    }

    // Apply source context to the raw frame
    apply_source_context_from_artifact(raw_frame, &module.minified_source);

    let smcache = match &module.smcache {
        Ok(smcache) => smcache,
        Err(CacheError::Malformed(_)) => return Err(JsFrameStatus::MalformedSourcemap),
        Err(_) => return Err(JsFrameStatus::MissingSourcemap),
    };

    let mut frame = SymbolicatedJsFrame {
        status: JsFrameStatus::Symbolicated,
        raw: raw_frame.clone(),
    };

    let (line, col) = match (raw_frame.lineno, raw_frame.colno) {
        (Some(line), Some(col)) if line > 0 && col > 0 => (line, col),
        _ => return Err(JsFrameStatus::InvalidSourceMapLocation),
    };
    let sp = SourcePosition::new(line - 1, col - 1);

    let token = smcache
        .get()
        .lookup(sp)
        .ok_or(JsFrameStatus::InvalidSourceMapLocation)?;

    let function_name = match token.scope() {
        ScopeLookupResult::NamedScope(name) => name.to_string(),
        ScopeLookupResult::AnonymousScope => "<anonymous>".to_string(),
        ScopeLookupResult::Unknown => {
            // Fallback to minified function name
            raw_frame
                .function
                .clone()
                .unwrap_or("<unknown>".to_string())
        }
    };

    frame.raw.function = Some(fold_function_name(&function_name));
    if let Some(filename) = token.file_name() {
        frame.raw.abs_path = filename.to_string();
    }
    frame.raw.lineno = Some(token.line().saturating_add(1));
    frame.raw.colno = Some(token.column().saturating_add(1));

    if let Some(file) = token.file() {
        frame.raw.filename = file.name().map(|f| f.to_string());
        if let Some(file_source) = file.source() {
            apply_source_context(&mut frame.raw, file_source);
        } else if module.has_debug_id() {
            *missing_sourcescontent += 1;
            tracing::error!("expected `SourceMap` with `DebugId` to have embedded sources");
        } else {
            *missing_sourcescontent += 1;
            // If we have no source context from within the `SourceMapCache`,
            // fall back to applying the source context from a raw artifact file
            let filename = frame.raw.filename.as_ref();
            let file_key = filename.and_then(|filename| module.source_file_key(filename));

            let source_file = match file_key {
                Some(key) => lookup.get_source_file(key).await,
                None => &Err(CacheError::NotFound),
            };

            apply_source_context_from_artifact(&mut frame.raw, source_file);
        }
    }

    Ok(frame)
}

fn apply_source_context_from_artifact(frame: &mut JsFrame, file: &CacheEntry<CachedFile>) {
    if let Ok(file) = file {
        apply_source_context(frame, &file.contents)
    } else {
        // TODO(sourcemap): report missing source?
    }
}

fn apply_source_context(frame: &mut JsFrame, source: &str) {
    let Some(lineno) = frame.lineno else { return; };
    let lineno = lineno as usize;
    let column = frame.colno.map(|col| col as usize);

    if let Some((pre_context, context_line, post_context)) =
        get_context_lines(source, lineno, column, None)
    {
        frame.pre_context = pre_context;
        frame.context_line = Some(context_line);
        frame.post_context = post_context;
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
