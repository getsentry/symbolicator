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

use std::collections::BTreeSet;
use std::sync::Arc;

use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::Url;
use symbolic::sourcemapcache::{ScopeLookupResult, SourcePosition};
use symbolicator_sources::SentrySourceConfig;

use crate::caching::{CacheEntry, CacheError};
use crate::services::sourcemap_lookup::{CachedFile, SourceMapLookup};
use crate::types::{
    CompletedJsSymbolicationResponse, JsFrame, JsModuleError, JsModuleErrorKind, JsStacktrace,
    RawObjectInfo, Scope,
};

use super::source_context::get_context_lines;
use super::SymbolicationActor;

static WEBPACK_NAMESPACE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"^webpack://[a-zA-Z0-9_\-@\.]+/\./"#).unwrap());

#[derive(Debug, Clone)]
pub struct SymbolicateJsStacktraces {
    pub scope: Scope,
    pub source: Arc<SentrySourceConfig>,
    pub release: Option<String>,
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
    ) -> anyhow::Result<CompletedJsSymbolicationResponse> {
        let mut raw_stacktraces = std::mem::take(&mut request.stacktraces);
        let mut lookup = SourceMapLookup::new(self.sourcemaps.clone(), request);
        lookup.prepare_modules(&raw_stacktraces);

        let mut unsymbolicated_frames = 0;
        let mut missing_sourcescontent = 0;

        let num_stacktraces = raw_stacktraces.len();
        let mut stacktraces = Vec::with_capacity(num_stacktraces);

        let mut errors = BTreeSet::new();
        for raw_stacktrace in &mut raw_stacktraces {
            let num_frames = raw_stacktrace.frames.len();
            let mut symbolicated_frames = Vec::with_capacity(num_frames);
            let mut callsite_fn_name = None;

            for raw_frame in &mut raw_stacktrace.frames {
                match symbolicate_js_frame(
                    &mut lookup,
                    raw_frame,
                    &mut errors,
                    std::mem::take(&mut callsite_fn_name),
                    &mut missing_sourcescontent,
                )
                .await
                {
                    Ok(mut frame) => {
                        std::mem::swap(&mut callsite_fn_name, &mut frame.token_name);
                        symbolicated_frames.push(frame);
                    }
                    Err(err) => {
                        unsymbolicated_frames += 1;
                        errors.insert(JsModuleError {
                            abs_path: raw_frame.abs_path.clone(),
                            kind: err,
                        });
                        symbolicated_frames.push(raw_frame.clone());
                    }
                }
            }

            stacktraces.push(JsStacktrace {
                frames: symbolicated_frames,
            });
        }

        lookup.record_metrics();
        metric!(time_raw("js.unsymbolicated_frames") = unsymbolicated_frames);
        metric!(time_raw("js.missing_sourcescontent") = missing_sourcescontent);

        Ok(CompletedJsSymbolicationResponse {
            stacktraces,
            raw_stacktraces,
            errors: errors.into_iter().collect(),
        })
    }
}

