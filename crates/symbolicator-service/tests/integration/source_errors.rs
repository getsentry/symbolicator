use std::time::Duration;

use symbolicator_service::types::{
    CompletedSymbolicationResponse, FrameStatus, ObjectDownloadInfo, ObjectFileStatus,
    ObjectUseInfo,
};
use symbolicator_test::FailingSymbolServer;

use crate::symbolication::{get_symbolication_request, setup_service};

#[tokio::test]
async fn test_download_errors() {
    let (symbolication, _cache_dir) = setup_service(|config| {
        config.max_download_timeout = Duration::from_millis(100);
    })
    .await;

    let server = FailingSymbolServer::new();

    // This returns frame and module statuses:
    // (
    //     frame.status,
    //     module.debug_status,
    //     module.candidate.debug
    //     module.candidate.download
    // )
    let get_statuses = |mut res: CompletedSymbolicationResponse| {
        dbg!(&res);
        let frame = &res.stacktraces[0].frames[0];
        let mut module = res.modules.remove(0);
        let candidate = module.candidates.0.remove(0);
        (
            frame.status,
            module.debug_status,
            candidate.debug,
            candidate.download,
        )
    };

    let request = get_symbolication_request(vec![server.reject_source]);
    let response = symbolication.symbolicate(request).await.unwrap();

    assert_eq!(
        get_statuses(response),
        (
            FrameStatus::Missing,
            ObjectFileStatus::Missing, // XXX: should be `FetchingFailed`
            ObjectUseInfo::None,
            ObjectDownloadInfo::Error {
                details: "failed to download: 500 Internal Server Error".into()
            }
        )
    );

    let request = get_symbolication_request(vec![server.pending_source]);
    let response = symbolication.symbolicate(request).await.unwrap();

    assert_eq!(
        get_statuses(response),
        (
            FrameStatus::Missing,
            ObjectFileStatus::Missing, // XXX: should be `Timeout`
            ObjectUseInfo::None,
            ObjectDownloadInfo::Error {
                details: "download was cancelled".into()
            }
        )
    );

    let request = get_symbolication_request(vec![server.not_found_source]);
    let response = symbolication.symbolicate(request).await.unwrap();

    assert_eq!(
        get_statuses(response),
        (
            FrameStatus::Missing,
            ObjectFileStatus::Missing,
            ObjectUseInfo::None,
            ObjectDownloadInfo::NotFound
        )
    );

    let request = get_symbolication_request(vec![server.forbidden_source]);
    let response = symbolication.symbolicate(request).await.unwrap();

    assert_eq!(
        get_statuses(response),
        (
            FrameStatus::Missing,
            ObjectFileStatus::Missing, // XXX: should be `FetchingFailed`
            ObjectUseInfo::None,
            ObjectDownloadInfo::NoPerm { details: "".into() }
        )
    );

    let request = get_symbolication_request(vec![server.invalid_file_source]);
    let response = symbolication.symbolicate(request).await.unwrap();

    assert_eq!(
        get_statuses(response),
        (
            FrameStatus::Malformed,
            ObjectFileStatus::Malformed,
            ObjectUseInfo::Malformed,
            ObjectDownloadInfo::Malformed
        )
    );
}
