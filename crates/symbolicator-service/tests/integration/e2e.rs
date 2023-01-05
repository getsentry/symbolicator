use std::path::Path;
use std::sync::Arc;

use symbolicator_service::services::symbolication::{StacktraceOrigin, SymbolicateStacktraces};
use symbolicator_service::types::{
    FrameStatus, ObjectDownloadInfo, ObjectFileStatus, RawObjectInfo, RawStacktrace, Scope,
};
use symbolicator_sources::{
    DirectoryLayoutType, FileType, FilesystemSourceConfig, HttpSourceConfig, RemoteFileUri,
    SentrySourceConfig, SourceConfig, SourceId,
};
use symbolicator_test::{fixture, source_config, HitCounter};

use crate::assert_snapshot;
use crate::symbolication::{get_symbolication_request, setup_service};

// FIXME: Using this fixture in combination with the public microsoft symbol server means that
// we are downloading multiple of megabytes on each test run, which we should ideally avoid by
// testing with a local http source instead.
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

/// tests that nothing happens when no source is supplied
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

/// Tests that source file types are correctly filtered
#[tokio::test]
async fn test_sources_filetypes() {
    let (symbolication, _cache_dir) = setup_service(|_| ()).await;

    let (modules, stacktraces) = request_fixture();

    // This symbol source is filtering for only `mach_code` files.
    let hitcounter = HitCounter::new();

    let request = SymbolicateStacktraces {
        modules: modules.into_iter().map(From::from).collect(),
        stacktraces,
        signal: None,
        origin: StacktraceOrigin::Symbolicate,
        sources: Arc::new([hitcounter.source("not-found", "/respond_statuscode/404/")]),
        scope: Default::default(),
    };

    let response = symbolication.symbolicate(request).await;

    assert_eq!(hitcounter.accesses(), 0);

    assert_snapshot!(response.unwrap());
}

/// tests that sourcce `path_patterns` work as expected
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
        let mut files = source_config(DirectoryLayoutType::Symstore, vec![FileType::Pdb]);
        files.filters.path_patterns = path_patterns;

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

