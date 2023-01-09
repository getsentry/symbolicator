use std::sync::Arc;

use symbolicator_service::services::symbolication::{StacktraceOrigin, SymbolicateStacktraces};
use symbolicator_service::types::{ObjectDownloadInfo, RawObjectInfo, RawStacktrace};
use symbolicator_sources::{
    CommonSourceConfig, DirectoryLayout, DirectoryLayoutType, FileType, FilesystemSourceConfig,
    HttpSourceConfig, SourceConfig, SourceFilters, SourceId,
};
use symbolicator_test::{fixture, FailingSymbolServer};

use crate::assert_snapshot;
use crate::symbolication::{get_symbolication_request, setup_service};

fn request_fixture() -> (Vec<RawObjectInfo>, Vec<RawStacktrace>) {
    let modules = serde_json::from_str(
        r#"[{
          "type": "pe",
          "debug_id": "ff9f9f78-41db-88f0-cded-a9e1e9bff3b5-1",
          "code_file": "C:\\Windows\\System32\\kernel32.dll",
          "debug_file": "C:\\Windows\\System32\\wkernel32.pdb",
          "image_addr": "0x749d0000",
          "image_size": 851968
        }]"#,
    )
    .unwrap();
    let stacktraces = serde_json::from_str(
        r#"[{
          "registers": {"eip": "0x0000000001509530"},
          "frames": [{"instruction_addr": "0x749e8630"}]
        }]"#,
    )
    .unwrap();
    (modules, stacktraces)
}

#[tokio::test]
async fn test_no_sources() {
    let (symbolication, cache_dir) = setup_service(|_| ()).await;

    let (modules, stacktraces) = request_fixture();

    let request = SymbolicateStacktraces {
        modules: modules.into_iter().map(From::from).collect(),
        stacktraces,
        signal: None,
        origin: StacktraceOrigin::Symbolicate,
        sources: Arc::new([]),
        scope: Default::default(),
    };

    let response = symbolication.symbolicate(request).await;

    assert_snapshot!(response.unwrap());

    assert!(!cache_dir.path().join("objects/global").exists());
    assert!(!cache_dir.path().join("symcaches/global").exists());
}

#[tokio::test]
async fn test_sources_filetypes() {
    let (symbolication, _cache_dir) = setup_service(|_| ()).await;

    let (modules, stacktraces) = request_fixture();

    // This symbol source is filtering for only `mach_code` files.
    let source = FailingSymbolServer::new();

    let request = SymbolicateStacktraces {
        modules: modules.into_iter().map(From::from).collect(),
        stacktraces,
        signal: None,
        origin: StacktraceOrigin::Symbolicate,
        sources: Arc::new([source.not_found_source.clone()]),
        scope: Default::default(),
    };

    let response = symbolication.symbolicate(request).await;

    assert_eq!(source.accesses(), 0);

    assert_snapshot!(response.unwrap());
}

#[tokio::test]
async fn test_path_patterns() {
    let (symbolication, _cache_dir) = setup_service(|_| ()).await;

    let (modules, stacktraces) = request_fixture();

    let patterns = [
        (Some("\"?:/windows/**\""), true),
        (Some("\"?:/windows/*\""), true),
        (None, true),
        (Some("\"?:/windows/\""), false),
        (Some("\"d:/windows/**\""), false),
    ];

    for (pattern, should_be_found) in patterns {
        let path_patterns = if let Some(pattern) = pattern {
            vec![serde_json::from_str(pattern).unwrap()]
        } else {
            vec![]
        };
        let files = CommonSourceConfig {
            filters: SourceFilters {
                filetypes: vec![FileType::Pdb],
                path_patterns,
            },
            layout: DirectoryLayout {
                ty: DirectoryLayoutType::Symstore,
                ..Default::default()
            },
            ..Default::default()
        };
        let source = SourceConfig::Filesystem(Arc::new(FilesystemSourceConfig {
            id: SourceId::new("local"),
            path: fixture("symbols"),
            files,
        }));

        let request = SymbolicateStacktraces {
            modules: modules.iter().cloned().map(From::from).collect(),
            stacktraces: stacktraces.clone(),
            signal: None,
            origin: StacktraceOrigin::Symbolicate,
            sources: Arc::new([source]),
            scope: Default::default(),
        };

        let mut response = symbolication.symbolicate(request).await.unwrap();

        let mut module = response.modules.pop().unwrap();
        let candidate = module.candidates.0.pop().unwrap();

        if should_be_found {
            assert!(candidate
                .location
                .to_string()
                .ends_with("wkernel32.pdb/FF9F9F7841DB88F0CDEDA9E1E9BFF3B51/wkernel32.pdb"));
        } else {
            assert_eq!(
                &candidate.location.to_string(),
                "No object files listed on this source"
            );
        }
    }
}

