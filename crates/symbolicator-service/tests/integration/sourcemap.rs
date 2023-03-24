use std::sync::Arc;

use serde_json::json;
use symbolicator_service::types::{JsFrame, Scope};
use symbolicator_service::{
    services::symbolication::SymbolicateJsStacktraces, types::JsStacktrace,
};
use symbolicator_sources::SentrySourceConfig;

use crate::{assert_snapshot, setup_service};

/*
 * NOTES:
 *
 * - `cargo test --package symbolicator-service --test integration sourcemap` to run just tests below
 * - all test names are 1:1 to those in monolith for easier tracking
 * - original stacktrace frames are used in top-most frame first order, which is reverse of monolith
 *   is using, thus when you copy test from there, just reverse the json payload
 * - when copying the json payload, make sure to translate all escaped characters and python keywords
 *
 * TODOS:
 *
 * - add all error assertions, by querying for `event.data["errors"]` in monolith
 */

#[track_caller]
fn make_js_request(
    source: SentrySourceConfig,
    frames: &str,
    release: impl Into<Option<String>>,
    dist: impl Into<Option<String>>,
) -> SymbolicateJsStacktraces {
    let frames: Vec<JsFrame> = serde_json::from_str(frames).unwrap();
    let stacktraces = vec![JsStacktrace { frames }];

    SymbolicateJsStacktraces {
        scope: Scope::Global,
        source: Arc::new(source),
        stacktraces,
        modules: vec![],
        release: release.into(),
        dist: dist.into(),
        allow_scraping: false,
    }
}

#[tokio::test]
async fn test_sourcemap_expansion() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) =
        symbolicator_test::sourcemap_server("01_sourcemap_expansion", |url, _query| {
            json!([{
                "type": "file",
                "id": "1",
                "url": format!("{url}/test.min.js"),
                "abs_path": "~/test.min.js",
            }, {
                "type": "file",
                "id": "2",
                "url": format!("{url}/test.min.js.map"),
                "abs_path": "~/test.min.js.map",
            }])
        });

    let frames = r#"[{
        "abs_path": "http://example.com/index.html",
        "filename": "index.html",
        "lineno": 6,
        "colno": 7,
        "function": "produceStack"
    }, {
        "abs_path": "http://example.com/test.min.js",
        "filename": "test.min.js",
        "lineno": 1,
        "colno": 183,
        "function": "i"
    }, {
        "abs_path": "http://example.com/test.min.js",
        "filename": "test.min.js",
        "lineno": 1,
        "colno": 136,
        "function": "r"
    }, {
        "abs_path": "http://example.com/test.min.js",
        "filename": "test.min.js",
        "lineno": 1,
        "colno": 64,
        "function": "e"
    }]"#;

    let request = make_js_request(source, frames, None, None);
    let response = symbolication.symbolicate_js(request).await;

    assert_snapshot!(response.unwrap());
}

#[tokio::test]
async fn test_sourcemap_source_expansion() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) =
        symbolicator_test::sourcemap_server("02_sourcemap_source_expansion", |url, _query| {
            json!([{
                "type": "file",
                "id": "1",
                "url": format!("{url}/file.min.js"),
                "abs_path": "~/file.min.js",
            }, {
                "type": "file",
                "id": "2",
                "url": format!("{url}/file.min.js.map"),
                "abs_path": "~/file.min.js.map",
            }, {
                "type": "file",
                "id": "3",
                "url": format!("{url}/file1.js"),
                "abs_path": "~/file1.js",
            }, {
                "type": "file",
                "id": "4",
                "url": format!("{url}/file2.js"),
                "abs_path": "~/file2.js",
            }])
        });

    let frames = r#"[{
        "function": "function: \"HTMLDocument.<anonymous>\"",
        "abs_path": "http://example.com/index.html",
        "filename": "index.html",
        "lineno": 283,
        "colno": 17,
        "in_app": false
    }, {
        "abs_path": "http://example.com/file.min.js",
        "filename": "file.min.js",
        "lineno": 1,
        "colno": 39
    }]"#;

    let request = make_js_request(source, frames, None, None);
    let response = symbolication.symbolicate_js(request).await;

    assert_snapshot!(response.unwrap());
}

#[tokio::test]
async fn test_sourcemap_embedded_source_expansion() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) = symbolicator_test::sourcemap_server(
        "03_sourcemap_embedded_source_expansion",
        |url, _query| {
            json!([{
                "type": "file",
                "id": "1",
                "url": format!("{url}/embedded.js"),
                "abs_path": "~/embedded.js",
            }, {
                "type": "file",
                "id": "2",
                "url": format!("{url}/embedded.js.map"),
                "abs_path": "~/embedded.js.map",
            }])
        },
    );

    let frames = r#"[{
        "function": "function: \"HTMLDocument.<anonymous>\"",
        "abs_path": "http://example.com/index.html",
        "filename": "index.html",
        "lineno": 283,
        "colno": 17,
        "in_app": false
    }, {
        "abs_path": "http://example.com/embedded.js",
        "filename": "embedded.js",
        "lineno": 1,
        "colno": 39
    }]"#;

    let request = make_js_request(source, frames, None, None);
    let response = symbolication.symbolicate_js(request).await;

    assert_snapshot!(response.unwrap());
}