async fn symbolicate_js_frame(
    lookup: &mut SourceMapLookup,
    raw_frame: &mut JsFrame,
    errors: &mut BTreeSet<JsModuleError>,
    callsite_fn_name: Option<String>,
    missing_sourcescontent: &mut u64,
) -> Result<JsFrame, JsModuleErrorKind> {
    let module = lookup.get_module(&raw_frame.abs_path).await;

    if !module.is_valid() {
        return Err(JsModuleErrorKind::InvalidAbsPath);
    }

    tracing::trace!(
        abs_path = &raw_frame.abs_path,
        ?module,
        "Found Module for `abs_path`"
    );

    // Apply source context to the raw frame. If it fails, we bail early, as it's not possible
    // to construct a `SourceMapCache` without the minified source anyway.
    apply_source_context_from_artifact(raw_frame, &module.minified_source.entry)?;

    let sourcemap_label = &module
        .minified_source
        .entry
        .as_ref()
        .map(|entry| entry.sourcemap_url())
        .ok()
        .flatten()
        .unwrap_or_else(|| raw_frame.abs_path.clone());

    let smcache = match &module.smcache {
        Some(smcache) => match &smcache.entry {
            Ok(entry) => entry,
            Err(CacheError::Malformed(_)) => {
                return Err(JsModuleErrorKind::MalformedSourcemap {
                    url: sourcemap_label.to_owned(),
                })
            }
            Err(_) => return Err(JsModuleErrorKind::MissingSourcemap),
        },
        // In case it's just a source file, with no sourcemap reference or any debug id, we bail.
        None => return Ok(raw_frame.clone()),
    };

    let mut frame = raw_frame.clone();
    frame.data.sourcemap = Some(sourcemap_label.clone());

    let (line, col) = match (raw_frame.lineno, raw_frame.colno) {
        (Some(line), Some(col)) if line > 0 && col > 0 => (line, col),
        _ => {
            return Err(JsModuleErrorKind::InvalidLocation {
                line: raw_frame.lineno,
                col: raw_frame.colno,
            })
        }
    };
    let sp = SourcePosition::new(line - 1, col - 1);

    let token = smcache
        .get()
        .lookup(sp)
        .ok_or(JsModuleErrorKind::InvalidLocation {
            line: Some(line),
            col: Some(col),
        })?;

    // Store the resolved token name, which can be used for function name resolution in next frame.
    // Refer to https://blog.sentry.io/2022/11/30/how-we-made-javascript-stack-traces-awesome/
    // for more details about "caller naming".
    frame.token_name = token.name().map(|n| n.to_owned());

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

    frame.function = Some(fold_function_name(get_function_for_token(
        raw_frame.function.as_deref(),
        &function_name,
        callsite_fn_name.as_deref(),
    )));

    if let Some(filename) = token.file_name() {
        let mut filename = filename.to_string();
        frame.abs_path = module
            .source_file_base()
            .and_then(|base| nonstandard_path_join(base, &filename))
            .unwrap_or_else(|| filename.clone());

        if filename.starts_with("webpack:") {
            filename = fixup_webpack_filename(&filename);
        }

        frame.filename = Some(filename);
    }

    frame.lineno = Some(token.line().saturating_add(1));
    frame.colno = Some(token.column().saturating_add(1));

    if let Some(file) = token.file() {
        if let Some(file_source) = file.source() {
            if let Err(err) = apply_source_context(&mut frame, file_source) {
                errors.insert(JsModuleError {
                    abs_path: raw_frame.abs_path.clone(),
                    kind: err,
                });
            }
        } else {
            *missing_sourcescontent += 1;

            // If we have no source context from within the `SourceMapCache`,
            // fall back to applying the source context from a raw artifact file
            let file_key = file
                .name()
                .and_then(|filename| module.source_file_key(filename));

            let source_file = match &file_key {
                Some(key) => &lookup.get_source_file(key.clone()).await.entry,
                None => &Err(CacheError::NotFound),
            };

            if apply_source_context_from_artifact(&mut frame, source_file).is_err() {
                // It's arguable whether we should collect it, but this is what monolith does now,
                // and it might be useful to indicate incorrect sentry-cli rewrite behavior.
                errors.insert(JsModuleError {
                    abs_path: raw_frame.abs_path.clone(),
                    kind: JsModuleErrorKind::MissingSourceContent {
                        source: file_key
                            .and_then(|key| key.abs_path().map(|path| path.to_string()))
                            .unwrap_or_default(),
                        sourcemap: sourcemap_label.clone(),
                    },
                });
            }
        }
    }

    Ok(frame)
}

fn apply_source_context_from_artifact(
    frame: &mut JsFrame,
    file: &CacheEntry<CachedFile>,
) -> Result<(), JsModuleErrorKind> {
    if let Ok(file) = file {
        apply_source_context(frame, &file.contents)
    } else {
        Err(JsModuleErrorKind::MissingSource)
    }
}

fn apply_source_context(frame: &mut JsFrame, source: &str) -> Result<(), JsModuleErrorKind> {
    let Some(lineno) = frame.lineno else {
        return Err(JsModuleErrorKind::InvalidLocation {
            line: frame.lineno,
            col: frame.colno,
        })
    };
    let lineno = lineno as usize;
    let column = frame.colno.map(|col| col as usize);

    if let Some((pre_context, context_line, post_context)) =
        get_context_lines(source, lineno, column, None)
    {
        frame.pre_context = pre_context;
        frame.context_line = Some(context_line);
        frame.post_context = post_context;
    }

    Ok(())
}

