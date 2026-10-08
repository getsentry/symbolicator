use std::path::Path;
use std::sync::Arc;

use serde_json::json;
use symbolic::debuginfo::sourcebundle::{SourceBundleWriter, SourceFileInfo, SourceFileType};
use symbolicator_js::interface::{
    JsFrame, JsModule, JsModuleType, JsStacktrace, ResolvedWith, SymbolicateJsStacktraces,
};
use symbolicator_service::types::{FrameOrder, Scope, ScrapingConfig};
use symbolicator_sources::SentrySourceConfig;

use crate::setup_service;

const DEBUG_ID: &str = "22222222-2222-4222-8222-222222222222";

fn write_bundle(path: &Path, source_name: &str, debug_id: &str) {
    let mut bundle = SourceBundleWriter::create(path).unwrap();
    let mut source_info = SourceFileInfo::new();
    source_info.set_ty(SourceFileType::MinifiedSource);
    source_info.set_url("~/main.jsbundle".into());
    source_info.add_header("debug-id".into(), debug_id.into());
    source_info.add_header("sourcemap".into(), "main.jsbundle.map".into());
    bundle
        .add_file("main.jsbundle", &b""[..], source_info)
        .unwrap();

    let sourcemap = json!({
        "version": 3,
        "sources": [source_name],
        "sourcesContent": [format!("throw new Error('{source_name}');")],
        "names": [],
        "mappings": "AAAA",
    });
    let mut map_info = SourceFileInfo::new();
    map_info.set_ty(SourceFileType::SourceMap);
    map_info.set_url("~/main.jsbundle.map".into());
    map_info.add_header("debug-id".into(), debug_id.into());
    bundle
        .add_file(
            "main.jsbundle.map",
            sourcemap.to_string().as_bytes(),
            map_info,
        )
        .unwrap();
    bundle.finish().unwrap();
}

fn request(source: SentrySourceConfig, release: &str) -> SymbolicateJsStacktraces {
    SymbolicateJsStacktraces {
        platform: None,
        scope: Scope::Global,
        source: Arc::new(source),
        release: Some(release.into()),
        dist: Some("1".into()),
        scraping: ScrapingConfig {
            enabled: false,
            ..Default::default()
        },
        apply_source_context: true,
        frame_order: FrameOrder::CallerFirst,
        stacktraces: vec![JsStacktrace {
            frames: ["app:///InternalBytecode.js", "app:///main.jsbundle"]
                .into_iter()
                .map(|abs_path| JsFrame {
                    abs_path: abs_path.into(),
                    lineno: 1,
                    colno: Some(1),
                    ..Default::default()
                })
                .collect(),
        }],
        modules: vec![JsModule {
            r#type: JsModuleType::Sourcemap,
            code_file: "app:///main.jsbundle".into(),
            debug_id: DEBUG_ID.into(),
        }],
    }
}

#[tokio::test]
async fn debug_id_lookup_before_url_fallback() {
    // Whether a lookup by URL also returns the debug ID bundle.
    for return_both_bundles in [false, true] {
        let (symbolication, _cache_dir) = setup_service(|_| ());
        let fixtures = symbolicator_test::tempdir();
        // The release bundle sorts first in the reverse-order bundle cache lookup.
        write_bundle(
            &fixtures.path().join("02-release.zip"),
            "old.js",
            "11111111-1111-4111-8111-111111111111",
        );
        write_bundle(
            &fixtures.path().join("01-debug-id.zip"),
            "current.js",
            DEBUG_ID,
        );
        let (server, source) =
            symbolicator_test::sourcemap_server(fixtures.path(), move |url, query| {
                let old = json!({
                    "type": "bundle", "id": "2",
                    "url": format!("{url}/02-release.zip"), "resolved_with": "release",
                });
                let current = json!({
                    "type": "bundle", "id": "1",
                    "url": format!("{url}/01-debug-id.zip"), "resolved_with": "debug-id",
                });
                if query.contains(&format!("debug_id={DEBUG_ID}")) {
                    json!([current])
                } else if return_both_bundles {
                    json!([old, current])
                } else {
                    json!([old])
                }
            });

        server.all_hits();

        let response = symbolication
            .symbolicate_js(request(source, "current"))
            .await;
        let frame = &response.stacktraces[0].frames[1];
        assert_eq!(frame.filename.as_deref(), Some("current.js"));
        assert_eq!(
            frame.context_line.as_deref(),
            Some("throw new Error('current.js');")
        );
        assert_eq!(frame.data.resolved_with, Some(ResolvedWith::DebugId));
        assert!(frame.data.symbolicated);
        assert_eq!(response.errors.len(), 1);
        assert_eq!(response.errors[0].abs_path, "app:///InternalBytecode.js");

        let debug_id_queries: usize = server
            .all_hits()
            .iter()
            .filter(|(url, _)| url.contains("debug_id="))
            .map(|(_, count)| count)
            .sum();
        assert_eq!(debug_id_queries, usize::from(!return_both_bundles));
    }
}

#[tokio::test]
async fn debug_id_lookup_before_individual_artifact_fallback() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let fixtures = symbolicator_test::tempdir();
    write_bundle(&fixtures.path().join("current.zip"), "current.js", DEBUG_ID);
    std::fs::write(fixtures.path().join("main.jsbundle"), "").unwrap();
    std::fs::write(
        fixtures.path().join("main.jsbundle.map"),
        json!({
            "version": 3, "sources": ["old.js"],
            "sourcesContent": ["throw new Error('old.js');"],
            "names": [], "mappings": "AAAA",
        })
        .to_string(),
    )
    .unwrap();
    let (_server, source) = symbolicator_test::sourcemap_server(fixtures.path(), |url, query| {
        if query.contains(&format!("debug_id={DEBUG_ID}")) {
            json!([{
                "type": "bundle", "id": "3",
                "url": format!("{url}/current.zip"), "resolved_with": "debug-id",
            }])
        } else {
            json!([{
                "type": "file", "id": "1", "abs_path": "~/main.jsbundle",
                "url": format!("{url}/main.jsbundle"), "resolved_with": "release-old",
                "headers": {"sourcemap": "main.jsbundle.map"},
            }, {
                "type": "file", "id": "2", "abs_path": "~/main.jsbundle.map",
                "url": format!("{url}/main.jsbundle.map"), "resolved_with": "release-old",
            }])
        }
    });
    let response = symbolication
        .symbolicate_js(request(source, "current"))
        .await;
    let frame = &response.stacktraces[0].frames[1];
    assert_eq!(frame.filename.as_deref(), Some("current.js"));
    assert_eq!(frame.data.resolved_with, Some(ResolvedWith::DebugId));
    assert!(frame.data.symbolicated);
}

#[tokio::test]
async fn debug_id_lookup_preserves_release_fallback() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let fixtures = symbolicator_test::tempdir();
    write_bundle(
        &fixtures.path().join("release.zip"),
        "old.js",
        "11111111-1111-4111-8111-111111111111",
    );
    let (_server, source) = symbolicator_test::sourcemap_server(fixtures.path(), |url, query| {
        if query.contains("debug_id=") {
            json!([])
        } else {
            json!([{
                "type": "bundle", "id": "1",
                "url": format!("{url}/release.zip"), "resolved_with": "release",
            }])
        }
    });
    let response = symbolication
        .symbolicate_js(request(source, "current"))
        .await;
    let frame = &response.stacktraces[0].frames[1];
    assert_eq!(frame.filename.as_deref(), Some("old.js"));
    assert_eq!(frame.data.resolved_with, Some(ResolvedWith::Release));
    assert!(frame.data.symbolicated);
}
