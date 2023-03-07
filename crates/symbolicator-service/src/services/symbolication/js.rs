use std::sync::Arc;

use symbolic::sourcemapcache::{ScopeLookupResult, SourcePosition};
use symbolicator_sources::SentrySourceConfig;

use crate::caching::{CacheEntry, CacheError};
use crate::services::sourcemap_lookup::{CachedFile, OwnedSourceMapCache};
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

// TODO(sourcemap): Use our generic caching solution for all Artifacts.
// TODO(sourcemap): Rename all `JsProcessing_` and `js_processing_` prefixed names to something we agree on.
impl SymbolicationActor {
    #[tracing::instrument(skip_all)]
    pub async fn symbolicate_js(
        &self,
        request: SymbolicateJsStacktraces,
    ) -> Result<CompletedJsSymbolicationResponse, anyhow::Error> {
        let mut lookup = self.sourcemaps.create_sourcemap_lookup(
            request.scope.clone(),
            request.source.clone(),
            &request.modules,
            request.allow_scraping,
        );
        lookup.prefetch_artifacts(&request.stacktraces).await;

        let mut raw_stacktraces = request.stacktraces;

        let num_stacktraces = raw_stacktraces.len();
        let mut stacktraces = Vec::with_capacity(num_stacktraces);

        for raw_stacktrace in &mut raw_stacktraces {
            let num_frames = raw_stacktrace.frames.len();
            let mut symbolicated_frames = Vec::with_capacity(num_frames);

            for raw_frame in &mut raw_stacktrace.frames {
                let cached_module = lookup.get_module(&raw_frame.abs_path).await;

                if !cached_module.is_valid() {
                    symbolicated_frames.push(SymbolicatedJsFrame {
                        status: JsFrameStatus::InvalidAbsPath,
                        raw: raw_frame.clone(),
                    });
                    continue;
                }

                // Apply source context to the raw frame
                apply_source_context_from_artifact(raw_frame, &cached_module.minified_source);

                // And symbolicate
                match symbolicate_js_frame(raw_frame, &cached_module.smcache) {
                    Ok((mut frame, did_apply_source)) => {
                        // If we have no source context from within the `SourceMapCache`,
                        // fall back to applying the source context from a raw artifact file
                        // TODO: we should only do this fallback if there is *no* `DebugId`.
                        if !did_apply_source {
                            let filename = frame.raw.filename.as_ref();
                            let file_key = filename
                                .and_then(|filename| cached_module.source_file_key(filename));

                            let source_file = match file_key {
                                Some(key) => lookup.get_source_file(key).await,
                                None => &Err(CacheError::NotFound),
                            };

                            apply_source_context_from_artifact(&mut frame.raw, source_file);
                        }
                        symbolicated_frames.push(frame)
                    }
                    Err(status) => {
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

        Ok(CompletedJsSymbolicationResponse {
            stacktraces,
            raw_stacktraces,
        })
    }
}

fn symbolicate_js_frame(
    frame: &JsFrame,
    smcache: &CacheEntry<OwnedSourceMapCache>,
) -> Result<(SymbolicatedJsFrame, bool), JsFrameStatus> {
    let smcache = match smcache {
        Ok(smcache) => smcache,
        Err(CacheError::Malformed(_)) => return Err(JsFrameStatus::MalformedSourcemap),
        Err(_) => return Err(JsFrameStatus::MissingSourcemap),
    };

    let mut result = SymbolicatedJsFrame {
        status: JsFrameStatus::Symbolicated,
        raw: frame.clone(),
    };

    // TODO(sourcemap): Report invalid source location error
    let (line, col) = match (frame.lineno, frame.colno) {
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
            frame.function.clone().unwrap_or("<unknown>".to_string())
        }
    };

    result.raw.function = Some(fold_function_name(&function_name));
    if let Some(filename) = token.file_name() {
        result.raw.abs_path = filename.to_string();
    }
    result.raw.lineno = Some(token.line().saturating_add(1));
    result.raw.colno = Some(token.column().saturating_add(1));

    let mut did_apply_source = false;
    if let Some(file) = token.file() {
        result.raw.filename = file.name().map(|f| f.to_string());
        if let Some(file_source) = file.source() {
            apply_source_context(&mut result.raw, file_source);
            did_apply_source = true;
        } else {
            // TODO: report missing source?
        }
    }

    Ok((result, did_apply_source))
}

fn apply_source_context_from_artifact(frame: &mut JsFrame, file: &CacheEntry<CachedFile>) {
    if let Ok(file) = file {
        apply_source_context(frame, &file.contents)
    } else {
        // TODO: report missing source?
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
