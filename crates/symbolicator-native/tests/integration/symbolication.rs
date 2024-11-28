use std::sync::Arc;

use symbolicator_service::types::Scope;

use crate::{
    assert_snapshot, example_request, fixture, make_symbolication_request, setup_service,
    symbol_server,
};

#[tokio::test]
async fn test_remove_bucket() {
    // Test with sources first, and then without. This test should verify that we do not leak
    // cached debug files to requests that no longer specify a source.

    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_symsrv, source) = symbol_server();

    let request = example_request(vec![source]);
    let response = symbolication.symbolicate(request).await;

    assert_snapshot!(response.unwrap());

    let request = example_request(vec![]);
    let response = symbolication.symbolicate(request).await;

    assert_snapshot!(response.unwrap());
}

#[tokio::test]
async fn test_add_bucket() {
    // Test without sources first, then with. This test should verify that we apply a new source
    // to requests immediately.

    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_symsrv, source) = symbol_server();

    let request = example_request(vec![]);
    let response = symbolication.symbolicate(request).await;

    assert_snapshot!(response.unwrap());

    let request = example_request(vec![source]);
    let response = symbolication.symbolicate(request).await;

    assert_snapshot!(response.unwrap());
}

#[tokio::test]
async fn test_apple_crash_report() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_symsrv, source) = symbol_server();

    let report_file = std::fs::File::open(fixture("apple_crash_report.txt")).unwrap();

    let response = symbolication
        .process_apple_crash_report(
            None,
            Scope::Global,
            report_file,
            Arc::new([source]),
            Default::default(),
        )
        .await;

    assert_snapshot!(response.unwrap());
}

#[tokio::test]
async fn test_wasm_payload() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_symsrv, source) = symbol_server();

    let request = make_symbolication_request(
        vec![source],
        r#"[{
          "type":"wasm",
          "debug_id":"bda18fd8-5d4a-4eb8-9302-2d6bfad846b1",
          "code_id":"bda18fd85d4a4eb893022d6bfad846b1",
          "debug_file":"file://foo.invalid/demo.wasm"
        }]"#,
        r#"[{
          "frames":[{
            "instruction_addr":"0x8c",
            "addr_mode":"rel:0"
          }]
        }]"#,
    );
    let response = symbolication.symbolicate(request).await;

    assert_snapshot!(response.unwrap());
}

#[tokio::test]
async fn test_source_candidates() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_symsrv, source) = symbol_server();

    // its not wasm, but that is the easiest to write tests because of relative
    // addressing ;-)
    let request = make_symbolication_request(
        vec![source],
        r#"[{
          "type":"wasm",
          "debug_id":"7f883fcd-c553-36d0-a809-b0150f09500b",
          "code_id":"7f883fcdc55336d0a809b0150f09500b"
        }]"#,
        r#"[{
          "frames":[{
            "instruction_addr":"0x3880",
            "addr_mode":"rel:0"
          }]
        }]"#,
    );
    let response = symbolication.symbolicate(request).await;

    assert_snapshot!(response.unwrap());
}

#[tokio::test]
async fn test_dotnet_integration() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) = symbol_server();

    let request = make_symbolication_request(
        vec![source],
        r#"[{
          "type":"pe_dotnet",
          "debug_file":"integration.pdb",
          "debug_id":"0c1033f78632492e91c6c314b72e1920e60b819d"
        }]"#,
        r#"[{
          "frames":[{
            "instruction_addr": 10,
            "function_id": 6,
            "addr_mode":"rel:0"
          }, {
            "instruction_addr": 6,
            "function_id": 5,
            "addr_mode": "rel:0"
          }, {
            "instruction_addr": 0,
            "function_id": 3,
            "addr_mode": "rel:0"
          }, {
            "instruction_addr": 0,
            "function_id": 2,
            "addr_mode": "rel:0"
          }, {
            "instruction_addr": 45,
            "function_id": 1,
            "addr_mode": "rel:0"
          }]
        }]"#,
    );
    let response = symbolication.symbolicate(request).await;

    assert_snapshot!(response.unwrap());
}

#[tokio::test]
async fn test_dotnet_windows_pdb() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) = symbol_server();

    // try to symbolicate a `PE_DOTNET` event with a Windows PDB file, this should fail
    let request = make_symbolication_request(
        vec![source],
        r#"[{
          "type":"pe_dotnet",
          "debug_file":"crash.pdb",
          "debug_id":"3249d99d0c4049318610f4e4fb0b6936"
        }]"#,
        r#"[{
          "frames":[{
            "instruction_addr": 10,
            "function_id": 6,
            "addr_mode":"rel:0"
          }, {
            "instruction_addr": 6,
            "function_id": 5,
            "addr_mode": "rel:0"
          }, {
            "instruction_addr": 0,
            "function_id": 3,
            "addr_mode": "rel:0"
          }, {
            "instruction_addr": 0,
            "function_id": 2,
            "addr_mode": "rel:0"
          }, {
            "instruction_addr": 45,
            "function_id": 1,
            "addr_mode": "rel:0"
          }]
        }]"#,
    );
    let response = symbolication.symbolicate(request).await;

    assert_snapshot!(response.unwrap());
}

#[tokio::test]
async fn test_dotnet_embedded_sources() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) = symbol_server();

    let request = make_symbolication_request(
        vec![source],
        r#"[{
          "type":"pe_dotnet",
          "debug_file":"portable-embedded.pdb",
          "debug_id":"b6919861-510c-4887-9994-943f64f70c37-870b9ef9"
        }]"#,
        r#"[{
          "frames":[{
            "instruction_addr": 47,
            "function_id": 5,
            "addr_mode":"rel:0"
          }]
        }]"#,
    );
    let response = symbolication.symbolicate(request).await;

    assert_snapshot!(response.unwrap());
}

#[tokio::test]
async fn test_dotnet_source_links() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) = symbol_server();

    let request = make_symbolication_request(
        vec![source],
        r#"[{
          "type":"pe_dotnet",
          "debug_file":"source-links.pdb",
          "debug_id":"37e9e8a6-1a8e-404e-b93c-6902e277ff55-a09672e1"
        }]"#,
        r#"[{
          "frames":[{
            "instruction_addr": 1,
            "function_id": 7,
            "addr_mode":"rel:0"
          }]
        }]"#,
    );
    let response = symbolication.symbolicate(request).await;

    assert_snapshot!(response.unwrap());
}

#[tokio::test]
async fn test_dotnet_only_source_links() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) = symbol_server();

    let request = make_symbolication_request(
        vec![source],
        r#"[{
          "type":"pe_dotnet",
          "debug_file":"source-links.pdb",
          "debug_id":"0c380a12-8221-4069-8565-bee6b3ac196e-a596286e"
        }]"#,
        r#"[{
          "frames":[{
            "instruction_addr": "0x2f",
            "function_id": "0x5",
            "addr_mode":"rel:0"
          }]
        }]"#,
    );
    let response = symbolication.symbolicate(request).await;

    assert_snapshot!(response.unwrap());
}