#[tokio::test]
async fn test_source_expansion() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) =
        symbolicator_test::sourcemap_server("04_source_expansion", |url, _query| {
            json!([{
                "type": "file",
                "id": "1",
                "url": format!("{url}/foo.js"),
                "abs_path": "~/foo.js",
            }])
        });

    let frames = r#"[{
        "abs_path": "http://example.com/foo.js",
        "filename": "foo.js",
        "lineno": 1,
        "colno": 0
    }, {
        "abs_path": "http://example.com/foo.js",
        "filename": "foo.js",
        "lineno": 4,
        "colno": 0
    }]"#;

    let request = make_js_request(source, frames, None, None);
    let response = symbolication.symbolicate_js(request).await;

    assert_snapshot!(response.unwrap());
}

#[tokio::test]
async fn test_inlined_sources() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) =
        symbolicator_test::sourcemap_server("05_inlined_sources", |url, _query| {
            json!([{
                "type": "file",
                "id": "1",
                "url": format!("{url}/test.min.js"),
                "abs_path": "~/test.min.js",
            }])
        });

    let frames = r#"[{
        "abs_path": "http://example.com/test.min.js",
        "filename": "test.js",
        "lineno": 1,
        "colno": 1
    }]"#;

    let request = make_js_request(source, frames, None, None);
    let response = symbolication.symbolicate_js(request).await;

    assert_snapshot!(response.unwrap());
}

#[tokio::test]
async fn test_sourcemap_nofiles_source_expansion() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) = symbolicator_test::sourcemap_server(
        "06_sourcemap_nofiles_source_expansion",
        |url, _query| {
            json!([{
                "type": "file",
                "id": "1",
                "url": format!("{url}/nofiles.js"),
                "abs_path": "~/nofiles.js",
            }, {
                "type": "file",
                "id": "2",
                "url": format!("{url}/nofiles.js.map"),
                "abs_path": "~/nofiles.js.map",
            }])
        },
    );

    let frames = r#"[{
        "abs_path": "app:///nofiles.js",
        "lineno": 1,
        "colno": 39
    }]"#;

    let request = make_js_request(source, frames, None, None);
    let response = symbolication.symbolicate_js(request).await;

    assert_snapshot!(response.unwrap());
}

#[tokio::test]
async fn test_indexed_sourcemap_source_expansion() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) = symbolicator_test::sourcemap_server(
        "07_indexed_sourcemap_source_expansion",
        |url, _query| {
            json!([{
                "type": "file",
                "id": "1",
                "url": format!("{url}/indexed.min.js"),
                "abs_path": "~/indexed.min.js",
            }, {
                "type": "file",
                "id": "2",
                "url": format!("{url}/indexed.min.js.map"),
                "abs_path": "~/indexed.min.js.map",
            }, {
                "type": "file",
                "id": "3",
                "url": format!("{url}/file1.js"),
                "abs_path": "~/file1.js",
            }, {
                "type": "file",
                "id": "4",
                "url": format!("{url}/file2.js"),
                "abs_path": "~/file2.js",
            }])
        },
    );

    let frames = r#"[{
        "abs_path": "http://example.com/indexed.min.js",
        "filename": "indexed.min.js",
        "lineno": 1,
        "colno": 39
    }, {
        "abs_path": "http://example.com/indexed.min.js",
        "filename": "indexed.min.js",
        "lineno": 2,
        "colno": 44
    }]"#;

    let request = make_js_request(source, frames, None, None);
    let response = symbolication.symbolicate_js(request).await;

    assert_snapshot!(response.unwrap());
}

#[tokio::test]
async fn test_malformed_abs_path() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) = symbolicator_test::sourcemap_server("", |_url, _query| json!([]));

    // Missing colon was removed on purpose.
    let frames = r#"[{
        "abs_path": "http//example.com/test.min.js",
        "filename": "test.js",
        "lineno": 1,
        "colno": 1
    }]"#;

    let request = make_js_request(source, frames, None, None);
    let response = symbolication.symbolicate_js(request).await;

    assert_snapshot!(response.unwrap());
}

#[tokio::test]
async fn test_fetch_error() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (srv, source) = symbolicator_test::sourcemap_server("", |_url, _query| json!([]));

    let missing_asset_url_1 = srv.url("/assets/missing_foo.js");
    let missing_asset_url_2 = srv.url("/assets/missing_bar.js");

    let frames = format!(
        r#"[{{
            "abs_path": "{missing_asset_url_1}",
            "filename": "foo.js",
            "lineno": 1,
            "colno": 0
        }}, {{
            "abs_path": "{missing_asset_url_2}",
            "filename": "foo.js",
            "lineno": 4,
            "colno": 0
        }}]"#
    );

    let mut request = make_js_request(source, &frames, None, None);
    request.allow_scraping = true;
    let response = symbolication.symbolicate_js(request).await;

    assert_snapshot!(response.unwrap());
}

#[tokio::test]
async fn test_invalid_location() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) =
        symbolicator_test::sourcemap_server("08_sourcemap_invalid_location", |url, _query| {
            json!([{
                "type": "file",
                "id": "1",
                "url": format!("{url}/invalidlocation.js"),
                "abs_path": "~/invalidlocation.js",
            }, {
                "type": "file",
                "id": "2",
                "url": format!("{url}/invalidlocation.js.map"),
                "abs_path": "~/invalidlocation.js.map",
            }])
        });

    let frames = r#"[{
        "abs_path": "http://example.com/invalidlocation.js",
        "filename": "invalidlocation.js",
        "lineno": 0,
        "colno": 0,
        "function": "e"
    }]"#;

    let request = make_js_request(source, frames, None, None);
    let response = symbolication.symbolicate_js(request).await;

    assert_snapshot!(response.unwrap());
}
