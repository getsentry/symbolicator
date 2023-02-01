//! Integration tests for some public symbol servers that we want to explicitly fetch from.
//!
//! Ideally we should only test each public symbol server we care about only once, as fetching "real"
//! symbols from the web can be slow and presents a source of flakiness.
//! FIXME: We currently use the microsoft symbol server all over the place for a couple of tests,
//! which we should migrate over to use our internal fixtures instead.

use std::sync::Arc;

use symbolicator_sources::{
    DirectoryLayoutType, FileType, HttpSourceConfig, SourceConfig, SourceId,
};

use crate::{assert_snapshot, make_symbolication_request, setup_service, source_config};

#[tokio::test]
async fn test_nuget_source() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let source = SourceConfig::Http(Arc::new(HttpSourceConfig {
        id: SourceId::new("nuget"),
        url: "https://symbols.nuget.org/download/symbols/"
            .parse()
            .unwrap(),
        headers: Default::default(),
        files: source_config(
            DirectoryLayoutType::Symstore,
            vec![FileType::Pe, FileType::Pdb, FileType::PortablePdb],
        ),
    }));

    let request = make_symbolication_request(
        vec![source],
        r#"[{
          "type":"pe_dotnet",
          "code_id": "efc9a199e000",
          "code_file": "./TimeZoneConverter.dll",
          "debug_id": "4e2ca887-825e-46f3-968f-25b41ae1b5f3-9e6d3fcc",
          "debug_file": "./TimeZoneConverter.pdb",
          "debug_checksum": "SHA256:87a82c4e5e82f386968f25b41ae1b5f3cc3f6d9e79cfb4464f8240400fc47dcd"
        }]"#,
        r#"[{
          "frames":[{
            "instruction_addr": "0x21",
            "function_id": "0xc",
            "addr_mode":"rel:0"
          }]
        }]"#,
    );
    let response = symbolication.symbolicate(request).await;

    assert_snapshot!(response.unwrap());
}
