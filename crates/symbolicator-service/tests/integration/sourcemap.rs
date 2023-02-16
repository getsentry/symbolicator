use std::sync::Arc;

use symbolicator_service::{
    services::symbolication::JsProcessingSymbolicateStacktraces, types::JsProcessingRawStacktrace,
};
use symbolicator_sources::{SentrySourceConfig, SourceId};

use crate::setup_service;

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
 * - move to snapshots once all tests are ported over by using `assert_snapshot!(response.unwrap());`
 *   everywhere instead of `assert!/assert_eq!`
 */

#[tokio::test]
async fn test_sourcemap_expansion() {
    let (symbolication, _) = setup_service(|_| ());
    let srv = symbolicator_test::sourcemap_server("01_sourcemap_expansion");

    let stacktraces: Vec<JsProcessingRawStacktrace> = serde_json::from_str(
        r#"[{
            "frames": [
                {
                    "abs_path": "http://example.com/index.html",
                    "filename": "index.html",
                    "lineno": 6,
                    "colno": 7,
                    "function": "produceStack"
                },
                {
                    "abs_path": "http://example.com/test.min.js",
                    "filename": "test.min.js",
                    "lineno": 1,
                    "colno": 183,
                    "function": "i"
                },
                {
                    "abs_path": "http://example.com/test.min.js",
                    "filename": "test.min.js",
                    "lineno": 1,
                    "colno": 136,
                    "function": "r"
                },
                {
                    "abs_path": "http://example.com/test.min.js",
                    "filename": "test.min.js",
                    "lineno": 1,
                    "colno": 64,
                    "function": "e"
                }
            ]
        }]"#,
    )
    .unwrap();

    let request = JsProcessingSymbolicateStacktraces {
        source: Arc::new(SentrySourceConfig {
            id: SourceId::new("sentry:project"),
            url: srv.url("/files/"),
            token: String::new(),
        }),
        stacktraces,
        dist: None,
    };

    let response = symbolication.js_processing_symbolicate(request).await;
    let frames = &mut response.unwrap().stacktraces[0].frames;

    assert_eq!(frames.len(), 4);

    assert_eq!(frames[0].raw.function, Some("produceStack".to_string()));
    assert_eq!(frames[0].raw.lineno, Some(6));
    assert_eq!(frames[0].raw.filename, Some("index.html".to_string()));

    assert_eq!(frames[1].raw.function, Some("test".to_string()));
    assert_eq!(frames[1].raw.lineno, Some(20));
    assert_eq!(frames[1].raw.filename, Some("test.js".to_string()));

    assert_eq!(frames[2].raw.function, Some("invoke".to_string()));
    assert_eq!(frames[2].raw.lineno, Some(15));
    assert_eq!(frames[2].raw.filename, Some("test.js".to_string()));

    assert_eq!(frames[3].raw.function, Some("onFailure".to_string()));
    assert_eq!(frames[3].raw.lineno, Some(5));
    assert_eq!(frames[3].raw.filename, Some("test.js".to_string()));
}

// TODO(kamil): Failing due to source context not resolving file content from non-sourceContents locations.
#[ignore]
#[tokio::test]
async fn test_sourcemap_source_expansion() {
    let (symbolication, _) = setup_service(|_| ());
    let srv = symbolicator_test::sourcemap_server("02_sourcemap_source_expansion");

    let stacktraces: Vec<JsProcessingRawStacktrace> = serde_json::from_str(
        r#"[{
            "frames": [
                {
                    "function": "function: \"HTMLDocument.<anonymous>\"",
                    "abs_path": "http//example.com/index.html",
                    "filename": "index.html",
                    "lineno": 283,
                    "colno": 17,
                    "in_app": false
                },
                {
                    "abs_path": "http://example.com/file.min.js",
                    "filename": "file.min.js",
                    "lineno": 1,
                    "colno": 39
                }
            ]
        }]"#,
    )
    .unwrap();

    let request = JsProcessingSymbolicateStacktraces {
        source: Arc::new(SentrySourceConfig {
            id: SourceId::new("sentry:project"),
            url: srv.url("/files/"),
            token: String::new(),
        }),
        stacktraces,
        dist: None,
    };

    let response = symbolication.js_processing_symbolicate(request).await;
    let frames = &mut response.unwrap().stacktraces[0].frames;

    assert_eq!(frames.len(), 2);

    assert_eq!(
        frames[1].raw.pre_context,
        &["function add(a, b) {", "\t\"use strict\";"]
    );
    assert_eq!(
        frames[1].raw.context_line,
        Some("\treturn a + b; // fôo".to_string())
    );
    assert_eq!(frames[1].raw.post_context, &["}", ""]);
}

