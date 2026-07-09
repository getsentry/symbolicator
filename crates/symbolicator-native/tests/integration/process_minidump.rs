use std::io::Write;
use std::sync::Arc;

use axum::Router;
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;
use symbolicator_test::Server;
use tempfile::tempfile;

use symbolicator_native::interface::{
    AttachmentFile, CompletedSymbolicationResponse, ProcessMinidump,
};
use symbolicator_service::types::{Scope, UnwindStrategy};

use crate::{assert_snapshot, read_fixture, setup_service, symbol_server};

async fn stackwalk_minidump(path: &str) -> CompletedSymbolicationResponse {
    stackwalk_minidump_with_unwind_strategy(path, UnwindStrategy::CfiFirst).await
}

async fn stackwalk_minidump_with_unwind_strategy(
    path: &str,
    unwind_strategy: UnwindStrategy,
) -> CompletedSymbolicationResponse {
    let (symbolication, _cache_dir) =
        setup_service(|cfg| cfg.unwind_strategy = unwind_strategy);
    let (_symsrv, source) = symbol_server();

    let minidump = read_fixture(path);
    let mut minidump_file = tempfile().unwrap();
    minidump_file.write_all(&minidump).unwrap();

    symbolication
        .process_minidump(ProcessMinidump {
            platform: None,
            scope: Scope::Global,
            minidump_file: AttachmentFile::Local(minidump_file),
            sources: Arc::new([source]),
            scraping: Default::default(),
            rewrite_first_module: Default::default(),
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn test_minidump_windows() {
    let res = stackwalk_minidump("windows.dmp").await;
    assert_snapshot!(res);
}

#[tokio::test]
async fn test_minidump_macos() {
    let res = stackwalk_minidump("macos.dmp").await;
    assert_snapshot!(res);
}

/// Verifies that `UnwindStrategy::FramePointerFirst` doesn't regress stackwalking
/// for minidumps that have intact frame pointer chains throughout: the two unwind
/// methods should agree, producing identical stacktraces to the default
/// `CfiFirst` strategy.
///
/// `windows.dmp` is deliberately excluded here: it's a 32-bit release build with
/// frame pointer omission (FPO) in some system DLLs, so the two strategies are
/// *expected* to diverge for it (see `test_minidump_windows_frame_pointer_first`).
#[tokio::test]
async fn test_minidump_frame_pointer_first_matches_cfi_first() {
    for fixture in ["macos.dmp", "linux.dmp"] {
        let cfi_first = stackwalk_minidump_with_unwind_strategy(fixture, UnwindStrategy::CfiFirst)
            .await
            .stacktraces;
        let fp_first =
            stackwalk_minidump_with_unwind_strategy(fixture, UnwindStrategy::FramePointerFirst)
                .await
                .stacktraces;
        assert_eq!(
            format!("{cfi_first:#?}"),
            format!("{fp_first:#?}"),
            "mismatch for fixture {fixture}"
        );
    }
}

/// Demonstrates the tradeoff `UnwindStrategy::FramePointerFirst` is documented to
/// have: `windows.dmp` is a 32-bit release build where some system DLLs (e.g.
/// `rpcrt4.dll`) omit frame pointers (FPO). Walking the (unreliable) `ebp` chain
/// first there produces a spurious extra frame that the accurate PDB-derived CFI
/// (used by the default `CfiFirst` strategy, see `test_minidump_windows`) does not.
/// This confirms the `unwind_strategy` config option is wired up end-to-end and
/// actually changes stackwalking behavior, not just that it's accepted/parsed.
#[tokio::test]
async fn test_minidump_windows_frame_pointer_first() {
    let res =
        stackwalk_minidump_with_unwind_strategy("windows.dmp", UnwindStrategy::FramePointerFirst)
            .await;
    assert_snapshot!(res);
}

/// Verifies that debug information is used to guide stack scanning.
///
/// The minidump being tested has frame pointers disabled, so in the absence of (usable) CFI,
/// there is no choice but to scan. Left to its own devices, the stack scanner can produce a lot
/// of spurious frames, so we want to use both CFI and debug info that we have available to guide
/// it. The heuristic is as follows: if stack scanning would produce an instruction address
/// * falling into some module,
/// * for which we have CFI or debug info,
/// * which doesn't cover that instruction address,
/// then that address is probably bogus and we discard the frame.
///
/// The CFI half of this heuristic was implemented in
/// https://github.com/getsentry/symbolicator/pull/1651.
/// https://github.com/getsentry/symbolicator/pull/1913 added the debug info part.
///
/// This test simulates an actual customer situation: we have both debug info and CFI
/// for the minidump, but the CFI is truncated/poor. If we only used CFI to constrain the
/// stack scanner, all frames in `sentry_example` would be rejected. However, since we also use
/// the (good) debug info to check the frames, they are retained.
///
/// The `dyld` frames in the stacktrace are found by scanning, and since we have no CFI or
/// debug info at all for that module, they're all accepted.
#[tokio::test]
async fn test_minidump_macos_broken_cfi() {
    let res = stackwalk_minidump("macos-no-fp.dmp").await;
    let crashed_thread = res
        .stacktraces
        .iter()
        .find(|st| st.is_requesting.unwrap_or_default())
        .unwrap();
    assert_snapshot!(crashed_thread);
}

#[tokio::test]
async fn test_minidump_linux() {
    let res = stackwalk_minidump("linux.dmp").await;
    assert_snapshot!(res);
}

#[tokio::test]
async fn test_minidump_attachment_download() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_symsrv, source) = symbol_server();

    async fn get_minidump() -> impl IntoResponse {
        let minidump = read_fixture("windows.dmp");
        let compressed = zstd::bulk::compress(&minidump, 0).unwrap();

        ([(header::CONTENT_ENCODING, "zstd")], compressed)
    }
    let router = Router::new().route("/the_minidump.dmp", get(get_minidump));
    let attachment_server = Server::with_router(router);

    let response = symbolication
        .process_minidump(ProcessMinidump {
            platform: None,
            scope: Scope::Global,
            minidump_file: AttachmentFile::Remote {
                storage_url: attachment_server.url("/the_minidump.dmp").to_string(),
                storage_token: None,
            },
            sources: Arc::new([source]),
            scraping: Default::default(),
            rewrite_first_module: Default::default(),
        })
        .await;

    assert_snapshot!(response.unwrap());
}
