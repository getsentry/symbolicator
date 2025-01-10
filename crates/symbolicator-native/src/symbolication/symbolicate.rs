use std::sync::Arc;

use symbolic::common::Name;
use symbolic::demangle::Demangle;
use symbolicator_service::caches::SourceFilesCache;
use symbolicator_service::caching::{Cache, CacheError};
use symbolicator_service::objects::ObjectsActor;
use symbolicator_service::services::SharedServices;

use crate::caches::bitcode::BitcodeService;
use crate::caches::cficaches::CfiCacheActor;
use crate::caches::il2cpp::Il2cppService;
use crate::caches::ppdb_caches::PortablePdbCacheActor;
use crate::caches::symcaches::SymCacheActor;
use crate::interface::{
    AdjustInstructionAddr, CompleteStacktrace, CompletedSymbolicationResponse, FrameStatus,
    FrameTrust, RawFrame, RawStacktrace, Registers, Signal, SymbolicateStacktraces,
    SymbolicatedFrame,
};
use crate::metrics::{record_symbolication_metrics, StacktraceMetrics};

use super::demangle::{DemangleCache, DEMANGLE_OPTIONS};
use super::dotnet::symbolicate_dotnet_frame;
use super::module_lookup::{CacheFileEntry, ModuleLookup};
use super::native::{get_relative_caller_addr, symbolicate_native_frame};

// we should really rename this here to the `SymbolicatorService`, as it does a lot more
// than just symbolication ;-)
#[derive(Clone, Debug)]
pub struct SymbolicationActor {
    demangle_cache: DemangleCache,
    pub(crate) objects: ObjectsActor,
    symcaches: SymCacheActor,
    pub(crate) cficaches: CfiCacheActor,
    ppdb_caches: PortablePdbCacheActor,
    pub(crate) diagnostics_cache: Cache,
    pub(crate) sourcefiles_cache: Arc<SourceFilesCache>,
}

impl SymbolicationActor {
    pub fn new(services: &SharedServices) -> Self {
        let caches = &services.caches;
        let shared_cache = services.shared_cache.clone();
        let objects = services.objects.clone();
        let download_svc = services.download_svc.clone();
        let sourcefiles_cache = services.sourcefiles_cache.clone();

        let bitcode = BitcodeService::new(
            caches.auxdifs.clone(),
            shared_cache.clone(),
            download_svc.clone(),
        );

        let il2cpp = Il2cppService::new(caches.il2cpp.clone(), shared_cache.clone(), download_svc);

        let symcaches = SymCacheActor::new(
            caches.symcaches.clone(),
            shared_cache.clone(),
            objects.clone(),
            bitcode,
            il2cpp,
        );

        let cficaches = CfiCacheActor::new(
            caches.cficaches.clone(),
            shared_cache.clone(),
            objects.clone(),
        );

        let ppdb_caches =
            PortablePdbCacheActor::new(caches.ppdb_caches.clone(), shared_cache, objects.clone());

        let demangle_cache = DemangleCache::builder()
            .max_capacity(10 * 1024 * 1024) // 10 MiB, considering key and value:
            .weigher(|k, v| (k.0.len() + v.len()).try_into().unwrap_or(u32::MAX))
            .build();

        SymbolicationActor {
            demangle_cache,
            objects,
            symcaches,
            cficaches,
            ppdb_caches,
            diagnostics_cache: caches.diagnostics.clone(),
            sourcefiles_cache,
        }
    }

