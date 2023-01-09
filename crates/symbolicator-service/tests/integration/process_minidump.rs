use std::fs;
use std::io::Write;
use std::sync::Arc;

use tempfile::NamedTempFile;

use symbolicator_service::types::Scope;
use symbolicator_test as test;
use symbolicator_test::assert_snapshot;

use crate::symbolication::setup_service;

macro_rules! stackwalk_minidump {
    ($path:expr) => {
        async {
            let (symbolication, cache_dir) = setup_service(|_| ()).await;
            let (_symsrv, source) = test::symbol_server();

            let minidump = test::read_fixture($path);
            let mut minidump_file = NamedTempFile::new().unwrap();
            minidump_file.write_all(&minidump).unwrap();
            let response = symbolication
                .process_minidump(
                    Scope::Global,
                    minidump_file.into_temp_path(),
                    Arc::new([source]),
                )
                .await;

            assert_snapshot!(response.unwrap());

            let global_dir = cache_dir.path().join("object_meta/global");
            let mut cache_entries: Vec<_> = fs::read_dir(global_dir)
                .unwrap()
                .map(|x| x.unwrap().file_name().into_string().unwrap())
                .collect();

            cache_entries.sort();
            assert_snapshot!(cache_entries);
        }
    };
}

#[tokio::test]
async fn test_minidump_windows() {
    stackwalk_minidump!("windows.dmp").await
}

#[tokio::test]
async fn test_minidump_macos() {
    stackwalk_minidump!("macos.dmp").await
}

#[tokio::test]
async fn test_minidump_linux() {
    stackwalk_minidump!("linux.dmp").await
}
