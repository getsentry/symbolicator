use serde_json::Value;
use symbolicator_service::services::symbolication::SymbolicateJsStacktraces;
use symbolicator_service::types::RawObjectInfo;
use symbolicator_sources::ObjectType;

use crate::raw_event::Object;
use crate::{RawEvent, RawProcessor};

pub async fn process_js_event(processor: &RawProcessor, event: &mut RawEvent) -> Option<()> {
    let platform = event
        .pointer("platform")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let release = event
        .pointer("release")
        .and_then(Value::as_str)
        .map(Into::into);
    let dist = event
        .pointer("dist")
        .and_then(Value::as_str)
        .map(Into::into);
    let modules = js_modules(event);

    let mut stacktraces = vec![];
    event.for_each_stack_traces(|sinfo| {
        let frames = todo!();
        stacktraces.push(frames);
    });

    let res = processor
        .service
        .symbolicate_js(SymbolicateJsStacktraces {
            scope: todo!(),
            source: todo!(),
            release,
            dist,
            debug_id_index: todo!(),
            url_index: todo!(),
            stacktraces: todo!(),
            modules,
            scraping: todo!(),
            apply_source_context: true,
        })
        .await;

    Some(())
}

fn js_modules(event: &mut RawEvent) -> Vec<RawObjectInfo> {
    /*
    def is_sourcemap_image(image):
        return (
            bool(image)
            and image.get("type") == "sourcemap"
            and image.get("debug_id") is not None
            and image.get("code_file") is not None
        )


    def sourcemap_images_from_data(data):
        return get_path(data, "debug_meta", "images", default=(), filter=is_sourcemap_image)
    */
    let Some(images) = event.debug_images() else {
        return vec![];
    };
    images
        .filter_map(|image| {
            if image.get("type").and_then(Value::as_str) != Some("sourcemap") {
                return None;
            }
            let debug_id = image.get("debug_id").and_then(Value::as_str)?;
            let code_file = image.get("code_file").and_then(Value::as_str)?;
            Some(RawObjectInfo {
                ty: ObjectType::SourceMap,
                debug_id: Some(debug_id.into()),
                code_file: Some(code_file.into()),
                code_id: None,
                debug_file: None,
                debug_checksum: None,
                image_addr: Default::default(),
                image_size: None,
            })
        })
        .collect()
}

fn handles_frame<'f>(frame: &'f Object, platform: &str) -> Option<(&'f str, u64)> {
    // Skip frames without an `abs_path` or line number
    let abs_path = frame.get("abs_path").and_then(Value::as_str)?;
    let lineno = frame.get("lineno").and_then(Value::as_u64)?;

    // Skip "native" frames
    if is_native_frame(abs_path) {
        return None;
    }

    // Skip builtin node modules
    if is_built_in(abs_path, platform) {
        return None;
    }

    Some((abs_path, lineno))
}

fn is_native_frame(abs_path: &str) -> bool {
    matches!(abs_path, "native" | "[native code]")
}

fn is_built_in(abs_path: &str, platform: &str) -> bool {
    platform == "node"
        && !["/", "app:", "webpack:"]
            .iter()
            .any(|prefix| abs_path.starts_with(prefix))
}

/*
modules = sourcemap_images_from_data(data)

stacktrace_infos = find_stacktraces_in_data(data)
stacktraces = [
    {
        "frames": [
            _normalize_frame(frame)
            for frame in sinfo.stacktrace.get("frames") or ()
            if _handles_frame(frame, data)
        ],
    }
    for sinfo in stacktrace_infos
]

metrics.incr("sourcemaps.symbolicator.events")

if not any(stacktrace["frames"] for stacktrace in stacktraces):
    metrics.incr("sourcemaps.symbolicator.events.skipped")
    return

response = symbolicator.process_js(
    stacktraces=stacktraces,
    modules=modules,
    release=data.get("release"),
    dist=data.get("dist"),
)

if not _handle_response_status(data, response):
    return data

used_artifact_bundles = response.get("used_artifact_bundles", [])
if used_artifact_bundles:
    maybe_renew_artifact_bundles_from_processing(symbolicator.project.id, used_artifact_bundles)

processing_errors = response.get("errors", [])
if len(processing_errors) > 0:
    data.setdefault("errors", []).extend(map_symbolicator_process_js_errors(processing_errors))

assert len(stacktraces) == len(response["stacktraces"]), (stacktraces, response)

for sinfo, raw_stacktrace, complete_stacktrace in zip(
    stacktrace_infos, response["raw_stacktraces"], response["stacktraces"]
):
    processed_frame_idx = 0
    new_frames = []
    new_raw_frames = []
    for sinfo_frame in sinfo.stacktrace["frames"]:
        if not _handles_frame(sinfo_frame, data):
            new_raw_frames.append(sinfo_frame)
            new_frames.append(_normalize_nonhandled_frame(dict(sinfo_frame), data))
            continue

        raw_frame = raw_stacktrace["frames"][processed_frame_idx]
        complete_frame = complete_stacktrace["frames"][processed_frame_idx]
        processed_frame_idx += 1

        merged_context_frame = _merge_frame_context(sinfo_frame, raw_frame)
        new_raw_frames.append(merged_context_frame)

        merged_frame = _merge_frame(merged_context_frame, complete_frame)
        new_frames.append(merged_frame)

    sinfo.stacktrace["frames"] = new_frames

    if sinfo.container is not None:
        sinfo.container["raw_stacktrace"] = {
            "frames": new_raw_frames,
        }

return data
*/
