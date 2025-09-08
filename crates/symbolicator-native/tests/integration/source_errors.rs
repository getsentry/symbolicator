use std::time::Duration;

use symbolicator_native::interface::{CompletedSymbolicationResponse, FrameStatus};
use symbolicator_service::objects::{ObjectDownloadInfo, ObjectUseInfo};
use symbolicator_service::types::ObjectFileStatus;

use crate::{Server, example_request, setup_service};

#[tokio::test]
async fn test_download_errors() {
    let (symbolication, _cache_dir) = setup_service(|config| {
        config.timeouts.max_download = Duration::from_millis(200);
    });

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
        let module = res.modules.pop().unwrap();
        let candidate = module.candidates.into_inner().pop().unwrap();
        (
            frame.status,
            module.debug_status,
            candidate.debug,
            candidate.download,
        )
    };

    // NOTE: we run requests twice to make sure that round-trips through the cache give us the same results.
    for _ in 0..2 {
        // NOTE: we try this 3 times on error
        let source = hitcounter.source("rejected", "/respond_statuscode/500/");
        let request = example_request(vec![source]);
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
        let request = example_request(vec![source]);
        let response = symbolication.symbolicate(request).await.unwrap();

        assert_eq!(
            get_statuses(response),
            (
                FrameStatus::Missing,
                ObjectFileStatus::Timeout,
                ObjectUseInfo::None,
                ObjectDownloadInfo::Error {
                    details: "download timed out after 200ms".into()
                }
            )
        );

        let source = hitcounter.source("notfound", "/respond_statuscode/404/");
        let request = example_request(vec![source]);
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
        let request = example_request(vec![source]);
        let response = symbolication.symbolicate(request).await.unwrap();

        assert_eq!(
            get_statuses(response),
            (
                FrameStatus::Missing,
                ObjectFileStatus::FetchingFailed,
                ObjectUseInfo::None,
                ObjectDownloadInfo::NoPerm {
                    details: "403 Forbidden".into()
                }
            )
        );

        let source = hitcounter.source("invalid", "/garbage_data/invalid");
        let request = example_request(vec![source]);
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

#[tokio::test]
async fn test_deny_list() {
    let (symbolication, _cache_dir) = setup_service(|config| {
        config.cache_dir = None;
        config.caches.downloaded.retry_misses_after = Some(Duration::ZERO);
        config.caches.derived.retry_misses_after = Some(Duration::ZERO);
        // FIXME: `object_meta` caches treat download errors as `malformed`
        config.caches.derived.retry_malformed_after = Some(Duration::ZERO);
        config.timeouts.max_download = Duration::from_millis(200);
        config.deny_list_time_window = Duration::from_millis(500);
        config.deny_list_bucket_size = Duration::from_millis(100);
        config.deny_list_threshold = 2;
        config.deny_list_block_time = Duration::from_millis(500);
    });

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
        let module = res.modules.pop().unwrap();
        let candidate = module.candidates.into_inner().pop().unwrap();
        (
            frame.status,
            module.debug_status,
            candidate.debug,
            candidate.download,
        )
    };

    let source = hitcounter.source("rejected", "/respond_statuscode/500/");
    let request = example_request(vec![source]);

    // The first two times should return 500
    for _ in 0..2 {
        let response = symbolication.symbolicate(request.clone()).await.unwrap();
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
    }

    // For the third time the host should be blocked
    let response = symbolication.symbolicate(request.clone()).await.unwrap();
    assert_eq!(
        get_statuses(response),
        (
            FrameStatus::Missing,
            ObjectFileStatus::FetchingFailed,
            ObjectUseInfo::None,
            ObjectDownloadInfo::Error {
                details: "download failed: Host localhost is temporarily blocked because there were too many download failures. It will remain blocked for a maximum of 500ms. The error that triggered the block was: `download failed: 500 Internal Server Error`.".into()
            }
        )
    );

    let source = hitcounter.source("pending", "/delay/1h/");
    let request = example_request(vec![source]);

    std::thread::sleep(Duration::from_millis(500));

    // The first two times should return a timeout
    for _ in 0..2 {
        let response = symbolication.symbolicate(request.clone()).await.unwrap();
        assert_eq!(
            get_statuses(response),
            (
                FrameStatus::Missing,
                ObjectFileStatus::Timeout,
                ObjectUseInfo::None,
                ObjectDownloadInfo::Error {
                    details: "download timed out after 200ms".into()
                }
            )
        );
    }

    // For the third time the host should be blocked
    let response = symbolication.symbolicate(request.clone()).await.unwrap();
    assert_eq!(
        get_statuses(response),
        (
            FrameStatus::Missing,
            ObjectFileStatus::FetchingFailed,
            ObjectUseInfo::None,
            ObjectDownloadInfo::Error {
                details: "download failed: Server is temporarily blocked".into()
            }
        )
    );

    std::thread::sleep(Duration::from_millis(500));

    let source = hitcounter.source("notfound", "/respond_statuscode/404/");
    let request = example_request(vec![source]);

    // 404 should not result in blocking
    for _ in 0..3 {
        let response = symbolication.symbolicate(request.clone()).await.unwrap();
        assert_eq!(
            get_statuses(response),
            (
                FrameStatus::Missing,
                ObjectFileStatus::Missing,
                ObjectUseInfo::None,
                ObjectDownloadInfo::NotFound
            )
        );
    }

    std::thread::sleep(Duration::from_millis(500));

    let source = hitcounter.source("permissiondenied", "/respond_statuscode/403/");
    let request = example_request(vec![source]);

    // 403 should not result in blocking
    for _ in 0..3 {
        let response = symbolication.symbolicate(request.clone()).await.unwrap();

        assert_eq!(
            get_statuses(response),
            (
                FrameStatus::Missing,
                ObjectFileStatus::FetchingFailed,
                ObjectUseInfo::None,
                ObjectDownloadInfo::NoPerm {
                    details: "403 Forbidden".into()
                }
            )
        );
    }

    // server errors are tried up to 3 times, all others once, for a total of
    // 14 requests
    assert_eq!(hitcounter.accesses(), 14);
}
