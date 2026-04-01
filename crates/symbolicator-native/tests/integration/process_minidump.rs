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
use symbolicator_service::types::Scope;

use crate::{assert_snapshot, read_fixture, setup_service, symbol_server};

async fn stackwalk_minidump(path: &str) -> CompletedSymbolicationResponse {
    let (symbolication, _cache_dir) = setup_service(|_| ());
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
