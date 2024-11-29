use std::io::Write;
use std::sync::Arc;

use tempfile::NamedTempFile;

use symbolicator_service::types::Scope;

use crate::{assert_snapshot, read_fixture, setup_service, symbol_server};

macro_rules! stackwalk_minidump {
    ($path:expr) => {
        async {
            let (symbolication, _cache_dir) = setup_service(|_| ());
            let (_symsrv, source) = symbol_server();

            let minidump = read_fixture($path);
            let mut minidump_file = NamedTempFile::new().unwrap();
            minidump_file.write_all(&minidump).unwrap();
            let response = symbolication
                .process_minidump(
                    None,
                    Scope::Global,
                    minidump_file.into_temp_path(),
                    Arc::new([source]),
                    Default::default(),
                )
                .await;

            assert_snapshot!(response.unwrap());
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
