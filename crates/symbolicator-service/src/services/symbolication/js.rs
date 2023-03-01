use std::io::BufRead;

use symbolic::sourcemapcache::{ScopeLookupResult, SourcePosition};

use crate::caching::{CacheEntry, CacheError};
use crate::services::sourcemap_lookup::{CachedFile, OwnedSourceMapCache};
use crate::types::{
    CompletedJsSymbolicationResponse, JsFrame, JsFrameStatus, SymbolicatedJsFrame,
    SymbolicatedJsStacktrace,
};

use super::{SymbolicateJsStacktraces, SymbolicationActor};

// TODO(sourcemap): Use our generic caching solution for all Artifacts.
// TODO(sourcemap): Rename all `JsProcessing_` and `js_processing_` prefixed names to something we agree on.
impl SymbolicationActor {
    #[tracing::instrument(skip_all)]
    pub async fn symbolicate_js(
        &self,
        request: SymbolicateJsStacktraces,
    ) -> Result<CompletedJsSymbolicationResponse, anyhow::Error> {
        let mut lookup = self
            .sourcemaps
            .create_sourcemap_lookup(request.source.clone(), &request.modules);
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
                                Some(key) => lookup.get_file(key).await,
                                None => Err(CacheError::NotFound),
                            };

                            apply_source_context_from_artifact(&mut frame.raw, &source_file);
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
            apply_source_context(&mut result.raw, file_source.as_bytes());
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

fn apply_source_context(frame: &mut JsFrame, source: &[u8]) {
    // At this stage we know we have _some_ line here, so it's safe to unwrap.
    let frame_line = frame.lineno.unwrap();
    let frame_column = frame.colno.unwrap_or_default();

    let current_line = frame_line.saturating_sub(1);
    let pre_line = current_line.saturating_sub(5);
    let post_line = current_line.saturating_add(5);

    let mut lines: Vec<_> = source.lines().map(|l| l.unwrap_or_default()).collect();

    // `BufRead::lines` doesn't include trailing new-lines, but we do want one if it was there.
    if source.ends_with("\n".as_bytes()) {
        lines.push("\n".to_string());
    }

    frame.context_line = lines
        .get(current_line as usize)
        .map(|line| trim_context_line(line, frame_column));

    frame.pre_context = (pre_line..current_line)
        .filter_map(|line_no| lines.get(line_no as usize))
        .map(|line| trim_context_line(line, frame_column))
        .collect();

    frame.post_context = (current_line + 1..=post_line)
        .filter_map(|line_no| lines.get(line_no as usize))
        .map(|line| trim_context_line(line, frame_column))
        .collect();
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

/// Trims a line down to a goal of 140 characters, with a little wiggle room to be sensible
/// and tries to trim around the given `column`. So it tries to extract 60 characters
/// before the provided `column` and fill the rest up to 140 characters to yield a better context.
fn trim_context_line(line: &str, column: u32) -> String {
    let mut line = line.trim_end_matches('\n').to_string();
    let len = line.len();

    if len <= 150 {
        return line;
    }

    let col: usize = match column.try_into() {
        Ok(c) => std::cmp::min(c, len),
        Err(_) => return line,
    };

    let mut start = col.saturating_sub(60);
    // Round down if it brings us close to the edge.
    if start < 5 {
        start = 0;
    }

    let mut end = std::cmp::min(start + 140, len);
    // Round up to the end if it's close.
    if end > len.saturating_sub(5) {
        end = len;
    }

    // If we are bumped all the way to the end, make sure we still get a full 140 chars in the line.
    if end == len {
        start = end.saturating_sub(140);
    }

    line = line[start..end].to_string();

    if end < len {
        // We've snipped from the end.
        line = format!("{line} {{snip}}");
    }

    if start > 0 {
        // We've snipped from the beginning.
        line = format!("{{snip}} {line}");
    }

    line
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

    #[test]
    fn test_trim_context_line() {
        let long_line = "The public is more familiar with bad design than good design. It is, in effect, conditioned to prefer bad design, because that is what it lives with. The new becomes threatening, the old reassuring.";
        assert_eq!(trim_context_line("foo", 0), "foo".to_string());
        assert_eq!(trim_context_line(long_line, 0), "The public is more familiar with bad design than good design. It is, in effect, conditioned to prefer bad design, because that is what it li {snip}".to_string());
        assert_eq!(trim_context_line(long_line, 10), "The public is more familiar with bad design than good design. It is, in effect, conditioned to prefer bad design, because that is what it li {snip}".to_string());
        assert_eq!(trim_context_line(long_line, 66), "{snip} blic is more familiar with bad design than good design. It is, in effect, conditioned to prefer bad design, because that is what it lives wi {snip}".to_string());
        assert_eq!(trim_context_line(long_line, 190), "{snip} gn. It is, in effect, conditioned to prefer bad design, because that is what it lives with. The new becomes threatening, the old reassuring.".to_string());
        assert_eq!(trim_context_line(long_line, 9999), "{snip} gn. It is, in effect, conditioned to prefer bad design, because that is what it lives with. The new becomes threatening, the old reassuring.".to_string());
    }
}
