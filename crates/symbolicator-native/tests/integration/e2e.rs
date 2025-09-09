use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use symbolicator_native::interface::{FrameStatus, ProcessMinidump, SymbolicateStacktraces};
use symbolicator_service::caching::Metadata;
use symbolicator_service::objects::ObjectDownloadInfo;
use symbolicator_service::types::{ObjectFileStatus, Scope};
use symbolicator_sources::{
    CommonSourceConfig, DirectoryLayout, DirectoryLayoutType, FileType, FilesystemSourceConfig,
    HttpSourceConfig, RemoteFileUri, SentrySourceConfig, SentryToken, SourceConfig, SourceId,
};
use symbolicator_test::read_fixture;
use tempfile::NamedTempFile;

use crate::{
    Server, assert_snapshot, example_request, fixture, make_symbolication_request, setup_service,
    source_config,
};

// FIXME: Using this fixture in combination with the public microsoft symbol server means that
// we are downloading multiple of megabytes on each test run, which we should ideally avoid by
// testing with a local http source instead.
fn request_fixture(sources: Vec<SourceConfig>) -> SymbolicateStacktraces {
    make_symbolication_request(
        sources,
        r#"[{
          "type": "pe",
          "debug_id": "ff9f9f78-41db-88f0-cded-a9e1e9bff3b5-1",
          "code_file": "C:\\Windows\\System32\\kernel32.dll",
          "debug_file": "C:\\Windows\\System32\\wkernel32.pdb",
          "image_addr": "0x749d0000",
          "image_size": 851968
        }]"#,
        r#"[{
          "registers": {"eip": "0x0000000001509530"},
          "frames": [{"instruction_addr": "0x749e8630"}]
        }]"#,
    )
}

/// tests that nothing happens when no source is supplied
#[tokio::test]
async fn test_no_sources() {
    let (symbolication, cache_dir) = setup_service(|_| ());

    let request = request_fixture(vec![]);
    let response = symbolication.symbolicate(request).await;

    assert_snapshot!(response.unwrap());

    assert!(!cache_dir.path().join("objects/global").exists());
    assert!(!cache_dir.path().join("symcaches/global").exists());
}

/// Tests that source file types are correctly filtered
#[tokio::test]
async fn test_sources_filetypes() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    // This symbol source is filtering for only `mach_code` files.
    let hitcounter = Server::new();

    let request = request_fixture(vec![
        hitcounter.source("not-found", "/respond_statuscode/404/"),
    ]);
    let response = symbolication.symbolicate(request).await;

    assert_eq!(hitcounter.accesses(), 0);

    assert_snapshot!(response.unwrap());
}

/// tests that source `path_patterns` work as expected
#[tokio::test]
async fn test_path_patterns() {
    let (symbolication, _cache_dir) = setup_service(|_| ());

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

        let request = request_fixture(vec![source]);
        let mut response = symbolication.symbolicate(request).await.unwrap();

        let module = response.modules.pop().unwrap();
        let candidate = module.candidates.into_inner().pop().unwrap();

        if should_be_found {
            assert!(
                candidate
                    .location
                    .to_string()
                    .ends_with("wkernel32.pdb/FF9F9F7841DB88F0CDEDA9E1E9BFF3B51/wkernel32.pdb")
            );
        } else {
            assert_eq!(
                &candidate.location.to_string(),
                "No object files listed on this source"
            );
        }
    }
}

// Tests that a symstore index works as expected by attempting to
// symbolicate a minidump.
#[tokio::test]
async fn test_minidump_symstore_index() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let server = Server::new();

    let files = CommonSourceConfig {
        layout: DirectoryLayout {
            ty: DirectoryLayoutType::Symstore,
            ..Default::default()
        },
        has_index: true,
        ..Default::default()
    };

    let source = SourceConfig::Http(Arc::new(HttpSourceConfig {
        id: SourceId::new("local"),
        url: server.url("symbols/"),
        headers: Default::default(),
        files,
        accept_invalid_certs: false,
    }));

    let minidump = read_fixture("windows.dmp");
    let mut minidump_file = NamedTempFile::new().unwrap();
    minidump_file.write_all(&minidump).unwrap();
    let mut response = symbolication
        .process_minidump(ProcessMinidump {
            platform: None,
            scope: Scope::Global,
            minidump_file: minidump_file.into_temp_path(),
            sources: Arc::new([source]),
            scraping: Default::default(),
            rewrite_first_module: Default::default(),
        })
        .await
        .unwrap();

    // We only care about modules we actually tried to fetch here
    let mut modules = std::mem::take(&mut response.modules);
    modules.retain(|m| m.debug_status != ObjectFileStatus::Unused);

    assert_snapshot!(modules);
}

