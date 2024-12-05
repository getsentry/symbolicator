use std::collections::BTreeSet;

use symbolic::sourcemapcache::{ScopeLookupResult, SourcePosition};
use symbolicator_service::caching::CacheError;
use symbolicator_service::source_context::get_context_lines;

use crate::interface::{
    CompletedJsSymbolicationResponse, JsFrame, JsModuleError, JsModuleErrorKind, JsStacktrace,
    SymbolicateJsStacktraces,
};
use crate::lookup::SourceMapLookup;
use crate::metrics::{record_stacktrace_metrics, SymbolicationStats};
use crate::utils::{
    fixup_webpack_filename, fold_function_name, generate_module, get_function_for_token, is_in_app,
    join_paths,
};
use crate::SourceMapService;

impl SourceMapService {
    #[tracing::instrument(skip_all)]
    pub async fn symbolicate_js(
        &self,
        mut request: SymbolicateJsStacktraces,
    ) -> CompletedJsSymbolicationResponse {
        let mut raw_stacktraces = std::mem::take(&mut request.stacktraces);
        let apply_source_context = request.apply_source_context;
        let platform = request.platform.clone();
        let mut lookup = SourceMapLookup::new(self.clone(), request).await;
        lookup.prepare_modules(&mut raw_stacktraces[..]);

        let mut stats = SymbolicationStats::default();

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
                    apply_source_context,
                    &mut stats,
                )
                .await
                {
                    Ok(mut frame) => {
                        *stats
                            .symbolicated_frames
                            .entry(raw_frame.platform.clone())
                            .or_default() += 1;
                        std::mem::swap(&mut callsite_fn_name, &mut frame.token_name);
                        symbolicated_frames.push(frame);
                    }
                    Err(err) => {
                        *stats
                            .unsymbolicated_frames
                            .entry(raw_frame.platform.clone())
                            .or_default() += 1;
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

        stats.num_stacktraces = stacktraces.len() as u64;

        lookup.record_metrics();
        record_stacktrace_metrics(platform, stats);

        let (used_artifact_bundles, scraping_attempts) = lookup.into_records();

        CompletedJsSymbolicationResponse {
            stacktraces,
            raw_stacktraces,
            errors: errors.into_iter().collect(),
            used_artifact_bundles,
            scraping_attempts,
        }
    }
}

async fn symbolicate_js_frame(
    lookup: &mut SourceMapLookup,
    raw_frame: &mut JsFrame,
    errors: &mut BTreeSet<JsModuleError>,
    callsite_fn_name: Option<String>,
    should_apply_source_context: bool,
    stats: &mut SymbolicationStats,
) -> Result<JsFrame, JsModuleErrorKind> {
    // we check for a valid line (i.e. >= 1) first, as we want to avoid resolving / scraping the minified
    // file in that case. we frequently saw 0 line/col values in combination with non-js files,
    // and we want to avoid scraping a bunch of html files in that case.
    let line = if raw_frame.lineno > 0 {
        raw_frame.lineno
    } else {
        return Err(JsModuleErrorKind::InvalidLocation {
            line: raw_frame.lineno,
            col: raw_frame.colno,
        });
    };

    let col = raw_frame.colno.unwrap_or_default();

    let module = lookup.get_module(&raw_frame.abs_path).await;

    tracing::trace!(
        abs_path = &raw_frame.abs_path,
        ?module,
        "Module for `abs_path`"
    );

    // Apply source context to the raw frame. If it fails, we bail early, as it's not possible
    // to construct a `SourceMapCache` without the minified source anyway.
    let mut frame = match &module.minified_source.entry {
        Ok(minified_source) => {
            // Clone the frame before applying source context to the raw frame.
            // If we apply source context first, this can lead to the following confusing situation:
            // 1. Apply source context to the minified frame
            // 2. Clone it for the unminified frame
            // 3. We don't have unminified source
            // 4. The unminified frame contains the minified source context
            let frame = raw_frame.clone();
            if should_apply_source_context {
                apply_source_context(raw_frame, minified_source.contents())?
            }

            frame
        }
        Err(CacheError::DownloadError(msg)) if msg == "Scraping disabled" => {
            return Err(JsModuleErrorKind::ScrapingDisabled);
        }
        Err(_) => return Err(JsModuleErrorKind::MissingSource),
    };

    let sourcemap_label = &module
        .minified_source
        .entry
        .as_ref()
        .map(|entry| entry.sourcemap_url())
        .ok()
        .flatten()
        .unwrap_or_else(|| raw_frame.abs_path.clone());

    let (smcache, resolved_with, sourcemap_origin) = match &module.smcache {
        Some(smcache) => match &smcache.entry {
            Ok(entry) => (entry, smcache.resolved_with, smcache.uri.clone()),
            Err(CacheError::Malformed(_)) => {
                // If we successfully resolved the sourcemap but it's broken somehow,
                // We should still record that we resolved it.
                raw_frame.data.resolved_with = Some(smcache.resolved_with);
                raw_frame.data.sourcemap_origin = Some(smcache.uri.clone());
                return Err(JsModuleErrorKind::MalformedSourcemap {
                    url: sourcemap_label.to_owned(),
                });
            }
            Err(CacheError::DownloadError(msg)) if msg == "Scraping disabled" => {
                return Err(JsModuleErrorKind::ScrapingDisabled);
            }
            Err(_) => return Err(JsModuleErrorKind::MissingSourcemap),
        },
        // In case it's just a source file, with no sourcemap reference or any debug id, we bail.
        None => return Ok(raw_frame.clone()),
    };

    frame.data.sourcemap = Some(sourcemap_label.clone());
    frame.data.sourcemap_origin = Some(sourcemap_origin);
    frame.data.resolved_with = Some(resolved_with);

    let sp = SourcePosition::new(line - 1, col.saturating_sub(1));
    let token = smcache
        .get()
        .lookup(sp)
        .ok_or(JsModuleErrorKind::InvalidLocation {
            line,
            col: Some(col),
        })?;

    // We consider the frame successfully symbolicated if we can resolve the minified source position
    // to a token.
    frame.data.symbolicated = true;

    // Store the resolved token name, which can be used for function name resolution in next frame.
    // Refer to https://blog.sentry.io/2022/11/30/how-we-made-javascript-stack-traces-awesome/
    // for more details about "caller naming".
    frame.token_name = token.name().map(|n| n.to_owned());

    let function_name = match token.scope() {
        ScopeLookupResult::NamedScope(name) => {
            let scope_name = name.to_string();
            // Special case for Dart async function rewrites
            // https://github.com/dart-lang/sdk/blob/fab753ea277c96c7699920852dabf977a7065fa5/pkg/compiler/lib/src/js_backend/namer.dart#L1845-L1866
            // ref: https://github.com/getsentry/symbolic/issues/791
            if name.starts_with("$async$") {
                token.name().map_or_else(|| scope_name, |n| n.to_owned())
            } else {
                scope_name
            }
        }
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
            .map(|base| join_paths(base, &filename))
            .unwrap_or_else(|| filename.clone());

        if filename.starts_with("webpack:") {
            filename = fixup_webpack_filename(&filename);
            frame.module = Some(generate_module(&filename));
        }

        frame.in_app = is_in_app(&frame.abs_path, &filename);

        if frame.module.is_none()
            && (frame.abs_path.starts_with("http:")
                || frame.abs_path.starts_with("https:")
                || frame.abs_path.starts_with("webpack:")
                || frame.abs_path.starts_with("app:"))
        {
            frame.module = Some(generate_module(&frame.abs_path));
        }

        frame.filename = Some(filename);
    }

    frame.lineno = token.line().saturating_add(1);
    frame.colno = Some(token.column().saturating_add(1));

    if !should_apply_source_context {
        return Ok(frame);
    }

    if let Some(file) = token.file() {
        if let Some(file_source) = file.source() {
            if let Err(err) = apply_source_context(&mut frame, file_source) {
                errors.insert(JsModuleError {
                    abs_path: raw_frame.abs_path.clone(),
                    kind: err,
                });
            }
        } else {
            stats.missing_sourcescontent += 1;

            // If we have no source context from within the `SourceMapCache`,
            // fall back to applying the source context from a raw artifact file
            let file_key = file
                .name()
                .and_then(|filename| module.source_file_key(filename));

            let source_file = match &file_key {
                Some(key) => &lookup.get_source_file(key.clone()).await.entry,
                None => &Err(CacheError::NotFound),
            };

            if source_file
                .as_ref()
                .map_err(|_| JsModuleErrorKind::MissingSource)
                .and_then(|file| apply_source_context(&mut frame, file.contents()))
                .is_err()
            {
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

fn apply_source_context(frame: &mut JsFrame, source: &str) -> Result<(), JsModuleErrorKind> {
    let lineno = frame.lineno as usize;
    let column = frame.colno.map(|col| col as usize).unwrap_or_default();

    if let Some((pre_context, context_line, post_context)) =
        get_context_lines(source, lineno, column, None)
    {
        frame.pre_context = pre_context;
        frame.context_line = Some(context_line);
        frame.post_context = post_context;
    }

    Ok(())
}
