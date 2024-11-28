use std::sync::Arc;

use symbolicator_native::interface::{StacktraceOrigin, SymbolicateStacktraces};
use symbolicator_native::SymbolicationActor;
use symbolicator_service::config::Config;
use symbolicator_service::services::SharedServices;
use symbolicator_service::types::RawObjectInfo;
use symbolicator_sources::SourceConfig;
use symbolicator_test as test;

pub use test::{assert_snapshot, fixture, read_fixture, source_config, symbol_server, Server};

/// Setup tests and create a test service.
///
/// This function returns a tuple containing the service to test, and a temporary cache
/// directory. The directory is cleaned up when the [`TempDir`] instance is dropped. Keep it as
/// guard until the test has finished.
///
/// The service is configured with `connect_to_reserved_ips = True`. This allows to use a local
/// symbol server to test object file downloads.
/// The `update_config` closure can modify any default configuration if needed before the server is
/// started.
pub fn setup_service(
    update_config: impl FnOnce(&mut Config),
) -> (SymbolicationActor, test::TempDir) {
    test::setup();

    let cache_dir = test::tempdir();

    let mut config = Config {
        cache_dir: Some(cache_dir.path().to_owned()),
        connect_to_reserved_ips: true,
        ..Default::default()
    };
    update_config(&mut config);

    let handle = tokio::runtime::Handle::current();
    let shared_services = SharedServices::new(config, handle).unwrap();
    let symbolication = SymbolicationActor::new(&shared_services);

    (symbolication, cache_dir)
}

/// Creates a new Symbolication request with the given modules and stack traces,
/// which are being parsed from JSON.
#[track_caller]
pub fn make_symbolication_request(
    sources: Vec<SourceConfig>,
    modules: &str,
    stacktraces: &str,
) -> SymbolicateStacktraces {
    let modules: Vec<RawObjectInfo> = serde_json::from_str(modules).unwrap();
    let stacktraces = serde_json::from_str(stacktraces).unwrap();
    SymbolicateStacktraces {
        platform: None,
        modules: modules.into_iter().map(From::from).collect(),
        stacktraces,
        signal: None,
        origin: StacktraceOrigin::Symbolicate,
        sources: Arc::from(sources),
        scope: Default::default(),
        apply_source_context: true,
        scraping: Default::default(),
    }
}

/// Creates an example Symbolication request with the given sources.
/// The stack trace contains a single frame referencing the one module, which is available in the
/// fixtures that are being served by [`symbol_server`].
pub fn example_request(sources: Vec<SourceConfig>) -> SymbolicateStacktraces {
    make_symbolication_request(
        sources,
        r#"[{
          "type":"macho",
          "debug_id":"502fc0a5-1ec1-3e47-9998-684fa139dca7",
          "code_id":"502fc0a51ec13e479998684fa139dca7",
          "image_addr": "0x100000000",
          "image_size": 4096
        }]"#,
        r#"[{
          "frames":[{
            "instruction_addr":"0x100000fa0"
          }]
        }]"#,
    )
}
