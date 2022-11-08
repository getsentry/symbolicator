use symbolicator_sources::ObjectId;

use crate::services::cficaches::{CfiCacheActor, CfiCacheError};
use crate::services::objects::ObjectsActor;
use crate::services::ppdb_caches::{PortablePdbCacheActor, PortablePdbCacheError};
use crate::services::symcaches::{SymCacheActor, SymCacheError};
use crate::types::{ObjectFileStatus, RawObjectInfo};

mod apple;
mod module_lookup;
mod process_minidump;
// we should really rename this here to the `SymbolicatorService`, as it does a lot more
// than just symbolication ;-)
#[allow(clippy::module_inception)]
mod symbolication;

pub use symbolication::{StacktraceOrigin, SymbolicateStacktraces};

#[derive(Clone, Debug)]
pub struct SymbolicationActor {
    objects: ObjectsActor,
    symcaches: SymCacheActor,
    cficaches: CfiCacheActor,
    ppdb_caches: PortablePdbCacheActor,
    diagnostics_cache: crate::cache::Cache,
}

impl SymbolicationActor {
    pub fn new(
        objects: ObjectsActor,
        symcaches: SymCacheActor,
        cficaches: CfiCacheActor,
        ppdb_caches: PortablePdbCacheActor,
        diagnostics_cache: crate::cache::Cache,
    ) -> Self {
        SymbolicationActor {
            objects,
            symcaches,
            cficaches,
            ppdb_caches,
            diagnostics_cache,
        }
    }
}

impl From<&CfiCacheError> for ObjectFileStatus {
    fn from(e: &CfiCacheError) -> ObjectFileStatus {
        match e {
            CfiCacheError::Fetching(_) => ObjectFileStatus::FetchingFailed,
            // nb: Timeouts during download are also caught by Fetching
            CfiCacheError::Timeout => ObjectFileStatus::Timeout,
            CfiCacheError::ObjectParsing(_) => ObjectFileStatus::Malformed,

            _ => {
                // Just in case we didn't handle an error properly,
                // capture it here. If an error was captured with
                // `capture_error` further down in the callstack, it
                // should be explicitly handled here as a
                // SymCacheError variant.
                sentry::capture_error(e);
                ObjectFileStatus::Other
            }
        }
    }
}

impl From<&SymCacheError> for ObjectFileStatus {
    fn from(e: &SymCacheError) -> ObjectFileStatus {
        match e {
            SymCacheError::Fetching(_) => ObjectFileStatus::FetchingFailed,
            // nb: Timeouts during download are also caught by Fetching
            SymCacheError::Timeout => ObjectFileStatus::Timeout,
            SymCacheError::Malformed => ObjectFileStatus::Malformed,
            SymCacheError::ObjectParsing(_) => ObjectFileStatus::Malformed,
            _ => {
                // Just in case we didn't handle an error properly,
                // capture it here. If an error was captured with
                // `capture_error` further down in the callstack, it
                // should be explicitly handled here as a
                // SymCacheError variant.
                sentry::capture_error(e);
                ObjectFileStatus::Other
            }
        }
    }
}

impl From<&PortablePdbCacheError> for ObjectFileStatus {
    fn from(e: &PortablePdbCacheError) -> ObjectFileStatus {
        match e {
            PortablePdbCacheError::Fetching(_) => ObjectFileStatus::FetchingFailed,
            // nb: Timeouts during download are also caught by Fetching
            PortablePdbCacheError::Timeout => ObjectFileStatus::Timeout,
            PortablePdbCacheError::Malformed => ObjectFileStatus::Malformed,
            PortablePdbCacheError::PortablePdbParsing(_) => ObjectFileStatus::Malformed,
            _ => {
                // Just in case we didn't handle an error properly,
                // capture it here. If an error was captured with
                // `capture_error` further down in the callstack, it
                // should be explicitly handled here as a
                // PortablePdbCacheError variant.
                sentry::capture_error(e);
                ObjectFileStatus::Other
            }
        }
    }
}

