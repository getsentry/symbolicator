use symbolic::common::split_path;
use symbolic::ppdb::PortablePdbCache;

use crate::interface::{FrameStatus, RawFrame, SymbolicatedFrame};

pub fn symbolicate_dotnet_frame(
    ppdbcache: &PortablePdbCache,
    frame: &RawFrame,
    index: usize,
) -> Result<Vec<SymbolicatedFrame>, FrameStatus> {
    // TODO: Add a new error variant for this?
    let function_idx = frame.function_id.ok_or(FrameStatus::MissingSymbol)?.0 as u32;
    let il_offset = frame.instruction_addr.0 as u32;

    let line_info = ppdbcache
        .lookup(function_idx, il_offset)
        .ok_or(FrameStatus::MissingSymbol)?;

    let abs_path = line_info.file_name;
    let filename = split_path(abs_path).1;
    let result = SymbolicatedFrame {
        status: FrameStatus::Symbolicated,
        original_index: Some(index),
        raw: RawFrame {
            lang: Some(line_info.file_lang),
            filename: Some(filename.to_string()),
            abs_path: Some(abs_path.to_string()),
            lineno: Some(line_info.line),
            ..frame.clone()
        },
    };

    Ok(vec![result])
}
