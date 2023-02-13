use std::collections::HashMap;

use symbolic::sourcemapcache::{ScopeLookupResult, SourcePosition};

use crate::caching::{CacheEntry, CacheError};
use crate::services::sourcemap::OwnedSourceMapCache;
use crate::types::{
    CompleteJsStacktrace, JsProcessingCompletedSymbolicationResponse, JsProcessingFrameStatus,
    JsProcessingRawFrame, JsProcessingRawStacktrace, JsProcessingSymbolicatedFrame,
};

use super::{JsProcessingSymbolicateStacktraces, SymbolicationActor};

// TODO(sourcemap): Use our generic caching solution for all Artifacts.
// TODO(sourcemap): Rename all `JsProcessing_` and `js_processing_` prefixed names to something we agree on.
impl SymbolicationActor {
    #[tracing::instrument(skip_all)]
    pub async fn js_processing_symbolicate(
        &self,
        request: JsProcessingSymbolicateStacktraces,
    ) -> Result<JsProcessingCompletedSymbolicationResponse, anyhow::Error> {
        let artifacts = self.sourcemaps.collect_stacktrace_artifacts(&request).await;

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
    artifacts: &HashMap<String, CacheEntry<OwnedSourceMapCache>>,
) -> CompleteJsStacktrace {
    let mut symbolicated_frames = vec![];
    let unsymbolicated_frames_iter = stacktrace.frames.into_iter();

    for mut frame in unsymbolicated_frames_iter {
        match js_processing_symbolicate_frame(&mut frame, artifacts) {
            Ok(frame) => symbolicated_frames.push(frame),
            Err(status) => {
                symbolicated_frames.push(JsProcessingSymbolicatedFrame { status, raw: frame });
            }
        }
    }

    CompleteJsStacktrace {
        frames: symbolicated_frames,
    }
}

fn js_processing_symbolicate_frame(
    frame: &mut JsProcessingRawFrame,
    artifacts: &HashMap<String, CacheEntry<OwnedSourceMapCache>>,
) -> Result<JsProcessingSymbolicatedFrame, JsProcessingFrameStatus> {
    let smcache = artifacts
        .get(&frame.abs_path)
        .ok_or(JsProcessingFrameStatus::MissingSourcemap)?;
    // TODO(sourcemap): Report invalid source location error
    let (line, col) = match (frame.lineno, frame.colno) {
        (Some(line), Some(col)) if line > 0 && col > 0 => (line, col),
        _ => return Err(JsProcessingFrameStatus::InvalidSourceMapLocation),
    };
    let sp = SourcePosition::new(line - 1, col - 1);
    let smcache = match smcache {
        Ok(smcache) => smcache,
        Err(CacheError::Malformed(_)) => return Err(JsProcessingFrameStatus::MalformedSourcemap),
        Err(_) => return Err(JsProcessingFrameStatus::MissingSourcemap),
    };

    let token = smcache
        .get()
        .lookup(sp)
        .ok_or(JsProcessingFrameStatus::InvalidSourceMapLocation)?;

    let function_name = match token.scope() {
        ScopeLookupResult::NamedScope(name) => name.to_string(),
        ScopeLookupResult::AnonymousScope => "<anonymous>".to_string(),
        ScopeLookupResult::Unknown => {
            // Fallback to minified function name
            frame.function.clone().unwrap_or("<unknown>".to_string())
        }
    };

    let mut result = JsProcessingSymbolicatedFrame {
        status: JsProcessingFrameStatus::Symbolicated,
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

        result.raw.context_line = token
            .line_contents()
            .map(|line| line.trim_end().to_string());

        let pre_line = current_line.saturating_sub(5);
        result.raw.pre_context = (pre_line..current_line)
            .filter_map(|line| file.line(line as usize))
            .map(|v| v.trim_end().to_string())
            .collect();

        let post_line = current_line.saturating_add(5);
        result.raw.post_context = (current_line + 1..=post_line)
            .filter_map(|line| file.line(line as usize))
            .map(|v| v.trim_end().to_string())
            .collect();
    }

    Ok(result)
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