#[tokio::test]
async fn test_sourcemap_embedded_source_expansion() {
    let (symbolication, _) = setup_service(|_| ());
    let srv = symbolicator_test::sourcemap_server("05_sourcemap_embedded_source_expansion");

    let stacktraces: Vec<JsProcessingRawStacktrace> = serde_json::from_str(
        r#"[{
            "frames": [
                {
                    "function": "function: \"HTMLDocument.<anonymous>\"",
                    "abs_path": "http//example.com/index.html",
                    "filename": "index.html",
                    "lineno": 283,
                    "colno": 17,
                    "in_app": false
                },
                {
                    "abs_path": "http://example.com/embedded.js",
                    "filename": "embedded.js",
                    "lineno": 1,
                    "colno": 39
                }
            ]
        }]"#,
    )
    .unwrap();

    let request = JsProcessingSymbolicateStacktraces {
        source: Arc::new(SentrySourceConfig {
            id: SourceId::new("sentry:project"),
            url: srv.url("/files/"),
            token: String::new(),
        }),
        stacktraces,
        dist: None,
    };

    let response = symbolication.js_processing_symbolicate(request).await;
    let frames = &mut response.unwrap().stacktraces[0].frames;

    assert_eq!(frames.len(), 2);

    assert_eq!(
        frames[1].raw.pre_context,
        &["function add(a, b) {", "\t\"use strict\";"]
    );
    assert_eq!(
        frames[1].raw.context_line,
        Some("\treturn a + b; // fôo".to_string())
    );
    assert_eq!(frames[1].raw.post_context, &["}", ""]);
}

// TODO(kamil): Failing due to not using original source as source context fallback.
#[ignore]
#[tokio::test]
async fn test_source_expansion() {
    let (symbolication, _) = setup_service(|_| ());
    let srv = symbolicator_test::sourcemap_server("03_source_expansion");

    let stacktraces: Vec<JsProcessingRawStacktrace> = serde_json::from_str(
        r#"[{
            "frames": [
                {
                    "abs_path": "http://example.com/foo.js",
                    "filename": "foo.js",
                    "lineno": 1,
                    "colno": 0
                },
                {
                    "abs_path": "http://example.com/foo.js",
                    "filename": "foo.js",
                    "lineno": 4,
                    "colno": 0
                }
            ]
        }]"#,
    )
    .unwrap();

    let request = JsProcessingSymbolicateStacktraces {
        source: Arc::new(SentrySourceConfig {
            id: SourceId::new("sentry:project"),
            url: srv.url("/files/"),
            token: String::new(),
        }),
        stacktraces,
        dist: None,
    };

    let response = symbolication.js_processing_symbolicate(request).await;
    let frames = &mut response.unwrap().stacktraces[0].frames;

    assert_eq!(frames.len(), 2);

    assert!(frames[0].raw.pre_context.is_empty());
    assert_eq!(frames[0].raw.context_line, Some("h".to_string()));
    assert_eq!(frames[0].raw.post_context, &["e", "l", "l", "o", " "]);

    assert_eq!(frames[1].raw.pre_context, &["h", "e", "l"]);
    assert_eq!(frames[1].raw.context_line, Some("l".to_string()));
    assert_eq!(frames[1].raw.post_context, &["o", " ", "w", "o", "r"]);
}

#[tokio::test]
async fn test_inlined_sources() {
    let (symbolication, _) = setup_service(|_| ());
    let srv = symbolicator_test::sourcemap_server("04_inlined_sources");

    let stacktraces: Vec<JsProcessingRawStacktrace> = serde_json::from_str(
        r#"[{
            "frames": [
                {
                    "abs_path": "http://example.com/test.min.js",
                    "filename": "test.js",
                    "lineno": 1,
                    "colno": 1
                }
            ]
        }]"#,
    )
    .unwrap();

    let request = JsProcessingSymbolicateStacktraces {
        source: Arc::new(SentrySourceConfig {
            id: SourceId::new("sentry:project"),
            url: srv.url("/files/"),
            token: String::new(),
        }),
        stacktraces,
        dist: None,
    };

    let response = symbolication.js_processing_symbolicate(request).await;
    let frames = &mut response.unwrap().stacktraces[0].frames;

    assert_eq!(frames.len(), 1);

    assert!(frames[0].raw.pre_context.is_empty());
    assert_eq!(
        frames[0].raw.context_line,
        Some("console.log('hello, World!')".to_string())
    );
    assert!(frames[0].raw.post_context.is_empty());
}
