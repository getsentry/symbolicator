use std::sync::Arc;

use symbolicator_service::config::Config;
use symbolicator_service::services::create_service;
use symbolicator_service::services::symbolication::{
    StacktraceOrigin, SymbolicateStacktraces, SymbolicationActor,
};
use symbolicator_service::types::{
    CompleteObjectInfo, RawFrame, RawObjectInfo, RawStacktrace, Scope,
};
use symbolicator_service::utils::hex::HexValue;
use symbolicator_sources::{ObjectType, SourceConfig};
use symbolicator_test as test;
use symbolicator_test::{assert_snapshot, fixture};

/// Setup tests and create a test service.
///
/// This function returns a tuple containing the service to test, and a temporary cache
/// directory. The directory is cleaned up when the [`TempDir`] instance is dropped. Keep it as
/// guard until the test has finished.
///
/// The service is configured with `connect_to_reserved_ips = True`. This allows to use a local
/// symbol server to test object file downloads.
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
    let (symbolication, _objects) = create_service(&config, handle).unwrap();

    (symbolication, cache_dir)
}

pub fn get_symbolication_request(sources: Vec<SourceConfig>) -> SymbolicateStacktraces {
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
            debug_checksum: None,
        })],
    }
}

pub fn make_symbolication_request(
    modules: Vec<RawObjectInfo>,
    stacktraces: Vec<RawStacktrace>,
    sources: Vec<SourceConfig>,
) -> SymbolicateStacktraces {
    SymbolicateStacktraces {
        modules: modules.into_iter().map(From::from).collect(),
        stacktraces,
        signal: None,
        origin: StacktraceOrigin::Symbolicate,
        sources: Arc::from(sources),
        scope: Default::default(),
    }
}

#[tokio::test]
async fn test_remove_bucket() {
    // Test with sources first, and then without. This test should verify that we do not leak
    // cached debug files to requests that no longer specify a source.

    let (symbolication, _cache_dir) = setup_service(|_| ());
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

    let (symbolication, _cache_dir) = setup_service(|_| ());
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
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_symsrv, source) = test::symbol_server();

    let report_file = std::fs::File::open(fixture("apple_crash_report.txt")).unwrap();

    let response = symbolication
        .process_apple_crash_report(Scope::Global, report_file, Arc::new([source]))
        .await;

    assert_snapshot!(response.unwrap());
}

#[tokio::test]
async fn test_wasm_payload() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_symsrv, source) = test::symbol_server();

    let modules = serde_json::from_str(
        r#"[{
          "type":"wasm",
          "debug_id":"bda18fd8-5d4a-4eb8-9302-2d6bfad846b1",
          "code_id":"bda18fd85d4a4eb893022d6bfad846b1",
          "debug_file":"file://foo.invalid/demo.wasm"
        }]"#,
    )
    .unwrap();

    let stacktraces = serde_json::from_str(
        r#"[{
          "frames":[{
            "instruction_addr":"0x8c",
            "addr_mode":"rel:0"
          }]
        }]"#,
    )
    .unwrap();

    let request = make_symbolication_request(modules, stacktraces, vec![source]);
    let response = symbolication.symbolicate(request).await;

    assert_snapshot!(response.unwrap());
}

#[tokio::test]
async fn test_source_candidates() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_symsrv, source) = test::symbol_server();

    // its not wasm, but that is the easiest to write tests because of relative
    // addressing ;-)
    let modules = serde_json::from_str(
        r#"[{
          "type":"wasm",
          "debug_id":"7f883fcd-c553-36d0-a809-b0150f09500b",
          "code_id":"7f883fcdc55336d0a809b0150f09500b"
        }]"#,
    )
    .unwrap();

    let stacktraces = serde_json::from_str(
        r#"[{
          "frames":[{
            "instruction_addr":"0x3880",
            "addr_mode":"rel:0"
          }]
        }]"#,
    )
    .unwrap();

    let request = make_symbolication_request(modules, stacktraces, vec![source]);
    let response = symbolication.symbolicate(request).await;

    assert_snapshot!(response.unwrap());
}

#[tokio::test]
async fn test_dotnet_integration() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) = test::symbol_server();

    let modules = serde_json::from_str(
        r#"[{
          "type":"pe_dotnet",
          "debug_file":"integration.pdb",
          "debug_id":"0c1033f78632492e91c6c314b72e1920e60b819d"
        }]"#,
    )
    .unwrap();

    let stacktraces = serde_json::from_str(
        r#"[{
          "frames":[{
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
          }]
        }]"#,
    )
    .unwrap();

    let request = make_symbolication_request(modules, stacktraces, vec![source]);
    let response = symbolication.symbolicate(request).await;

    assert_snapshot!(response.unwrap());
}

#[tokio::test]
async fn test_dotnet_embedded_sources() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) = test::symbol_server();

    let modules = serde_json::from_str(
        r#"[{
          "type":"pe_dotnet",
          "debug_file":"portable-embedded.pdb",
          "debug_id":"b6919861-510c-4887-9994-943f64f70c37-870b9ef9"
        }]"#,
    )
    .unwrap();

    let stacktraces = serde_json::from_str(
        r#"[{
          "frames":[{
            "instruction_addr": 47,
            "function_id": 5,
            "addr_mode":"rel:0"
          }]
        }]"#,
    )
    .unwrap();

    let request = make_symbolication_request(modules, stacktraces, vec![source]);
    let response = symbolication.symbolicate(request).await;

    assert_snapshot!(response.unwrap());
}

#[tokio::test]
async fn test_nuget_file() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let source = test::nuget_source();

    let modules = serde_json::from_str(
        r#"[{
          "type":"pe_dotnet",
          "code_id": "efc9a199e000",
          "code_file": "./TimeZoneConverter.dll",
          "debug_id": "4e2ca887-825e-46f3-968f-25b41ae1b5f3-9e6d3fcc",
          "debug_file": "./TimeZoneConverter.pdb",
          "debug_checksum": "SHA256:87a82c4e5e82f386968f25b41ae1b5f3cc3f6d9e79cfb4464f8240400fc47dcd"
        }]"#,
    )
    .unwrap();

    let stacktraces = serde_json::from_str(
        r#"[{
          "frames":[{
            "instruction_addr": "0x21",
            "function_id": "0xc",
            "addr_mode":"rel:0"
          }]
        }]"#,
    )
    .unwrap();

    let request = make_symbolication_request(modules, stacktraces, vec![source]);
    let response = symbolication.symbolicate(request).await;

    assert_snapshot!(response.unwrap());
}