/// Tests permission errors for http, s3 and gcs sources
#[tokio::test]
async fn test_no_permission() {
    let (symbolication, _cache_dir) = setup_service(|_| ()).await;

    let hitcounter = HitCounter::new();

    let sources = vec![
        hitcounter.source("forbidden", "/respond_statuscode/403/"),
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

/// Tests that the `connect_to_reserved_ips` option correctly restricts
/// IP addresses and resolved hostnames, both local and public, as `dev.getsentry.net` resolves
/// to `127.0.0.1`.
#[tokio::test]
async fn test_reserved_ip_addresses() {
    let hitcounter = HitCounter::new();

    let files = source_config(DirectoryLayoutType::Native, vec![FileType::MachCode]);

    let mut sources = Vec::with_capacity(3);
    let mut url = hitcounter.url("not-found/");

    url.set_host(Some("dev.getsentry.net")).unwrap();
    sources.push(SourceConfig::Http(Arc::new(HttpSourceConfig {
        id: SourceId::new("getsentry"),
        url: url.clone(),
        headers: Default::default(),
        files: files.clone(),
    })));

    url.set_host(Some("127.0.0.1")).unwrap();
    sources.push(SourceConfig::Http(Arc::new(HttpSourceConfig {
        id: SourceId::new("ip"),
        url: url.clone(),
        headers: Default::default(),
        files: files.clone(),
    })));

    url.set_host(Some("localhost")).unwrap();
    sources.push(SourceConfig::Http(Arc::new(HttpSourceConfig {
        id: SourceId::new("localhost"),
        url,
        headers: Default::default(),
        files,
    })));

    let request = get_symbolication_request(sources);

    let (symbolication, _cache_dir) = setup_service(|_| ()).await;
    let mut response = symbolication.symbolicate(request.clone()).await.unwrap();
    let candidates = response.modules.pop().unwrap().candidates.0;

    assert_eq!(hitcounter.accesses(), 3);

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

    assert_eq!(hitcounter.accesses(), 0);

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

/// Tests that symbolicator correctly follows redirects
#[tokio::test]
async fn test_redirects() {
    let (symbolication, _cache_dir) = setup_service(|_| ()).await;

    let (modules, stacktraces) = request_fixture();

    let hitcounter = HitCounter::new();
    let config = source_config(DirectoryLayoutType::Symstore, vec![FileType::Pdb]);
    let source = hitcounter.source_with_config("hitcounter", "redirect/msdl", config);

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
    let expected_url = hitcounter
        .url("/redirect/msdl/wkernel32.pdb/FF9F9F7841DB88F0CDEDA9E1E9BFF3B51/wkernel32.pdb");
    assert_eq!(candidate.location, RemoteFileUri::from(expected_url));
    assert!(matches!(candidate.download, ObjectDownloadInfo::Ok { .. }));
}

/// Tests what candidate info we get for different response codes
#[tokio::test]
async fn test_unreachable_bucket() {
    let (symbolication, _cache_dir) = setup_service(|_| ()).await;

    let (modules, stacktraces) = request_fixture();

    let hitcounter = HitCounter::new();

    let tys = ["http", "sentry"];
    let codes = ["400", "500", "404"];

    for ty in tys {
        for code in codes {
            let source = if ty == "http" {
                let config = source_config(DirectoryLayoutType::Symstore, vec![FileType::Pdb]);
                hitcounter.source_with_config(
                    &format!("broken-{}-{}", ty, code),
                    &format!("respond_statuscode/{code}"),
                    config,
                )
            } else {
                SourceConfig::Sentry(Arc::new(SentrySourceConfig {
                    id: SourceId::new(format!("broken-{}-{}", ty, code)),
                    url: hitcounter.url(&format!("respond_statuscode/{code}")),
                    token: "123abc".into(),
                }))
            };

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
            let statuses = (module.debug_status, candidate.download);

            if ty == "http" && code == "500" {
                assert_eq!(
                    statuses,
                    (
                        ObjectFileStatus::FetchingFailed,
                        ObjectDownloadInfo::Error {
                            details: "download failed: 500 Internal Server Error".into()
                        }
                    )
                );
            } else {
                assert_eq!(
                    statuses,
                    (ObjectFileStatus::Missing, ObjectDownloadInfo::NotFound)
                );
            }
        }
    }
}

/// Tests request coalescing and effect of caches
#[tokio::test]
async fn test_lookup_deduplication() {
    let (symbolication, _cache_dir) = setup_service(|_| ()).await;

    let (modules, stacktraces) = request_fixture();

    let hitcounter = HitCounter::new();

    let mut config = source_config(DirectoryLayoutType::Symstore, vec![FileType::Pdb]);

    for is_public in [true, false] {
        config.is_public = is_public;
        let source =
            hitcounter.source_with_config(&format!("test-{}", is_public), "msdl/", config.clone());

        let requests = (0..20).map(|_| {
            let modules = modules.iter().cloned().map(From::from).collect();
            let stacktraces = stacktraces.clone();
            let symbolication = symbolication.clone();
            let source = source.clone();
            async move {
                let request = SymbolicateStacktraces {
                    modules,
                    stacktraces,
                    signal: None,
                    origin: StacktraceOrigin::Symbolicate,
                    sources: Arc::new([source]),
                    scope: Default::default(),
                };

                symbolication.symbolicate(request).await.unwrap()
            }
        });

        let responses = futures::future::join_all(requests).await;
        for res in responses {
            assert_eq!(
                res.stacktraces[0].frames[0].status,
                FrameStatus::Symbolicated
            );
        }

        let hits = hitcounter.all_hits();
        // the caches and request coalescing should make sure we only ever request things once
        assert_eq!(
            &hits,
            &[
                (
                    "/msdl/wkernel32.pdb/FF9F9F7841DB88F0CDEDA9E1E9BFF3B51/wkernel32.pd_".into(),
                    1
                ),
                (
                    "/msdl/wkernel32.pdb/FF9F9F7841DB88F0CDEDA9E1E9BFF3B51/wkernel32.pdb".into(),
                    1
                )
            ]
        );
    }
}

fn get_files(path: impl AsRef<Path>) -> Vec<(String, u64)> {
    let mut files = vec![];
    for entry in std::fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().to_string();
        let size = entry.metadata().unwrap().len();

        files.push((name, size));
    }
    files.sort();
    files
}

/// Tests caching side-effects, like cache files written and hits to the symbol source.
#[tokio::test]
async fn test_basic_windows() {
    let (modules, stacktraces) = request_fixture();

    let hitcounter = HitCounter::new();

    let mut config = source_config(DirectoryLayoutType::Symstore, vec![FileType::Pdb]);

    for is_public in [true, false] {
        for with_cache in [true, false] {
            config.is_public = is_public;
            let source = hitcounter.source_with_config("microsoft", "msdl/", config.clone());

            let (symbolication, cache_dir) = setup_service(|cfg| {
                if !with_cache {
                    cfg.cache_dir = None;
                }
            })
            .await;

            let request = SymbolicateStacktraces {
                modules: modules.iter().cloned().map(From::from).collect(),
                stacktraces: stacktraces.clone(),
                signal: None,
                origin: StacktraceOrigin::Symbolicate,
                sources: Arc::new([source]),
                scope: Scope::Scoped("myscope".into()),
            };

            for i in 0..2 {
                let response = symbolication.symbolicate(request.clone()).await.unwrap();
                assert_eq!(
                    response.stacktraces[0].frames[0].status,
                    FrameStatus::Symbolicated
                );

                if with_cache {
                    let cached_scope = if is_public { "global" } else { "myscope" };

                    let cached_objects = cache_dir.path().join("objects").join(cached_scope);
                    let cached_objects = get_files(cached_objects);
                    assert_eq!(&cached_objects, &[
                        ("microsoft_wkernel32_pdb_FF9F9F7841DB88F0CDEDA9E1E9BFF3B51_wkernel32_pd_".into(), 0),
                        ("microsoft_wkernel32_pdb_FF9F9F7841DB88F0CDEDA9E1E9BFF3B51_wkernel32_pdb".into(), 846_848)
                    ]);

                    let cached_symcaches = cache_dir
                        .path()
                        .join("symcaches")
                        .join("5") // NOTE: this needs to be updated when bumping symcaches
                        .join(cached_scope);
                    let cached_symcaches = get_files(cached_symcaches);
                    assert_eq!(&cached_symcaches, &[
                        ("microsoft_wkernel32_pdb_FF9F9F7841DB88F0CDEDA9E1E9BFF3B51_wkernel32_pdb".into(), 142_365)
                    ]);
                }

                if i > 0 && with_cache {
                    assert_eq!(hitcounter.accesses(), 0);
                } else {
                    let hits = hitcounter.all_hits();
                    let (hit_count, miss_count) = if with_cache {
                        (1, 1)
                    } else {
                        // we are downloading twice: once for the objects_meta request, and once
                        // again for the objects/symcache request
                        (2, 1)
                    };

                    assert_eq!(&hits, &[
                        ("/msdl/wkernel32.pdb/FF9F9F7841DB88F0CDEDA9E1E9BFF3B51/wkernel32.pd_".into(), miss_count),
                        ("/msdl/wkernel32.pdb/FF9F9F7841DB88F0CDEDA9E1E9BFF3B51/wkernel32.pdb".into(), hit_count),
                    ]);
                }
            }
        }
    }
}
