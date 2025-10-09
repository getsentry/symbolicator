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
            minidump_file: AttachmentFile::Remote(
                attachment_server.url("/the_minidump.dmp").to_string(),
            ),
            sources: Arc::new([source]),
            scraping: Default::default(),
            rewrite_first_module: Default::default(),
        })
        .await;

    assert_snapshot!(response.unwrap());
}
