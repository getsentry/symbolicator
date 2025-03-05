use std::{collections::HashMap, iter::Peekable, vec::IntoIter};

use prettytable::{cell, format::consts::FORMAT_CLEAN, row, Row, Table};
use symbolic::common::split_path;
use symbolicator_js::interface::{CompletedJsSymbolicationResponse, JsFrame};
use symbolicator_native::interface::{
    CompleteObjectInfo, CompletedSymbolicationResponse, FrameTrust, SymbolicatedFrame,
};

#[derive(Debug, Clone)]
pub enum CompletedResponse {
    NativeSymbolication(CompletedSymbolicationResponse),
    JsSymbolication(CompletedJsSymbolicationResponse),
}

pub fn print_compact(response: CompletedResponse) {
    match response {
        CompletedResponse::NativeSymbolication(response) => print_compact_native(response),
        CompletedResponse::JsSymbolication(response) => print_compact_js(response),
    }
}

pub fn print_pretty(response: CompletedResponse) {
    match response {
        CompletedResponse::NativeSymbolication(response) => print_pretty_native(response),
        CompletedResponse::JsSymbolication(response) => print_pretty_js(response),
    }
}

#[derive(Clone, Debug)]
struct NativeFrameData {
    instruction_addr: u64,
    trust: &'static str,
    module: Option<(String, u64)>,
    func: Option<(String, u64)>,
    file: Option<(String, u32)>,
}

#[derive(Clone, Debug)]
struct NativeFrames {
    inner: Peekable<IntoIter<SymbolicatedFrame>>,
    modules: HashMap<String, CompleteObjectInfo>,
}

impl Iterator for NativeFrames {
    type Item = NativeFrameData;

    fn next(&mut self) -> Option<Self::Item> {
        let frame = self.inner.next()?;
        let is_inline = Some(frame.raw.instruction_addr)
            == self
                .inner
                .peek()
                .map(|next_frame| next_frame.raw.instruction_addr);

        let trust = if is_inline {
            "inline"
        } else {
            match frame.raw.trust {
                FrameTrust::None => "none",
                FrameTrust::Scan => "scan",
                FrameTrust::CfiScan => "cfiscan",
                FrameTrust::Fp => "fp",
                FrameTrust::Cfi => "cfi",
                FrameTrust::PreWalked => "prewalked",
                FrameTrust::Context => "context",
            }
        };

        let instruction_addr = frame.raw.instruction_addr.0;

        let module = frame.raw.package.map(|module_file| {
            let module_addr = self.modules[&module_file].raw.image_addr.0;
            let module_file = split_path(&module_file).1.into();
            let module_rel_addr = instruction_addr - module_addr;

            (module_file, module_rel_addr)
        });

        let func = frame.raw.function.or(frame.raw.symbol).map(|func| {
            let sym_rel_addr = frame
                .raw
                .sym_addr
                .map(|sym_addr| instruction_addr - sym_addr.0)
                .unwrap_or_default();

            (func, sym_rel_addr)
        });

        let file = frame.raw.filename.map(|file| {
            let line = frame.raw.lineno.unwrap_or(0);

            (file, line)
        });

        Some(NativeFrameData {
            instruction_addr,
            trust,
            module,
            func,
            file,
        })
    }
}

fn get_crashing_thread_frames(mut response: CompletedSymbolicationResponse) -> NativeFrames {
    let modules: HashMap<_, _> = response
        .modules
        .into_iter()
        .filter_map(|module| Some((module.raw.code_file.clone()?, module)))
        .collect();

    let crashing_thread_idx = response
        .stacktraces
        .iter()
        .position(|s| s.is_requesting.unwrap_or(false))
        .unwrap_or(0);

    let crashing_thread = response.stacktraces.swap_remove(crashing_thread_idx);
    NativeFrames {
        inner: crashing_thread.frames.into_iter().peekable(),
        modules,
    }
}

fn print_compact_native(response: CompletedSymbolicationResponse) {
    if response.stacktraces.is_empty() {
        return;
    }

    let mut table = Table::new();
    table.set_format(*FORMAT_CLEAN);
    table.set_titles(row![b => "Trust", "Instruction", "Module File", "", "Function", "", "File"]);

    for frame in get_crashing_thread_frames(response) {
        let mut row = Row::empty();
        let NativeFrameData {
            instruction_addr,
            trust,
            module,
            func,
            file,
        } = frame;

        row.add_cell(cell!(trust));
        row.add_cell(cell!(r->format!("{instruction_addr:#x}")));

        match module {
            Some((module_file, module_offset)) => {
                row.add_cell(cell!(module_file));
                row.add_cell(cell!(r->format!("+{module_offset:#x}")));
            }
            None => row.add_cell(cell!("").with_hspan(2)),
        }

        match func {
            Some((func, func_offset)) => {
                row.add_cell(cell!(func));
                row.add_cell(cell!(r->format!("+{func_offset:#x}")));
            }
            None => row.add_cell(cell!("").with_hspan(2)),
        }

        match file {
            Some((name, line)) => row.add_cell(cell!(format!("{name}:{line}"))),
            None => row.add_cell(cell!("")),
        }

        table.add_row(row);
    }

    table.printstd();
}

