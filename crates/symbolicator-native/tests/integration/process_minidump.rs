use std::io::Write;
use std::sync::Arc;

use tempfile::tempfile;

use symbolicator_native::interface::{AttachmentFile, ProcessMinidump};
use symbolicator_service::types::Scope;

use crate::{assert_snapshot, read_fixture, setup_service, symbol_server};

macro_rules! stackwalk_minidump {
    ($path:expr) => {
        async {
            let (symbolication, _cache_dir) = setup_service(|_| ());
            let (_symsrv, source) = symbol_server();

            let minidump = read_fixture($path);
            let mut minidump_file = tempfile().unwrap();
            minidump_file.write_all(&minidump).unwrap();
            let response = symbolication
                .process_minidump(ProcessMinidump {
                    platform: None,
                    scope: Scope::Global,
                    minidump_file: AttachmentFile::Local(minidump_file),
                    sources: Arc::new([source]),
                    scraping: Default::default(),
                    rewrite_first_module: Default::default(),
                })
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
