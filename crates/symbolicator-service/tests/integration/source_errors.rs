use std::time::Duration;

use symbolicator_service::types::{
    CompletedSymbolicationResponse, FrameStatus, ObjectDownloadInfo, ObjectFileStatus,
    ObjectUseInfo,
};
use symbolicator_test::Server;

use crate::symbolication::{get_symbolication_request, setup_service};

#[tokio::test]
async fn test_download_errors() {
    let (symbolication, _cache_dir) = setup_service(|config| {
        config.max_download_timeout = Duration::from_millis(200);
    })
    .await;

    let hitcounter = Server::new();

    // This returns frame and module statuses:
    // (
    //     frame.status,
    //     module.debug_status,
    //     module.candidate.debug
    //     module.candidate.download
    // )
    let get_statuses = |mut res: CompletedSymbolicationResponse| {
        let frame = &res.stacktraces[0].frames[0];
        let mut module = res.modules.pop().unwrap();
        let candidate = module.candidates.0.pop().unwrap();
        (
            frame.status,
            module.debug_status,
            candidate.debug,
            candidate.download,
        )
    };

    // NOTE: we run requests twice to make sure that round-trips through the cache give us the same results.
    for i in 0..2 {
        // NOTE: we try this 3 times on error
        let source = hitcounter.source("rejected", "/respond_statuscode/500/");
        let request = get_symbolication_request(vec![source]);
        let response = symbolication.symbolicate(request).await.unwrap();

        assert_eq!(
            get_statuses(response),
            (
                FrameStatus::Missing,
                ObjectFileStatus::FetchingFailed,
                ObjectUseInfo::None,
                ObjectDownloadInfo::Error {
                    details: "download failed: 500 Internal Server Error".into()
                }
            )
        );

        // NOTE: we should probably try this 3 times?
        let source = hitcounter.source("pending", "/delay/1h/");
        let request = get_symbolication_request(vec![source]);
        let response = symbolication.symbolicate(request).await.unwrap();

        assert_eq!(
            get_statuses(response),
            (
                FrameStatus::Missing,
                ObjectFileStatus::Timeout,
                ObjectUseInfo::None,
                ObjectDownloadInfo::Error {
                    details: "download timed out".into()
                }
            )
        );

        let source = hitcounter.source("notfound", "/respond_statuscode/404/");
        let request = get_symbolication_request(vec![source]);
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

        let source = hitcounter.source("permissiondenied", "/respond_statuscode/403/");
        let request = get_symbolication_request(vec![source]);
        let response = symbolication.symbolicate(request).await.unwrap();

        assert_eq!(
            get_statuses(response),
            (
                FrameStatus::Missing,
                ObjectFileStatus::FetchingFailed,
                ObjectUseInfo::None,
                ObjectDownloadInfo::NoPerm {
                    // FIXME: We are currently serializing `CacheError` in "legacy" mode that does
                    // not support details
                    details: if i == 0 {
                        "403 Forbidden".into()
                    } else {
                        "".into()
                    }
                }
            )
        );

        let source = hitcounter.source("invalid", "/garbage_data/invalid");
        let request = get_symbolication_request(vec![source]);
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

    // server errors are tried up to 3 times, all others once, for a total of
    // 7 requests, as the second requests should be served from cache
    assert_eq!(hitcounter.accesses(), 7);
}