// Names that do not provide any reasonable value, and that can possibly obstruct
// better available names. In case we encounter one, we fallback to current frame fn name if available.
const USELESS_FN_NAMES: [&str; 3] = ["<anonymous>", "__webpack_require__", "__webpack_modules__"];

/// Get function name for a given frame based on the token resolved by symbolic.
/// It tries following paths in order:
/// - return token function name if we have a usable value (filtered through `USELESS_FN_NAMES` list),
/// - return mapped name of the caller (previous frame) token if it had,
/// - return token function name, including filtered values if it mapped to anything in the first place,
/// - return current frames function name as a fallback
///
// fn get_function_for_token(frame, token, previous_frame=None):
fn get_function_for_token<'a>(
    frame_fn_name: Option<&'a str>,
    token_fn_name: &'a str,
    callsite_fn_name: Option<&'a str>,
) -> &'a str {
    // Try to use the function name we got from sourcemap-cache, filtering useless names.
    if !USELESS_FN_NAMES.contains(&token_fn_name) {
        return token_fn_name;
    }

    // If not found, ask the callsite (previous token) for function name if possible.
    if let Some(token_name) = callsite_fn_name {
        if !token_name.is_empty() {
            return token_name;
        }
    }

    // If there was no minified name at all, return even useless, filtered one from the original token.
    if frame_fn_name.is_none() {
        return token_fn_name;
    }

    // Otherwise fallback to the old, minified name.
    frame_fn_name.unwrap_or("<unknown>")
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

fn fixup_webpack_filename(filename: &str) -> String {
    if let Some((_, rest)) = filename.split_once("/~/") {
        format!("~/{rest}")
    } else if WEBPACK_NAMESPACE_RE.is_match(filename) {
        WEBPACK_NAMESPACE_RE.replace(filename, "./").to_string()
    } else if let Some(rest) = filename.strip_prefix("webpack:///") {
        rest.to_string()
    } else {
        filename.to_string()
    }
}

/// Joins a path to a base URL without normalizing `..` segments.
fn nonstandard_path_join(base: &Url, path: &str) -> Option<String> {
    let path = path.replace("./", "__dotslash__");
    let result = base.join(&path).ok()?.to_string();
    Some(result.replace("__dotslash__", "./"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_function_name_valid_name() {
        assert_eq!(
            get_function_for_token(Some("original"), "lookedup", None),
            "lookedup"
        );
    }
    #[test]
    fn test_get_function_name_fallback_to_previous_frames_token_if_useless_name() {
        assert_eq!(
            get_function_for_token(None, "__webpack_require__", Some("previous_name")),
            "previous_name"
        )
    }
    #[test]
    fn test_get_function_name_fallback_to_useless_name() {
        assert_eq!(
            get_function_for_token(None, "__webpack_require__", None),
            "__webpack_require__"
        )
    }
    #[test]
    fn test_get_function_name_fallback_to_original_name() {
        assert_eq!(
            get_function_for_token(Some("original"), "__webpack_require__", None),
            "original"
        )
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

    #[test]
    fn test_fixup_webpack_filename() {
        let filename = "webpack:///../node_modules/@sentry/browser/esm/helpers.js";

        assert_eq!(
            fixup_webpack_filename(filename),
            "../node_modules/@sentry/browser/esm/helpers.js"
        );

        let filename = "webpack:///./app/utils/requestError/createRequestError.tsx";

        assert_eq!(
            fixup_webpack_filename(filename),
            "./app/utils/requestError/createRequestError.tsx"
        );
    }

    #[test]
    fn test_join_paths() {
        let base = "http://example.com".parse().unwrap();
        let path = "webpack:///../node_modules/scheduler/cjs/scheduler.production.min.js";
        assert_eq!(nonstandard_path_join(&base, path).unwrap(), path);

        let path = "path/./to/file.min.js";
        assert_eq!(
            nonstandard_path_join(&base, path).unwrap(),
            "http://example.com/path/./to/file.min.js"
        );
    }
}
