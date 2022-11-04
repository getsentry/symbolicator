use std::fmt;

use thiserror::Error;

use symbolicator_sources::ObjectId;

use crate::services::cficaches::{CfiCacheActor, CfiCacheError};
use crate::services::objects::ObjectsActor;
use crate::services::ppdb_caches::{PortablePdbCacheActor, PortablePdbCacheError};
use crate::services::symcaches::{SymCacheActor, SymCacheError};
use crate::types::{ObjectFileStatus, RawObjectInfo, SymbolicationResponse};

mod apple;
mod module_lookup;
mod process_minidump;
// we should really rename this here to the `SymbolicatorService`, as it does a lot more
// than just symbolication ;-)
#[allow(clippy::module_inception)]
mod symbolication;

pub use symbolication::{StacktraceOrigin, SymbolicateStacktraces};

#[derive(Clone)]
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

impl fmt::Debug for SymbolicationActor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        f.debug_struct("SymbolicationActor")
            .field("objects", &self.objects)
            .field("symcaches", &self.symcaches)
            .field("cficaches", &self.cficaches)
            .field("diagnostics_cache", &self.diagnostics_cache)
            .finish()
    }
}

/// Errors during symbolication.
#[derive(Debug, Error)]
pub enum SymbolicationError {
    #[error("symbolication took too long")]
    Timeout,

