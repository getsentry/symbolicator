use std::sync::Arc;

use anyhow::bail;
use serde::Deserialize;
use symbolic::common::Language;
use symbolicator_js::interface::{
    JsFrame, JsFrameData, JsModule, JsStacktrace, SymbolicateJsStacktraces,
};
use symbolicator_native::interface::{
    AddrMode, CompleteObjectInfo, FrameTrust, RawFrame, RawStacktrace, Signal, StacktraceOrigin,
    SymbolicateStacktraces,
};
use symbolicator_service::types::{FrameOrder, Platform, RawObjectInfo, Scope, ScrapingConfig};
use symbolicator_service::utils::hex::HexValue;
use symbolicator_sources::{SentrySourceConfig, SourceConfig};

pub fn create_js_symbolication_request(
    scope: Scope,
    source: Arc<SentrySourceConfig>,
    event: Event,
    scraping_enabled: bool,
) -> anyhow::Result<SymbolicateJsStacktraces> {
    let Event {
        platform,
        debug_meta,
        exception,
        threads,
        release,
        dist,
        ..
    } = event;

    let mut stacktraces = vec![];
    if let Some(mut excs) = exception.map(|excs| excs.values) {
        stacktraces.extend(
            excs.iter_mut()
                .filter_map(|exc| exc.raw_stacktrace.take().or_else(|| exc.stacktrace.take())),
        );
    }
    if let Some(mut threads) = threads.map(|threads| threads.values) {
        stacktraces.extend(threads.iter_mut().filter_map(|thread| {
            thread
                .raw_stacktrace
                .take()
                .or_else(|| thread.stacktrace.take())
        }));
    }

    let stacktraces: Vec<_> = stacktraces
        .into_iter()
        .map(JsStacktrace::from)
        .filter(|stacktrace| !stacktrace.frames.is_empty())
        .collect();

    let modules: Vec<_> = debug_meta
        .images
        .into_iter()
        .filter_map(|module| match module {
            Module::Sourcemap(m) => Some(m),
            _ => None,
        })
        .collect();

    if stacktraces.is_empty() {
        bail!("Event has no usable frames");
    };

    Ok(SymbolicateJsStacktraces {
        platform: Some(platform),
        scope,
        source,
        release,
        dist,
        scraping: ScrapingConfig {
            enabled: scraping_enabled,
            ..Default::default()
        },
        apply_source_context: true,
        // we manually reversed the frames when we created the stacktraces, so this is
        // "callee first"
        frame_order: FrameOrder::CalleeFirst,

        stacktraces,
        modules,
    })
}

pub fn create_native_symbolication_request(
    scope: Scope,
    sources: Arc<[SourceConfig]>,
    event: Event,
) -> anyhow::Result<SymbolicateStacktraces> {
    let Event {
        debug_meta,
        exception,
        threads,
        signal,
        ..
    } = event;

    let mut stacktraces = vec![];
    if let Some(mut excs) = exception.map(|excs| excs.values) {
        stacktraces.extend(
            excs.iter_mut()
                .filter_map(|exc| exc.raw_stacktrace.take().or_else(|| exc.stacktrace.take())),
        );
    }
    if let Some(mut threads) = threads.map(|threads| threads.values) {
        stacktraces.extend(threads.iter_mut().filter_map(|thread| {
            thread
                .raw_stacktrace
                .take()
                .or_else(|| thread.stacktrace.take())
        }));
    }

    let stacktraces: Vec<_> = stacktraces
        .into_iter()
        .map(RawStacktrace::from)
        .filter(|stacktrace| !stacktrace.frames.is_empty())
        .collect();

    let modules: Vec<_> = debug_meta
        .images
        .into_iter()
        .filter_map(|module| match module {
            Module::Object(m) => Some(CompleteObjectInfo::from(m)),
            _ => None,
        })
        .collect();

    if modules.is_empty() {
        bail!("Event has no debug images");
    };

    if stacktraces.is_empty() {
        bail!("Event has no usable frames");
    };

    Ok(SymbolicateStacktraces {
        platform: Some(event.platform),
        scope,
        signal,
        sources,
        origin: StacktraceOrigin::Symbolicate,
        stacktraces,
        modules,
        apply_source_context: true,
        scraping: Default::default(),
        rewrite_first_module: Default::default(),
        // we manually reversed the frames when we created the stacktraces, so this is
        // "callee first"
        frame_order: FrameOrder::CalleeFirst,
        minidump: None,
    })
}

