use sentry::types::{CodeId, DebugId};
use serde::{Deserialize, Serialize};
use similar::{udiff::unified_diff, Algorithm};

use crate::types::{FrameTrust, RawObjectInfo};

use super::StackWalkMinidumpResult;

#[derive(Debug, Serialize, Deserialize)]
pub(super) enum NewStackwalkingProblem {
    StacktraceDiff { diff: String, scan: bool },
    ModuleDiff { diff: String },
    Slow,
}

fn fixup_modules(modules: &mut [(DebugId, RawObjectInfo)]) {
    modules.sort_by_key(|module| (module.0, module.1.image_addr));

    for (_, info) in modules.iter_mut() {
        if let Some(ref mut code_id) = info.code_id {
            if code_id.is_empty() {
                info.code_id = None;
            }
        }

        if let Some(ref mut code_file) = info.code_file {
            if code_file.is_empty() {
                info.code_file = None;
            }
        }

        if let Some(ref mut debug_id) = info.debug_id {
            if debug_id.is_empty() {
                info.debug_id = None;
            }
        }

        if let Some(ref mut debug_file) = info.debug_file {
            if debug_file.is_empty() {
                info.debug_file = None;
            }
        }
    }
}

fn raw_object_info_equal(this: &RawObjectInfo, that: &RawObjectInfo) -> bool {
    this.ty == that.ty
        && this.code_id.as_ref().map(|id| id.parse::<CodeId>())
            == that.code_id.as_ref().map(|id| id.parse())
        && this.code_file == that.code_file
        && this.debug_id.as_ref().map(|id| id.parse::<DebugId>())
            == that.debug_id.as_ref().map(|id| id.parse())
        && this.debug_file == that.debug_file
        && this.image_addr == that.image_addr
        && this.image_size == that.image_size
}

/// Determines whether there was a problem with the rust-minidump based stackwalking method.
///
/// A problem is either
/// 1. a difference in the returned modules or stacktraces, modulo reordering and trivial renaming of registers, or
/// 2. rust-minidump taking more than 50% longer than breakpad.
pub(super) fn find_stackwalking_problem(
    result_breakpad: &StackWalkMinidumpResult,
    result_rust_minidump: &StackWalkMinidumpResult,
) -> Option<NewStackwalkingProblem> {
    metric!(timer("minidump.stackwalk.duration") = result_rust_minidump.duration, "method" => "rust-minidump");

    // Normalize the name of the `eflags` register (returned by breakpad) to `efl` (returned by rust-minidump).
    // Not doing this leads to tons of spurious diffs.
    let mut stacktraces_breakpad = result_breakpad.stacktraces.clone();
    let scan = stacktraces_breakpad
        .iter()
        .flat_map(|st| st.frames.iter())
        .any(|f| f.trust == FrameTrust::Scan);
    for stacktrace in stacktraces_breakpad.iter_mut() {
        if let Some(val) = stacktrace.registers.remove("eflags") {
            stacktrace.registers.insert("efl".to_string(), val);
        }
    }

    if result_rust_minidump.stacktraces != stacktraces_breakpad {
        let diff = serde_json::to_string_pretty(&result_breakpad.stacktraces)
            .map_err(|e| {
                let stderr: &dyn std::error::Error = &e;
                tracing::error!(stderr, "Failed to convert breakpad stacktraces to json")
            })
            .ok()
            .and_then(|breakpad| {
                serde_json::to_string_pretty(&result_rust_minidump.stacktraces)
                    .map_err(|e| {
                        let stderr: &dyn std::error::Error = &e;
                        tracing::error!(
                            stderr,
                            "Failed to convert rust-minidump stacktraces to json",
                        )
                    })
                    .ok()
                    .map(|rust_minidump| {
                        unified_diff(
                            Algorithm::Myers,
                            &breakpad,
                            &rust_minidump,
                            3,
                            Some(("breakpad", "rust-minidump")),
                        )
                    })
            })
            .unwrap_or_else(|| String::from("diff unrecoverable"));
        return Some(NewStackwalkingProblem::StacktraceDiff { diff, scan });
    }

    let mut modules_breakpad = result_breakpad.modules.clone().unwrap_or_default();
    fixup_modules(&mut modules_breakpad);
    let mut modules_rust_minidump = result_rust_minidump.modules.clone().unwrap_or_default();
    modules_rust_minidump.sort_by_key(|module| (module.0, module.1.image_addr));

    if modules_rust_minidump.len() != modules_breakpad.len()
        || !modules_rust_minidump
            .iter()
            .zip(modules_breakpad.iter())
            .all(|(md, bp)| md.0 == bp.0 && raw_object_info_equal(&md.1, &bp.1))
    {
        let diff = serde_json::to_string_pretty(&modules_breakpad)
            .map_err(|e| {
                let stderr: &dyn std::error::Error = &e;
                tracing::error!(stderr, "Failed to convert breakpad modules to json")
            })
            .ok()
            .and_then(|breakpad| {
                serde_json::to_string_pretty(&modules_rust_minidump)
                    .map_err(|e| {
                        let stderr: &dyn std::error::Error = &e;
                        tracing::error!(stderr, "Failed to convert rust-minidump modules to json",)
                    })
                    .ok()
                    .map(|rust_minidump| {
                        unified_diff(
                            Algorithm::Myers,
                            &breakpad,
                            &rust_minidump,
                            3,
                            Some(("breakpad", "rust-minidump")),
                        )
                    })
            })
            .unwrap_or_else(|| String::from("diff unrecoverable"));
        return Some(NewStackwalkingProblem::ModuleDiff { diff });
    }

    if 2 * result_rust_minidump.duration >= 3 * result_breakpad.duration {
        Some(NewStackwalkingProblem::Slow)
    } else {
        None
    }
}