    #[error(transparent)]
    Failed(#[from] anyhow::Error),

    #[error("failed to parse apple crash report")]
    InvalidAppleCrashReport(#[from] apple_crash_report_parser::ParseError),
}

impl SymbolicationError {
    fn to_symbolication_response(&self) -> SymbolicationResponse {
        match self {
            SymbolicationError::Timeout => SymbolicationResponse::Timeout,
            SymbolicationError::Failed(_) | SymbolicationError::InvalidAppleCrashReport(_) => {
                SymbolicationResponse::Failed {
                    message: self.to_string(),
                }
            }
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
    use super::*;

    use symbolicator_sources::ObjectType;

    use crate::config::Config;
    use crate::services::symbolication::module_lookup::ModuleLookup;
    use crate::services::Service;
    use crate::test::{self, fixture};
    use crate::types::{CompleteObjectInfo, RawFrame, RawStacktrace};
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
    pub(crate) async fn setup_service() -> (Service, test::TempDir) {
        test::setup();

        let cache_dir = test::tempdir();

        let config = Config {
            cache_dir: Some(cache_dir.path().to_owned()),
            connect_to_reserved_ips: true,
            ..Default::default()
        };
        let handle = tokio::runtime::Handle::current();
        let service = Service::create(config, handle.clone(), handle)
            .await
            .unwrap();

        (service, cache_dir)
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
            options: RequestOptions {
                dif_candidates: true,
            },
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
    async fn test_remove_bucket() -> Result<(), SymbolicationError> {
        // Test with sources first, and then without. This test should verify that we do not leak
        // cached debug files to requests that no longer specify a source.

        let (service, _cache_dir) = setup_service().await;
        let symbolication = service.symbolication();
        let (_symsrv, source) = test::symbol_server();

        let request = get_symbolication_request(vec![source]);
        let request_id = symbolication.symbolicate_stacktraces(request).unwrap();
        let response = symbolication.get_response(request_id, None).await;

        assert_snapshot!(response.unwrap());

        let request = get_symbolication_request(vec![]);
        let request_id = symbolication.symbolicate_stacktraces(request).unwrap();
        let response = symbolication.get_response(request_id, None).await;

        assert_snapshot!(response.unwrap());

        Ok(())
    }

    #[tokio::test]
    async fn test_add_bucket() -> anyhow::Result<()> {
        // Test without sources first, then with. This test should verify that we apply a new source
        // to requests immediately.

        let (service, _cache_dir) = setup_service().await;
        let symbolication = service.symbolication();
        let (_symsrv, source) = test::symbol_server();

        let request = get_symbolication_request(vec![]);
        let request_id = symbolication.symbolicate_stacktraces(request).unwrap();
        let response = symbolication.get_response(request_id, None).await;

        assert_snapshot!(response.unwrap());

        let request = get_symbolication_request(vec![source]);
        let request_id = symbolication.symbolicate_stacktraces(request).unwrap();
        let response = symbolication.get_response(request_id, None).await;

        assert_snapshot!(response.unwrap());

        Ok(())
    }

    #[tokio::test]
    async fn test_get_response_multi() {
        // Make sure we can repeatedly poll for the response
        let (service, _cache_dir) = setup_service().await;
        let symbolication = service.symbolication();

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
            modules: Vec::new(),
            stacktraces,
            signal: None,
            origin: StacktraceOrigin::Symbolicate,
            sources: Arc::new([]),
            scope: Default::default(),
            options: Default::default(),
        };

        let request_id = symbolication.symbolicate_stacktraces(request).unwrap();

        for _ in 0..2 {
            let response = symbolication.get_response(request_id, None).await.unwrap();

            assert!(
                matches!(&response, SymbolicationResponse::Completed(_)),
                "Not a complete response: {:#?}",
                response
            );
        }
    }

    #[tokio::test]
    async fn test_apple_crash_report() -> anyhow::Result<()> {
        let (service, _cache_dir) = setup_service().await;
        let symbolication = service.symbolication();
        let (_symsrv, source) = test::symbol_server();

        let report_file = std::fs::File::open(fixture("apple_crash_report.txt"))?;
        let request_id = symbolication
            .process_apple_crash_report(
                Scope::Global,
                report_file,
                Arc::new([source]),
                RequestOptions {
                    dif_candidates: true,
                },
            )
            .unwrap();

        let response = symbolication.get_response(request_id, None).await;

        assert_snapshot!(response.unwrap());
        Ok(())
    }

    #[tokio::test]
    async fn test_wasm_payload() -> anyhow::Result<()> {
        let (service, _cache_dir) = setup_service().await;
        let symbolication = service.symbolication();
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
        )?;

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
        )?;

        let request = SymbolicateStacktraces {
            modules: modules.into_iter().map(From::from).collect(),
            stacktraces,
            signal: None,
            origin: StacktraceOrigin::Symbolicate,
            sources: Arc::new([source]),
            scope: Default::default(),
            options: Default::default(),
        };

        let request_id = symbolication.symbolicate_stacktraces(request).unwrap();
        let response = symbolication.get_response(request_id, None).await;

        assert_snapshot!(response.unwrap());
        Ok(())
    }

    #[tokio::test]
    async fn test_source_candidates() -> anyhow::Result<()> {
        let (service, _cache_dir) = setup_service().await;
        let symbolication = service.symbolication();
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
        )?;

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
        )?;

        let request = SymbolicateStacktraces {
            modules: modules.into_iter().map(From::from).collect(),
            stacktraces,
            signal: None,
            origin: StacktraceOrigin::Symbolicate,
            sources: Arc::new([source]),
            scope: Default::default(),
            options: RequestOptions {
                dif_candidates: true,
            },
        };

        let request_id = symbolication.symbolicate_stacktraces(request).unwrap();
        let response = symbolication.get_response(request_id, None).await;

        assert_snapshot!(response.unwrap());
        Ok(())
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
    async fn test_max_requests() {
        test::setup();

        let cache_dir = test::tempdir();

        let config = Config {
            cache_dir: Some(cache_dir.path().to_owned()),
            connect_to_reserved_ips: true,
            max_concurrent_requests: Some(2),
            ..Default::default()
        };

        let handle = tokio::runtime::Handle::current();
        let service = Service::create(config, handle.clone(), handle)
            .await
            .unwrap();

        let symbolication = service.symbolication();
        let symbol_server = test::FailingSymbolServer::new();

        // Make three requests that never get resolved. Since the server is configured to only accept a maximum of
        // two concurrent requests, the first two should succeed and the third one should fail.
        let request = get_symbolication_request(vec![symbol_server.pending_source.clone()]);
        assert!(symbolication.symbolicate_stacktraces(request).is_ok());

        let request = get_symbolication_request(vec![symbol_server.pending_source.clone()]);
        assert!(symbolication.symbolicate_stacktraces(request).is_ok());

        let request = get_symbolication_request(vec![symbol_server.pending_source]);
        assert!(symbolication.symbolicate_stacktraces(request).is_err());
    }

    #[tokio::test]
    async fn test_dotnet_integration() -> anyhow::Result<()> {
        let (service, _cache_dir) = setup_service().await;
        let symbolication = service.symbolication();
        let (_srv, source) = test::symbol_server();

        let modules: Vec<RawObjectInfo> = serde_json::from_str(
            r#"[
              {
                "type":"pe_dotnet",
                "debug_file":"integration.pdb",
                "debug_id":"0c1033f78632492e91c6c314b72e1920e60b819d"
              }
            ]"#,
        )?;

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
        )?;

        let request = SymbolicateStacktraces {
            modules: modules.into_iter().map(From::from).collect(),
            stacktraces,
            signal: None,
            origin: StacktraceOrigin::Symbolicate,
            sources: Arc::new([source]),
            scope: Default::default(),
            options: RequestOptions {
                dif_candidates: true,
            },
        };

        let request_id = symbolication.symbolicate_stacktraces(request).unwrap();
        let response = symbolication.get_response(request_id, None).await;

        assert_snapshot!(response.unwrap());
        Ok(())
    }
}