    #[tracing::instrument(skip_all)]
    pub async fn symbolicate(
        &self,
        request: SymbolicateStacktraces,
    ) -> anyhow::Result<CompletedSymbolicationResponse> {
        let SymbolicateStacktraces {
            platform,
            stacktraces,
            sources,
            scope,
            signal,
            origin,
            modules,
            apply_source_context,
            scraping,
            ..
        } = request;

        let mut module_lookup = ModuleLookup::new(scope.clone(), sources, modules);
        module_lookup
            .fetch_caches(
                self.symcaches.clone(),
                self.ppdb_caches.clone(),
                &stacktraces,
            )
            .await;

        let mut metrics = StacktraceMetrics::default();
        let mut stacktraces: Vec<_> = stacktraces
            .into_iter()
            .map(|trace| {
                symbolicate_stacktrace(
                    &self.demangle_cache,
                    trace,
                    &module_lookup,
                    &mut metrics,
                    signal,
                )
            })
            .collect();

        if apply_source_context {
            self.apply_source_context(&mut module_lookup, &mut stacktraces, &scraping)
                .await
        }

        // bring modules back into the original order
        let modules = module_lookup.into_inner();
        record_symbolication_metrics(platform, origin, metrics, &modules, &stacktraces);

        Ok(CompletedSymbolicationResponse {
            signal,
            stacktraces,
            modules,
            ..Default::default()
        })
    }
}

fn symbolicate_stacktrace(
    demangle_cache: &DemangleCache,
    thread: RawStacktrace,
    caches: &ModuleLookup,
    metrics: &mut StacktraceMetrics,
    signal: Option<Signal>,
) -> CompleteStacktrace {
    let default_adjustment = AdjustInstructionAddr::default_for_thread(&thread);
    let mut symbolicated_frames = vec![];
    let mut unsymbolicated_frames_iter = thread.frames.into_iter().enumerate().peekable();

    while let Some((index, mut frame)) = unsymbolicated_frames_iter.next() {
        let adjustment = AdjustInstructionAddr::for_frame(&frame, default_adjustment);
        match symbolicate_frame(
            demangle_cache,
            caches,
            &thread.registers,
            signal,
            &mut frame,
            index,
            adjustment,
        ) {
            Ok(frames) => {
                if matches!(frame.trust, FrameTrust::Scan) {
                    metrics.scanned_frames += 1;
                }
                symbolicated_frames.extend(frames)
            }
            Err(status) => {
                // Since symbolication failed, the function name was not demangled. In case there is
                // either one of `function` or `symbol`, treat that as mangled name and try to
                // demangle it. If that succeeds, write the demangled name back.
                let mangled = frame.function.as_deref().xor(frame.symbol.as_deref());
                let demangled = mangled.and_then(|m| Name::from(m).demangle(DEMANGLE_OPTIONS));
                if let Some(demangled) = demangled {
                    if let Some(old_mangled) = frame.function.replace(demangled) {
                        frame.symbol = Some(old_mangled);
                    }
                }

                // Temporary workaround: Skip false-positive frames from stack scanning after the
                // fact.
                //
                // Usually, the stack scanner would skip all scanned frames when it *knows* that
                // they cannot be symbolized. However, in our case we supply breakpad symbols
                // without function records. This breaks its original heuristic, since it would now
                // *always* skip scan frames. Our patch in breakpad omits this check.
                //
                // Here, we fix this after the fact.
                //
                // - MissingSymbol: If symbolication failed for a scanned frame where we *know* we
                //   have a debug info, but the lookup inside that file failed.
                // - UnknownImage: If symbolication failed because the stackscanner found an
                //   instruction_addr that is not in any image *we* consider valid. We discard
                //   images which do not have a debug id, while the stackscanner considers them
                //   perfectly fine.
                if frame.trust == FrameTrust::Scan
                    && (status == FrameStatus::MissingSymbol || status == FrameStatus::UnknownImage)
                {
                    continue;
                }

                // Glibc inserts an explicit `DW_CFA_undefined: RIP` DWARF rule to say that `_start`
                // has no return address.
                // See https://sourceware.org/git/?p=glibc.git;a=blob;f=sysdeps/x86_64/start.S;h=1b3e36826b8a477474cee24d1c931429fbdf6d8f;hb=HEAD#l59
                // We do not support this due to lack of breakpad support, and will thus use the
                // previous rule for RIP, which says to look it up the value on the stack,
                // resulting in an unmapped garbage frame. We work around this by trimming the
                // trailing garbage frame on the following conditions:
                // * it is unmapped (UnknownImage)
                // * this is the last frame to symbolicate (via peek)
                // * the previous symbolicated frame is `_start`
                let is_start =
                    |frame: &SymbolicatedFrame| frame.raw.function.as_deref() == Some("_start");
                if status == FrameStatus::UnknownImage
                    && unsymbolicated_frames_iter.peek().is_none()
                    && symbolicated_frames.last().is_some_and(is_start)
                {
                    continue;
                }

                metrics.unsymbolicated_frames += 1;
                match frame.trust {
                    FrameTrust::Scan => {
                        metrics.scanned_frames += 1;
                        metrics.unsymbolicated_scanned_frames += 1;
                    }
                    FrameTrust::Cfi => metrics.unsymbolicated_cfi_frames += 1,
                    FrameTrust::Context => metrics.unsymbolicated_context_frames += 1,
                    _ => {}
                }
                if status == FrameStatus::UnknownImage {
                    metrics.unmapped_frames += 1;
                }

                symbolicated_frames.push(SymbolicatedFrame {
                    status,
                    original_index: Some(index),
                    raw: frame,
                });
            }
        }
    }

    // we try to find a base frame among the bottom 5
    if !symbolicated_frames
        .iter()
        .rev()
        .take(5)
        .any(is_likely_base_frame)
    {
        metrics.truncated_traces += 1;
    }
    // macOS has some extremely short but perfectly fine stacks, such as:
    // `__workq_kernreturn` > `_pthread_wqthread` > `start_wqthread`
    if symbolicated_frames.len() < 3 {
        metrics.short_traces += 1;
    }

    if metrics.scanned_frames > 0 || metrics.unsymbolicated_frames > 0 {
        metrics.bad_traces += 1;
    }

    CompleteStacktrace {
        thread_id: thread.thread_id,
        thread_name: thread.thread_name,
        is_requesting: thread.is_requesting,
        registers: thread.registers,
        frames: symbolicated_frames,
    }
}