#[tokio::test]
async fn test_no_permission() {
    let (symbolication, _cache_dir) = setup_service(|_| ()).await;

    let server = FailingSymbolServer::new();

    let sources = vec![
        server.forbidden_source.clone(),
        // NOTE: The bucket `symbolicator-test` needs to actually exist for this test to fail with
        // a permissions error.
        serde_json::from_str(
            r#"{
              "id": "invalid-s3",
              "type": "s3",
              "filters": {"filetypes": ["mach_code"]},
              "bucket": "symbolicator-test",
              "region": "us-east-1"
            }"#,
        )
        .unwrap(),
        serde_json::from_str(
            r#"{
              "id": "invalid-gcs",
              "type": "gcs",
              "filters": {"filetypes": ["mach_code"]},
              "bucket": "honk",
              "private_key": "",
              "client_email": "honk@sentry.io"
            }"#,
        )
        .unwrap(),
    ];

    let request = get_symbolication_request(sources);
    let mut response = symbolication.symbolicate(request).await.unwrap();
    let candidates = response.modules.pop().unwrap().candidates.0;

    // NOTE: every second candidate is a "No object files listed on this source" one for the
    // source bundle lookup

    assert_eq!(candidates[1].source, SourceId::new("forbidden"));
    assert_eq!(
        candidates[1].download,
        ObjectDownloadInfo::NoPerm { details: "".into() }
    );

    assert_eq!(candidates[3].source, SourceId::new("invalid-gcs"));
    assert_eq!(
        &candidates[3].location.to_string(),
        "gs://honk/502F/C0A5/1EC1/3E47/9998/684FA139DCA7.app"
    );
    // NOTE: with invalid credentials, GCS will rather raise a download error
    assert_eq!(
        candidates[3].download,
        ObjectDownloadInfo::Error {
            details: "download failed: failed to fetch data from GCS: failed encoding JWT".into()
        }
    );

    assert_eq!(candidates[5].source, SourceId::new("invalid-s3"));
    assert_eq!(
        &candidates[5].location.to_string(),
        "s3://symbolicator-test/502F/C0A5/1EC1/3E47/9998/684FA139DCA7.app"
    );
    assert_eq!(
        candidates[5].download,
        ObjectDownloadInfo::NoPerm { details: "".into() }
    );
}

#[tokio::test]
async fn test_reserved_ip_addresses() {
    let server = FailingSymbolServer::new();

    let files_config = CommonSourceConfig {
        filters: SourceFilters {
            filetypes: vec![FileType::MachCode],
            path_patterns: vec![],
        },
        layout: Default::default(),
        is_public: false,
    };

    let mut sources = Vec::with_capacity(3);
    let mut url = server.server.url("not-found/");

    url.set_host(Some("dev.getsentry.net")).unwrap();
    sources.push(SourceConfig::Http(Arc::new(HttpSourceConfig {
        id: SourceId::new("getsentry"),
        url: url.clone(),
        headers: Default::default(),
        files: files_config.clone(),
    })));

    url.set_host(Some("127.0.0.1")).unwrap();
    sources.push(SourceConfig::Http(Arc::new(HttpSourceConfig {
        id: SourceId::new("ip"),
        url: url.clone(),
        headers: Default::default(),
        files: files_config.clone(),
    })));

    url.set_host(Some("localhost")).unwrap();
    sources.push(SourceConfig::Http(Arc::new(HttpSourceConfig {
        id: SourceId::new("localhost"),
        url,
        headers: Default::default(),
        files: files_config.clone(),
    })));

    let request = get_symbolication_request(sources);

    let (symbolication, _cache_dir) = setup_service(|_| ()).await;
    let mut response = symbolication.symbolicate(request.clone()).await.unwrap();
    let candidates = response.modules.pop().unwrap().candidates.0;

    assert_eq!(server.accesses(), 3);

    // NOTE: every second candidate is a "No object files listed on this source" one for the
    // source bundle lookup

    assert_eq!(candidates[1].source, SourceId::new("getsentry"));
    assert!(candidates[1]
        .location
        .to_string()
        .starts_with("http://dev.getsentry.net"));
    assert_eq!(candidates[1].download, ObjectDownloadInfo::NotFound);

    assert_eq!(candidates[3].source, SourceId::new("ip"));
    assert!(candidates[3]
        .location
        .to_string()
        .starts_with("http://127.0.0.1"));
    assert_eq!(candidates[3].download, ObjectDownloadInfo::NotFound);

    assert_eq!(candidates[5].source, SourceId::new("localhost"));
    assert!(candidates[5]
        .location
        .to_string()
        .starts_with("http://localhost"));
    assert_eq!(candidates[5].download, ObjectDownloadInfo::NotFound);

    // ---

    let (symbolication, _cache_dir) = setup_service(|cfg| {
        cfg.connect_to_reserved_ips = false;
    })
    .await;
    let mut response = symbolication.symbolicate(request.clone()).await.unwrap();
    let candidates = response.modules.pop().unwrap().candidates.0;

    assert_eq!(server.accesses(), 0);

    // NOTE: every second candidate is a "No object files listed on this source" one for the
    // source bundle lookup
    let error = ObjectDownloadInfo::Error {
        details: "download failed: destination is restricted".into(),
    };

    assert_eq!(candidates[1].source, SourceId::new("getsentry"));
    assert!(candidates[1]
        .location
        .to_string()
        .starts_with("http://dev.getsentry.net"));
    assert_eq!(candidates[1].download, error);

    assert_eq!(candidates[3].source, SourceId::new("ip"));
    assert!(candidates[3]
        .location
        .to_string()
        .starts_with("http://127.0.0.1"));
    assert_eq!(candidates[3].download, error);

    assert_eq!(candidates[5].source, SourceId::new("localhost"));
    assert!(candidates[5]
        .location
        .to_string()
        .starts_with("http://localhost"));
    assert_eq!(candidates[5].download, error);
}