#[derive(Debug, Deserialize)]
pub struct Event {
    #[serde(default)]
    pub platform: Platform,
    #[serde(default)]
    debug_meta: DebugMeta,
    exception: Option<Exceptions>,
    threads: Option<Threads>,
    signal: Option<Signal>,
    release: Option<String>,
    dist: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct DebugMeta {
    images: Vec<Module>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Module {
    Sourcemap(JsModule),
    Object(RawObjectInfo),
}

#[derive(Debug, Deserialize)]
struct Exceptions {
    values: Vec<Exception>,
}

#[derive(Debug, Deserialize)]
struct Exception {
    raw_stacktrace: Option<Stacktrace>,
    stacktrace: Option<Stacktrace>,
}

#[derive(Debug, Deserialize)]
struct Threads {
    values: Vec<Thread>,
}

#[derive(Debug, Deserialize)]
struct Thread {
    raw_stacktrace: Option<Stacktrace>,
    stacktrace: Option<Stacktrace>,
}

#[derive(Debug, Deserialize)]
struct Stacktrace {
    frames: Vec<Frame>,
    #[serde(default)]
    is_requesting: bool,
}

impl From<Stacktrace> for RawStacktrace {
    fn from(stacktrace: Stacktrace) -> Self {
        let frames = stacktrace
            .frames
            .into_iter()
            .filter_map(to_raw_frame)
            .rev()
            .collect();

        Self {
            is_requesting: Some(stacktrace.is_requesting),
            frames,
            ..Default::default()
        }
    }
}

impl From<Stacktrace> for JsStacktrace {
    fn from(stacktrace: Stacktrace) -> Self {
        let frames = stacktrace
            .frames
            .into_iter()
            .filter_map(to_js_frame)
            .rev()
            .collect();

        Self { frames }
    }
}

#[derive(Debug, Deserialize)]
struct Frame {
    platform: Option<Platform>,
    #[serde(default)]
    addr_mode: AddrMode,

    instruction_addr: Option<HexValue>,

    #[serde(default)]
    function_id: Option<HexValue>,

    #[serde(default)]
    package: Option<String>,

    lang: Option<Language>,

    symbol: Option<String>,

    sym_addr: Option<HexValue>,

    function: Option<String>,

    filename: Option<String>,

    abs_path: Option<String>,

    lineno: Option<u32>,

    colno: Option<u32>,

    #[serde(default)]
    pre_context: Vec<String>,

    context_line: Option<String>,

    #[serde(default)]
    post_context: Vec<String>,

    module: Option<String>,

    source_link: Option<String>,

    in_app: Option<bool>,

    #[serde(default)]
    trust: FrameTrust,

    #[serde(default)]
    data: JsFrameData,
}

fn to_raw_frame(value: Frame) -> Option<RawFrame> {
    Some(RawFrame {
        platform: value.platform,
        addr_mode: value.addr_mode,
        instruction_addr: value.instruction_addr?,
        adjust_instruction_addr: None,
        function_id: value.function_id,
        package: value.package,
        lang: value.lang,
        symbol: value.symbol,
        sym_addr: value.sym_addr,
        function: value.function,
        filename: value.filename,
        abs_path: value.abs_path,
        lineno: value.lineno,
        pre_context: value.pre_context,
        context_line: value.context_line,
        post_context: value.post_context,
        source_link: value.source_link,
        in_app: value.in_app,
        trust: value.trust,
        registers: None,
        arguments: Vec::new(),
        local_variables: Vec::new(),
    })
}

fn to_js_frame(value: Frame) -> Option<JsFrame> {
    Some(JsFrame {
        platform: value.platform,
        function: value.function,
        filename: value.filename,
        module: value.module,
        abs_path: value.abs_path?,
        lineno: value.lineno?,
        colno: value.colno,
        pre_context: value.pre_context,
        context_line: value.context_line,
        post_context: value.post_context,
        token_name: None,
        data: value.data,
    })
}