fn symbolicate_frame(
    demangle_cache: &DemangleCache,
    caches: &ModuleLookup,
    registers: &Registers,
    signal: Option<Signal>,
    frame: &mut RawFrame,
    index: usize,
    adjustment: AdjustInstructionAddr,
) -> Result<Vec<SymbolicatedFrame>, FrameStatus> {
    let lookup_result = caches
        .lookup_cache(frame.instruction_addr.0, frame.addr_mode)
        .ok_or(FrameStatus::UnknownImage)?;

    frame
        .package
        .clone_from(&lookup_result.object_info.raw.code_file);

    match lookup_result.cache {
        Ok(CacheFileEntry::SymCache(symcache)) => {
            let symcache = symcache.get();
            let relative_addr = get_relative_caller_addr(
                symcache,
                &lookup_result,
                registers,
                signal,
                index,
                adjustment,
            )?;
            symbolicate_native_frame(
                demangle_cache,
                symcache,
                lookup_result,
                relative_addr,
                frame,
                index,
            )
        }
        Ok(CacheFileEntry::PortablePdbCache(ppdb_cache)) => {
            symbolicate_dotnet_frame(ppdb_cache.get(), frame, index)
        }
        Err(CacheError::Malformed(_)) => Err(FrameStatus::Malformed),
        _ => Err(FrameStatus::Missing),
    }
}

/// Determine if the [`SymbolicatedFrame`] is likely to be a thread base.
///
/// This is just a heuristic that matches the function to well known thread entry points.
fn is_likely_base_frame(frame: &SymbolicatedFrame) -> bool {
    let function = match frame
        .raw
        .function
        .as_deref()
        .or(frame.raw.symbol.as_deref())
    {
        Some(f) => f,
        None => return false,
    };

    // C start/main
    if matches!(function, "main" | "start" | "_start") {
        return true;
    }

    // Windows and posix thread base. These often have prefixes depending on the OS and Version, so
    // we use a substring match here.
    if function.contains("UserThreadStart")
        || function.contains("thread_start")
        || function.contains("start_thread")
        || function.contains("start_wqthread")
    {
        return true;
    }

    false
}
