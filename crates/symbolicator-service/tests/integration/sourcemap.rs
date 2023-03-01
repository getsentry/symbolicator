use std::sync::Arc;

use symbolicator_service::types::JsFrame;
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
    dist: impl Into<Option<String>>,
) -> SymbolicateJsStacktraces {
    let frames: Vec<JsFrame> = serde_json::from_str(frames).unwrap();
    let stacktraces = vec![JsStacktrace { frames }];

    SymbolicateJsStacktraces {
        source: Arc::new(source),
        stacktraces,
        modules: vec![],
        dist: dist.into(),
    }
}

#[tokio::test]
async fn test_sourcemap_expansion() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) = symbolicator_test::sourcemap_server("01_sourcemap_expansion");

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

    let request = make_js_request(source, frames, None);
    let response = symbolication.symbolicate_js(request).await;

    assert_snapshot!(response.unwrap());
}

#[tokio::test]
async fn test_sourcemap_source_expansion() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) = symbolicator_test::sourcemap_server("02_sourcemap_source_expansion");

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

    let request = make_js_request(source, frames, None);
    let response = symbolication.symbolicate_js(request).await;

    assert_snapshot!(response.unwrap());
}

#[tokio::test]
async fn test_sourcemap_embedded_source_expansion() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) =
        symbolicator_test::sourcemap_server("03_sourcemap_embedded_source_expansion");

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

    let request = make_js_request(source, frames, None);
    let response = symbolication.symbolicate_js(request).await;

    assert_snapshot!(response.unwrap());
}

#[tokio::test]
async fn test_source_expansion() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) = symbolicator_test::sourcemap_server("04_source_expansion");

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

    let request = make_js_request(source, frames, None);
    let response = symbolication.symbolicate_js(request).await;

    assert_snapshot!(response.unwrap());
}

#[tokio::test]
async fn test_inlined_sources() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) = symbolicator_test::sourcemap_server("05_inlined_sources");

    let frames = r#"[{
        "abs_path": "http://example.com/test.min.js",
        "filename": "test.js",
        "lineno": 1,
        "colno": 1
    }]"#;

    let request = make_js_request(source, frames, None);
    let response = symbolication.symbolicate_js(request).await;

    assert_snapshot!(response.unwrap());
}

#[tokio::test]
async fn test_sourcemap_nofiles_source_expansion() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) =
        symbolicator_test::sourcemap_server("06_sourcemap_nofiles_source_expansion");

    let frames = r#"[{
        "abs_path": "app:///nofiles.js",
        "lineno": 1,
        "colno": 39
    }]"#;

    let request = make_js_request(source, frames, None);
    let response = symbolication.symbolicate_js(request).await;

    assert_snapshot!(response.unwrap());
}

#[tokio::test]
async fn test_indexed_sourcemap_source_expansion() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) =
        symbolicator_test::sourcemap_server("07_indexed_sourcemap_source_expansion");

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

    let request = make_js_request(source, frames, None);
    let response = symbolication.symbolicate_js(request).await;

    assert_snapshot!(response.unwrap());
}

#[tokio::test]
async fn test_malformed_abs_path() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) = symbolicator_test::sourcemap_server("08_malformed_abs_path");

    // Missing colon was removed on purpose.
    let frames = r#"[{
        "abs_path": "http//example.com/test.min.js",
        "filename": "test.js",
        "lineno": 1,
        "colno": 1
    }]"#;

    let request = make_js_request(source, frames, None);
    let response = symbolication.symbolicate_js(request).await;

    assert_snapshot!(response.unwrap());
}