fn object_id_from_object_info(object_info: &RawObjectInfo) -> ObjectId {
    ObjectId {
        debug_id: match object_info.debug_id.as_deref() {
            None | Some("") => None,
            Some(string) => string.parse().ok(),
        },
        code_id: match object_info.code_id.as_deref() {
            None | Some("") => None,
            Some(string) => string.parse().ok(),
        },
        debug_file: object_info.debug_file.clone(),
        code_file: object_info.code_file.clone(),
        object_type: object_info.ty,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    use symbolicator_sources::{ObjectType, SourceConfig};

    use crate::config::Config;
    use crate::services::create_service;
    use crate::services::symbolication::module_lookup::ModuleLookup;
    use crate::test::{self, fixture};
    use crate::types::{CompleteObjectInfo, RawFrame, RawStacktrace, Scope};
    use crate::utils::addr::AddrMode;
    use crate::utils::hex::HexValue;

    /// Setup tests and create a test service.
    ///
    /// This function returns a tuple containing the service to test, and a temporary cache
    /// directory. The directory is cleaned up when the [`TempDir`] instance is dropped. Keep it as
    /// guard until the test has finished.
    ///
    /// The service is configured with `connect_to_reserved_ips = True`. This allows to use a local
    /// symbol server to test object file downloads.
    pub(crate) async fn setup_service() -> (SymbolicationActor, test::TempDir) {
        test::setup();

        let cache_dir = test::tempdir();

        let config = Config {
            cache_dir: Some(cache_dir.path().to_owned()),
            connect_to_reserved_ips: true,
            ..Default::default()
        };
        let handle = tokio::runtime::Handle::current();
        let (symbolication, _objects) = create_service(&config, handle).await.unwrap();

        (symbolication, cache_dir)
    }

    fn get_symbolication_request(sources: Vec<SourceConfig>) -> SymbolicateStacktraces {
        SymbolicateStacktraces {
            scope: Scope::Global,
            signal: None,
            sources: Arc::from(sources),
            origin: StacktraceOrigin::Symbolicate,
            stacktraces: vec![RawStacktrace {
                frames: vec![RawFrame {
                    instruction_addr: HexValue(0x1_0000_0fa0),
                    ..RawFrame::default()
                }],
                ..RawStacktrace::default()
            }],
            modules: vec![CompleteObjectInfo::from(RawObjectInfo {
                ty: ObjectType::Macho,
                code_id: Some("502fc0a51ec13e479998684fa139dca7".to_owned().to_lowercase()),
                debug_id: Some("502fc0a5-1ec1-3e47-9998-684fa139dca7".to_owned()),
                image_addr: HexValue(0x1_0000_0000),
                image_size: Some(4096),
                code_file: None,
                debug_file: None,
                checksum: None,
            })],
        }
    }

    /// Helper to redact the port number from localhost URIs in insta snapshots.
    ///
    /// Since we use a localhost source on a random port during tests we get random port
    /// numbers in URI of the dif object file candidates.  This redaction masks this out.
    pub(crate) fn redact_localhost_port(
        value: insta::internals::Content,
        _path: insta::internals::ContentPath<'_>,
    ) -> impl Into<insta::internals::Content> {
        let re = regex::Regex::new(r"^http://localhost:[0-9]+").unwrap();
        re.replace(value.as_str().unwrap(), "http://localhost:<port>")
            .into_owned()
    }

    macro_rules! assert_snapshot {
        ($e:expr) => {
            ::insta::assert_yaml_snapshot!($e, {
                ".**.location" => ::insta::dynamic_redaction(
                    $crate::services::symbolication::tests::redact_localhost_port
                )
            });
        }
    }

    #[tokio::test]
    async fn test_remove_bucket() {
        // Test with sources first, and then without. This test should verify that we do not leak
        // cached debug files to requests that no longer specify a source.

        let (symbolication, _cache_dir) = setup_service().await;
        let (_symsrv, source) = test::symbol_server();

        let request = get_symbolication_request(vec![source]);
        let response = symbolication.symbolicate(request).await;

        assert_snapshot!(response.unwrap());

        let request = get_symbolication_request(vec![]);
        let response = symbolication.symbolicate(request).await;

        assert_snapshot!(response.unwrap());
    }

    #[tokio::test]
    async fn test_add_bucket() {
        // Test without sources first, then with. This test should verify that we apply a new source
        // to requests immediately.

        let (symbolication, _cache_dir) = setup_service().await;
        let (_symsrv, source) = test::symbol_server();

        let request = get_symbolication_request(vec![]);
        let response = symbolication.symbolicate(request).await;

        assert_snapshot!(response.unwrap());

        let request = get_symbolication_request(vec![source]);
        let response = symbolication.symbolicate(request).await;

        assert_snapshot!(response.unwrap());
    }

    #[tokio::test]
    async fn test_apple_crash_report() {
        let (symbolication, _cache_dir) = setup_service().await;
        let (_symsrv, source) = test::symbol_server();

        let report_file = std::fs::File::open(fixture("apple_crash_report.txt")).unwrap();

        let response = symbolication
            .process_apple_crash_report(Scope::Global, report_file, Arc::new([source]))
            .await;

        assert_snapshot!(response.unwrap());
    }

    #[tokio::test]
    async fn test_wasm_payload() {
        let (symbolication, _cache_dir) = setup_service().await;
        let (_symsrv, source) = test::symbol_server();

        let modules: Vec<RawObjectInfo> = serde_json::from_str(
            r#"[
              {
                "type":"wasm",
                "debug_id":"bda18fd8-5d4a-4eb8-9302-2d6bfad846b1",
                "code_id":"bda18fd85d4a4eb893022d6bfad846b1",
                "debug_file":"file://foo.invalid/demo.wasm"
              }
            ]"#,
        )
        .unwrap();

        let stacktraces = serde_json::from_str(
            r#"[
              {
                "frames":[
                  {
                    "instruction_addr":"0x8c",
                    "addr_mode":"rel:0"
                  }
                ]
              }
            ]"#,
        )
        .unwrap();

        let request = SymbolicateStacktraces {
            modules: modules.into_iter().map(From::from).collect(),
            stacktraces,
            signal: None,
            origin: StacktraceOrigin::Symbolicate,
            sources: Arc::new([source]),
            scope: Default::default(),
        };

        let response = symbolication.symbolicate(request).await;

        assert_snapshot!(response.unwrap());
    }

    #[tokio::test]
    async fn test_source_candidates() {
        let (symbolication, _cache_dir) = setup_service().await;
        let (_symsrv, source) = test::symbol_server();

        // its not wasm, but that is the easiest to write tests because of relative
        // addressing ;-)
        let modules: Vec<RawObjectInfo> = serde_json::from_str(
            r#"[
              {
                "type":"wasm",
                "debug_id":"7f883fcd-c553-36d0-a809-b0150f09500b",
                "code_id":"7f883fcdc55336d0a809b0150f09500b"
              }
            ]"#,
        )
        .unwrap();

        let stacktraces = serde_json::from_str(
            r#"[
              {
                "frames":[
                  {
                    "instruction_addr":"0x3880",
                    "addr_mode":"rel:0"
                  }
                ]
              }
            ]"#,
        )
        .unwrap();

        let request = SymbolicateStacktraces {
            modules: modules.into_iter().map(From::from).collect(),
            stacktraces,
            signal: None,
            origin: StacktraceOrigin::Symbolicate,
            sources: Arc::new([source]),
            scope: Default::default(),
        };

        let response = symbolication.symbolicate(request).await;

        assert_snapshot!(response.unwrap());
    }

    #[test]
    fn test_symcache_lookup_open_end_addr() {
        test::setup();

        // The Rust SDK and some other clients sometimes send zero-sized images when no end addr
        // could be determined. Symbolicator should still resolve such images.
        let info = CompleteObjectInfo::from(RawObjectInfo {
            ty: ObjectType::Unknown,
            code_id: None,
            debug_id: None,
            code_file: None,
            debug_file: None,
            image_addr: HexValue(42),
            image_size: Some(0),
            checksum: None,
        });

        let lookup = ModuleLookup::new(Scope::Global, Arc::new([]), std::iter::once(info.clone()));

        let lookup_result = lookup.lookup_cache(43, AddrMode::Abs).unwrap();
        assert_eq!(lookup_result.module_index, 0);
        assert_eq!(lookup_result.object_info, &info);
        assert!(lookup_result.cache.is_none());
    }

    #[tokio::test]
    async fn test_dotnet_integration() {
        let (symbolication, _cache_dir) = setup_service().await;
        let (_srv, source) = test::symbol_server();

        let modules: Vec<RawObjectInfo> = serde_json::from_str(
            r#"[
              {
                "type":"pe_dotnet",
                "debug_file":"integration.pdb",
                "debug_id":"0c1033f78632492e91c6c314b72e1920e60b819d"
              }
            ]"#,
        )
        .unwrap();

        let stacktraces = serde_json::from_str(
            r#"[
              {
                "frames":[
                  {
                    "instruction_addr": 10,
                    "function_id": 6,
                    "addr_mode":"rel:0"
                  },
                  {
                    "instruction_addr": 6,
                    "function_id": 5,
                    "addr_mode": "rel:0"
                  },
                  {
                    "instruction_addr": 0,
                    "function_id": 3,
                    "addr_mode": "rel:0"
                  },
                  {
                    "instruction_addr": 0,
                    "function_id": 2,
                    "addr_mode": "rel:0"
                  },
                  {
                    "instruction_addr": 45,
                    "function_id": 1,
                    "addr_mode": "rel:0"
                  }
                ]
              }
            ]"#,
        )
        .unwrap();

        let request = SymbolicateStacktraces {
            modules: modules.into_iter().map(From::from).collect(),
            stacktraces,
            signal: None,
            origin: StacktraceOrigin::Symbolicate,
            sources: Arc::new([source]),
            scope: Default::default(),
        };

        let response = symbolication.symbolicate(request).await;

        assert_snapshot!(response.unwrap());
    }
}