fn print_pretty_native(response: CompletedSymbolicationResponse) {
    if response.stacktraces.is_empty() {
        return;
    }

    let mut table = Table::new();
    table.set_format(*prettytable::format::consts::FORMAT_CLEAN);

    for (i, frame) in get_crashing_thread_frames(response).enumerate() {
        let NativeFrameData {
            instruction_addr,
            trust,
            module,
            func,
            file,
        } = frame;

        let title_cell = cell!(lb->format!("Frame #{i}")).with_hspan(2);
        table.add_row(Row::new(vec![title_cell]));

        table.add_row(row![r->"  Trust:", trust]);
        table.add_row(row![r->"  Instruction:", format!("{instruction_addr:#0x}")]);

        if let Some((module_file, module_offset)) = module {
            table.add_row(row![
                r->"  Module:",
                format!("{module_file} +{module_offset:#0x}")
            ]);
        }

        if let Some((func, func_offset)) = func {
            table.add_row(row![r->"  Function:", format!("{func} +{func_offset:#x}")]);
        }

        if let Some((name, line)) = file {
            table.add_row(row![r->"  File:", format!("{name}:{line}")]);
        }

        table.add_empty_row();
    }
    table.printstd();
}

fn print_compact_js(mut response: CompletedJsSymbolicationResponse) {
    if response.stacktraces.is_empty() {
        return;
    }

    let mut table = Table::new();
    table.set_format(*FORMAT_CLEAN);
    table.set_titles(row![b => "Function", "File", "Line", "Column"]);

    for frame in std::mem::take(&mut response.stacktraces[0].frames) {
        let mut row = Row::empty();
        let JsFrame {
            filename,
            function,
            lineno,
            colno,
            ..
        } = frame;

        let function = function.unwrap_or_default();
        row.add_cell(cell!(function));

        let filename = filename.unwrap_or_default();
        row.add_cell(cell!(filename));

        let line = lineno.map(|line| line.to_string()).unwrap_or_default();
        row.add_cell(cell!(line));

        let col = colno.map(|col| col.to_string()).unwrap_or_default();
        row.add_cell(cell!(col));

        table.add_row(row);
    }

    table.printstd();
}

fn print_pretty_js(mut response: CompletedJsSymbolicationResponse) {
    if response.stacktraces.is_empty() {
        return;
    }

    let mut table = Table::new();
    table.set_format(*prettytable::format::consts::FORMAT_CLEAN);

    for (i, frame) in std::mem::take(&mut response.stacktraces[0].frames)
        .iter()
        .enumerate()
    {
        let JsFrame {
            module,
            function,
            filename,
            lineno,
            colno,
            ..
        } = frame;

        let title_cell = cell!(lb->format!("Frame #{i}")).with_hspan(2);
        table.add_row(Row::new(vec![title_cell]));

        if let Some(module) = module {
            table.add_row(row![
                r->"  Module:",
                module,
            ]);
        }

        if let Some(function) = function {
            table.add_row(row![r->"  Function:", function]);
        }

        if let Some(name) = filename {
            table.add_row(row![r->"  File:", name]);
        }

        match (colno, lineno) {
            (Some(col), Some(line)) => {
                table.add_row(row![r->"  Line/Column:", format!("{}:{}", line, col)]);
            }
            (Some(col), None) => {
                table.add_row(row![r->"  Column:", format!("{}", col)]);
            }
            (None, Some(line)) => {
                table.add_row(row![r->"  Line:", format!("{}", line)]);
            }
            (None, None) => {
                table.add_row(row![r->"  Line/Column:", "N/A"]);
            }
        }

        table.add_empty_row();
    }
    table.printstd();

    if response.errors.is_empty() {
        return;
    }

    let mut table = Table::new();
    table.set_format(*prettytable::format::consts::FORMAT_CLEAN);
    let title_cell = cell!(lb->"Errors during symbolication").with_hspan(2);
    table.add_row(Row::new(vec![title_cell]));

    for error in response.errors {
        table.add_row(row![
            r->"  Path:",
            error.abs_path,
        ]);

        table.add_row(row![
            r->"  Error:",
            error.kind,
        ]);

        table.add_empty_row();
    }
    table.printstd();
}