/// Tests permission errors for http, s3 and gcs sources
#[tokio::test]
async fn test_no_permission() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let hitcounter = Server::new();

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

    let request = example_request(sources);
    let mut response = symbolication.symbolicate(request).await.unwrap();
    let candidates = response.modules.pop().unwrap().candidates.into_inner();

    // NOTE: every second candidate is a "No object files listed on this source" one for the
    // source bundle lookup

    assert_eq!(candidates[1].source, SourceId::new("forbidden"));
    assert_eq!(
        candidates[1].download,
        ObjectDownloadInfo::NoPerm {
            details: "403 Forbidden".into()
        }
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
            details: "download failed: failed encoding JWT".into()
        }
    );

    assert_eq!(candidates[5].source, SourceId::new("invalid-s3"));
    assert_eq!(
        &candidates[5].location.to_string(),
        "s3://symbolicator-test/502F/C0A5/1EC1/3E47/9998/684FA139DCA7.app"
    );
    assert_eq!(
        candidates[5].download,
        ObjectDownloadInfo::Error {
            details: "download failed: unhandled error".to_owned()
        }
    );
}

/// Tests that the `connect_to_reserved_ips` option correctly restricts
/// IP addresses and resolved hostnames, both local and public, as `dev.getsentry.net` resolves
/// to `127.0.0.1`.
#[tokio::test]
async fn test_reserved_ip_addresses() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let hitcounter = Server::new();

    let files = source_config(DirectoryLayoutType::Native, vec![FileType::MachCode]);

    let mut sources = Vec::with_capacity(3);
    let mut url = hitcounter.url("not-found/");

    url.set_host(Some("dev.getsentry.net")).unwrap();
    sources.push(SourceConfig::Http(Arc::new(HttpSourceConfig {
        id: SourceId::new("getsentry"),
        url: url.clone(),
        headers: Default::default(),
        files: files.clone(),
        accept_invalid_certs: false,
    })));

    url.set_host(Some("127.0.0.1")).unwrap();
    sources.push(SourceConfig::Http(Arc::new(HttpSourceConfig {
        id: SourceId::new("ip"),
        url: url.clone(),
        headers: Default::default(),
        files: files.clone(),
        accept_invalid_certs: false,
    })));

    url.set_host(Some("localhost")).unwrap();
    sources.push(SourceConfig::Http(Arc::new(HttpSourceConfig {
        id: SourceId::new("localhost"),
        url,
        headers: Default::default(),
        files,
        accept_invalid_certs: false,
    })));

    let request = example_request(sources);
    let mut response = symbolication.symbolicate(request.clone()).await.unwrap();
    let candidates = response.modules.pop().unwrap().candidates.into_inner();

    assert_eq!(hitcounter.accesses(), 3);

    // NOTE: every second candidate is a "No object files listed on this source" one for the
    // source bundle lookup

    assert_eq!(candidates[1].source, SourceId::new("getsentry"));
    assert!(
        candidates[1]
            .location
            .to_string()
            .starts_with("http://dev.getsentry.net")
    );
    assert_eq!(candidates[1].download, ObjectDownloadInfo::NotFound);

    assert_eq!(candidates[3].source, SourceId::new("ip"));
    assert!(
        candidates[3]
            .location
            .to_string()
            .starts_with("http://127.0.0.1")
    );
    assert_eq!(candidates[3].download, ObjectDownloadInfo::NotFound);

    assert_eq!(candidates[5].source, SourceId::new("localhost"));
    assert!(
        candidates[5]
            .location
            .to_string()
            .starts_with("http://localhost")
    );
    assert_eq!(candidates[5].download, ObjectDownloadInfo::NotFound);

    // ---

    let (symbolication, _cache_dir) = setup_service(|cfg| {
        cfg.connect_to_reserved_ips = false;
    });
    let mut response = symbolication.symbolicate(request.clone()).await.unwrap();
    let candidates = response.modules.pop().unwrap().candidates.into_inner();

    assert_eq!(hitcounter.accesses(), 0);

    // NOTE: every second candidate is a "No object files listed on this source" one for the
    // source bundle lookup
    let error = ObjectDownloadInfo::Error {
        details: "download failed: destination is restricted".into(),
    };

    assert_eq!(candidates[1].source, SourceId::new("getsentry"));
    assert!(
        candidates[1]
            .location
            .to_string()
            .starts_with("http://dev.getsentry.net")
    );
    assert_eq!(candidates[1].download, error);

    assert_eq!(candidates[3].source, SourceId::new("ip"));
    assert!(
        candidates[3]
            .location
            .to_string()
            .starts_with("http://127.0.0.1")
    );
    assert_eq!(candidates[3].download, error);

    assert_eq!(candidates[5].source, SourceId::new("localhost"));
    assert!(
        candidates[5]
            .location
            .to_string()
            .starts_with("http://localhost")
    );
    assert_eq!(candidates[5].download, error);
}

