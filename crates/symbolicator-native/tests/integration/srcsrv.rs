use crate::{assert_snapshot, make_symbolication_request, setup_service, symbol_server};
use symbolicator_native::interface::CompletedSymbolicationResponse;

async fn symbolicate_with_pdb(pdb_file: &str) -> CompletedSymbolicationResponse {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_symsrv, source) = symbol_server();

    let modules_json = format!(
        r#"[{{
          "type":"pe",
          "debug_id":"3249d99d-0c40-4931-8610-f4e4fb0b6936-1",
          "code_file":"C:\\projects\\breakpad-tools\\windows\\Release\\crash.exe",
          "debug_file":"C:\\projects\\breakpad-tools\\windows\\Release\\{}",
          "image_addr": "0x2a0000",
          "image_size": 36864
        }}]"#,
        pdb_file
    );

    let request = make_symbolication_request(
        vec![source],
        &modules_json,
        r#"[{
          "frames":[{
            "instruction_addr":"0x2a2755"
          }]
        }]"#,
    );

    symbolication.symbolicate(request).await.unwrap()
}

#[tokio::test]
async fn test_pdb_with_srcsrv() {
    let response = symbolicate_with_pdb("crash_with_srcsrv.pdb").await;

    assert!(!response.modules.is_empty(), "Should have module info");

    let module = &response.modules[0];
    assert_eq!(
        module.features.srcsrv_vcs.as_deref(),
        Some("Perforce"),
        "Should extract Perforce VCS name from SRCSRV stream"
    );

    assert_snapshot!(response);
}

#[tokio::test]
async fn test_pdb_without_srcsrv() {
    let response = symbolicate_with_pdb("crash.pdb").await;

    assert!(!response.modules.is_empty(), "Should have module info");

    let module = &response.modules[0];
    assert_eq!(
        module.features.srcsrv_vcs, None,
        "Should not have VCS info for PDB without SRCSRV stream"
    );

    assert_snapshot!(response);
}
