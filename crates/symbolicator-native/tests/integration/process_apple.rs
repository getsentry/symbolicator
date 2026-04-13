use std::sync::Arc;

use axum::Router;
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;
use symbolicator_test::Server;

use symbolicator_native::interface::AttachmentFile;
use symbolicator_service::types::Scope;

use crate::{assert_snapshot, read_fixture, setup_service, symbol_server};

#[tokio::test]
async fn test_attachment_download() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_symsrv, source) = symbol_server();

    async fn get_crash_report() -> impl IntoResponse {
        let report = read_fixture("apple_crash_report.txt");
        let compressed = zstd::bulk::compress(&report, 0).unwrap();

        ([(header::CONTENT_ENCODING, "zstd")], compressed)
    }
    let router = Router::new().route("/the_crash_report.txt", get(get_crash_report));
    let attachment_server = Server::with_router(router);

    let response = symbolication
        .process_apple_crash_report(
            None,
            Scope::Global,
            AttachmentFile::Remote {
                storage_url: attachment_server.url("/the_crash_report.txt").to_string(),
                storage_token: None,
            },
            Arc::new([source]),
            Default::default(),
        )
        .await;

    assert_snapshot!(response.unwrap());
}