/// Tests that symbolicator correctly follows redirects
#[tokio::test]
async fn test_redirects() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let hitcounter = Server::new();

    let config = source_config(DirectoryLayoutType::Symstore, vec![FileType::Pdb]);
    let source = hitcounter.source_with_config("hitcounter", "redirect/msdl", config);

    let request = request_fixture(vec![source]);
    let mut response = symbolication.symbolicate(request).await.unwrap();

    let module = response.modules.pop().unwrap();
    let candidate = module.candidates.into_inner().pop().unwrap();
    let expected_url = hitcounter
        .url("/redirect/msdl/wkernel32.pdb/FF9F9F7841DB88F0CDEDA9E1E9BFF3B51/wkernel32.pdb");
    assert_eq!(
        candidate.location,
        RemoteFileUri::from(expected_url.as_str())
    );
    assert!(matches!(candidate.download, ObjectDownloadInfo::Ok { .. }));
}

/// Tests what candidate info we get for different response codes
#[tokio::test]
async fn test_unreachable_bucket() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let hitcounter = Server::new();

    let tys = ["http", "sentry"];
    let codes = ["400", "500", "404"];

    for ty in tys {
        for code in codes {
            let source = if ty == "http" {
                let config = source_config(DirectoryLayoutType::Symstore, vec![FileType::Pdb]);
                hitcounter.source_with_config(
                    &format!("broken-{ty}-{code}"),
                    &format!("respond_statuscode/{code}"),
                    config,
                )
            } else {
                SourceConfig::Sentry(Arc::new(SentrySourceConfig {
                    id: SourceId::new(format!("broken-{ty}-{code}")),
                    url: hitcounter.url(&format!("respond_statuscode/{code}")),
                    token: SentryToken("123abc".to_owned()),
                }))
            };

            let request = request_fixture(vec![source]);
            let mut response = symbolication.symbolicate(request).await.unwrap();

            let module = response.modules.pop().unwrap();
            let candidate = module.candidates.into_inner().pop().unwrap();
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
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let hitcounter = Server::new();

    let mut config = source_config(DirectoryLayoutType::Symstore, vec![FileType::Pdb]);

    for is_public in [true, false] {
        config.is_public = is_public;
        let source =
            hitcounter.source_with_config(&format!("test-{is_public}"), "msdl/", config.clone());
        let request = request_fixture(vec![source]);

        let requests = (0..20).map(|_| {
            let symbolication = symbolication.clone();
            let request = request.clone();
            async move { symbolication.symbolicate(request).await.unwrap() }
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

fn get_files_recursive(files: &mut Vec<(String, u64)>, root: &Path, path: &Path) {
    for entry in std::fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let ty = entry.file_type().unwrap();
        if ty.is_dir() {
            get_files_recursive(files, root, &path);
        } else if ty.is_file() {
            let name = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let size = entry.metadata().unwrap().len();
            files.push((name, size));
        }
        // we ignore symlinks
    }
}

fn get_cache_files(root: &Path) -> Vec<(String, u64)> {
    let mut files = vec![];

    get_files_recursive(&mut files, root, root);

    files.sort();
    files
}

/// Tests caching side-effects, like cache files written and hits to the symbol source.
#[tokio::test]
async fn test_basic_windows() {
    let hitcounter = Server::new();

    let mut config = source_config(DirectoryLayoutType::Symstore, vec![FileType::Pdb]);

    for is_public in [true, false] {
        for with_cache in [true, false] {
            config.is_public = is_public;
            let source = hitcounter.source_with_config("microsoft", "msdl/", config.clone());

            let (symbolication, cache_dir) = setup_service(|cfg| {
                if !with_cache {
                    cfg.cache_dir = None;
                }
            });

            let mut request = request_fixture(vec![source]);
            request.scope = Scope::Scoped("myscope".into());

            for i in 0..2 {
                let response = symbolication.symbolicate(request.clone()).await.unwrap();
                assert_eq!(
                    response.stacktraces[0].frames[0].status,
                    FrameStatus::Symbolicated
                );

                if with_cache {
                    let objects_dir = cache_dir.path().join("objects");
                    let mut cached_objects = get_cache_files(&objects_dir);

                    // NOTE: the cache key depends on the exact location of the file, which is
                    // random because it includes the [`Server`]s random port.
                    cached_objects.sort_by_key(|(_, size)| *size);
                    assert_eq!(cached_objects.len(), 4); // 2 filename patterns, 2 metadata files
                    assert_eq!(cached_objects[0].1, 0);
                    assert_eq!(cached_objects[3].1, 846_848);

                    // Checks the metadata file
                    let metadata_file = &cached_objects[1].0;
                    let cached_scope = if is_public { "global" } else { "myscope" };
                    let mut expected_debug = format!(
                        "scope: {cached_scope}\n\nsource: microsoft\nlocation: {}",
                        hitcounter.url(
                            "/msdl/wkernel32.pdb/FF9F9F7841DB88F0CDEDA9E1E9BFF3B51/wkernel32.pd"
                        )
                    );
                    let file = File::open(objects_dir.join(metadata_file)).unwrap();
                    let metadata: Metadata = serde_json::from_reader(&file).unwrap();
                    assert_eq!(metadata.scope.as_ref(), cached_scope);
                    assert!(
                        metadata
                            .debug
                            .as_ref()
                            .unwrap()
                            .key_data
                            .starts_with(&expected_debug),
                        "{metadata:?} == {expected_debug:?}"
                    );

                    let symcaches_dir = cache_dir.path().join("symcaches");
                    let mut cached_symcaches = get_cache_files(&symcaches_dir);

                    cached_symcaches.sort_by_key(|(_, size)| *size);
                    assert_eq!(cached_symcaches.len(), 2); // 1 symcache, 1 metadata file, 1 debug text file
                    assert_eq!(cached_symcaches[1].1, 142_581);

                    // Checks the metadata file
                    expected_debug.push_str("b\n"); // this truly ends in `.pdb` now
                    let metadata_file = &cached_symcaches[0].0;
                    let file = File::open(symcaches_dir.join(metadata_file)).unwrap();
                    let metadata: Metadata = serde_json::from_reader(&file).unwrap();
                    assert_eq!(metadata.scope.as_ref(), cached_scope);
                    assert_eq!(
                        &metadata.debug.as_ref().unwrap().key_data[..],
                        &expected_debug[..]
                    );
                }

                // our use of in-memory caching should make sure we only ever request each file once
                if i > 0 {
                    assert_eq!(hitcounter.accesses(), 0);
                } else {
                    let hits = hitcounter.all_hits();
                    assert_eq!(&hits, &[
                        ("/msdl/wkernel32.pdb/FF9F9F7841DB88F0CDEDA9E1E9BFF3B51/wkernel32.pd_".into(), 1),
                        ("/msdl/wkernel32.pdb/FF9F9F7841DB88F0CDEDA9E1E9BFF3B51/wkernel32.pdb".into(), 1),
                    ]);
                }
            }
        }
    }
}
