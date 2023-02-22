use std::sync::Arc;

use symbolicator_service::{
    services::symbolication::JsProcessingSymbolicateStacktraces, types::JsProcessingStacktrace,
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
    let input_frames = r#"[
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
    ]"#;

    let (symbolication, _) = setup_service(|_| ());
    let srv = symbolicator_test::sourcemap_server("01_sourcemap_expansion");

    let stacktraces: Vec<JsProcessingStacktrace> =
        serde_json::from_str(&format!(r#"[{{ "frames": {input_frames} }}]"#)).unwrap();
    let request = JsProcessingSymbolicateStacktraces {
        source: Arc::new(SentrySourceConfig {
            id: SourceId::new("sentry:project"),
            url: srv.url("/files/"),
            token: String::new(),
        }),
        stacktraces,
        dist: None,
    };
    let response = symbolication
        .js_processing_symbolicate(request)
        .await
        .unwrap();

    let frames = &response.stacktraces[0].frames;
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

#[tokio::test]
async fn test_sourcemap_source_expansion() {
    let input_frames = r#"[
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
    ]"#;

    let (symbolication, _) = setup_service(|_| ());
    let srv = symbolicator_test::sourcemap_server("02_sourcemap_source_expansion");

    let stacktraces: Vec<JsProcessingStacktrace> =
        serde_json::from_str(&format!(r#"[{{ "frames": {input_frames} }}]"#)).unwrap();
    let request = JsProcessingSymbolicateStacktraces {
        source: Arc::new(SentrySourceConfig {
            id: SourceId::new("sentry:project"),
            url: srv.url("/files/"),
            token: String::new(),
        }),
        stacktraces,
        dist: None,
    };
    let response = symbolication
        .js_processing_symbolicate(request)
        .await
        .unwrap();

    let frames = &response.stacktraces[0].frames;
    let raw_frames = &response.raw_stacktraces[0].frames;
    assert_eq!(frames.len(), 2);

    // Raw frames
    let pre_context: &[String] = &[];
    let context_line = Some("function add(a,b){\"use strict\";return a+b}function multiply(a,b){\"use strict\";return a*b}function divide(a,b){\"use strict\";try{return multip {snip}".to_string());
    let post_context = &["//@ sourceMappingURL=file.min.js.map", ""];
    assert_eq!(raw_frames[1].pre_context, pre_context);
    assert_eq!(raw_frames[1].context_line, context_line);
    assert_eq!(raw_frames[1].post_context, post_context);
    assert_eq!(raw_frames[1].lineno, Some(1));
    assert_eq!(raw_frames[1].colno, Some(39));

    // Processed frames
    let pre_context = &["function add(a, b) {", "\t\"use strict\";"];
    let context_line = Some("\treturn a + b; // fôo".to_string());
    let post_context = &["}", ""];
    assert_eq!(frames[1].raw.pre_context, pre_context);
    assert_eq!(frames[1].raw.context_line, context_line);
    assert_eq!(frames[1].raw.post_context, post_context);
    assert_eq!(frames[1].raw.lineno, Some(3));
    assert_eq!(frames[1].raw.colno, Some(9));
}

#[tokio::test]
async fn test_sourcemap_embedded_source_expansion() {
    let input_frames = r#"[
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
    ]"#;

    let (symbolication, _) = setup_service(|_| ());
    let srv = symbolicator_test::sourcemap_server("03_sourcemap_embedded_source_expansion");

    let stacktraces: Vec<JsProcessingStacktrace> =
        serde_json::from_str(&format!(r#"[{{ "frames": {input_frames} }}]"#)).unwrap();
    let request = JsProcessingSymbolicateStacktraces {
        source: Arc::new(SentrySourceConfig {
            id: SourceId::new("sentry:project"),
            url: srv.url("/files/"),
            token: String::new(),
        }),
        stacktraces,
        dist: None,
    };
    let response = symbolication
        .js_processing_symbolicate(request)
        .await
        .unwrap();

    let frames = &response.stacktraces[0].frames;
    assert_eq!(frames.len(), 2);

    let pre_context = &["function add(a, b) {", "\t\"use strict\";"];
    let context_line = Some("\treturn a + b; // fôo".to_string());
    let post_context = &["}", ""];
    assert_eq!(frames[1].raw.pre_context, pre_context);
    assert_eq!(frames[1].raw.context_line, context_line);
    assert_eq!(frames[1].raw.post_context, post_context);
    assert_eq!(frames[1].raw.lineno, Some(3));
    assert_eq!(frames[1].raw.colno, Some(9));
}

#[tokio::test]
async fn test_source_expansion() {
    let input_frames = r#"[
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
    ]"#;

    let (symbolication, _) = setup_service(|_| ());
    let srv = symbolicator_test::sourcemap_server("04_source_expansion");

    let stacktraces: Vec<JsProcessingStacktrace> =
        serde_json::from_str(&format!(r#"[{{ "frames": {input_frames} }}]"#)).unwrap();
    let request = JsProcessingSymbolicateStacktraces {
        source: Arc::new(SentrySourceConfig {
            id: SourceId::new("sentry:project"),
            url: srv.url("/files/"),
            token: String::new(),
        }),
        stacktraces,
        dist: None,
    };
    let response = symbolication
        .js_processing_symbolicate(request)
        .await
        .unwrap();

    let frames = &response.stacktraces[0].frames;
    assert_eq!(frames.len(), 2);

    let pre_context: &[String] = &[];
    let context_line = Some("h".to_string());
    let post_context = &["e", "l", "l", "o", " "];
    assert_eq!(frames[0].raw.pre_context, pre_context);
    assert_eq!(frames[0].raw.context_line, context_line);
    assert_eq!(frames[0].raw.post_context, post_context);
    assert_eq!(frames[0].raw.lineno, Some(1));
    assert_eq!(frames[0].raw.colno, Some(0));

    let pre_context = &["h", "e", "l"];
    let context_line = Some("l".to_string());
    let post_context = &["o", " ", "w", "o", "r"];
    assert_eq!(frames[1].raw.pre_context, pre_context);
    assert_eq!(frames[1].raw.context_line, context_line);
    assert_eq!(frames[1].raw.post_context, post_context);
    assert_eq!(frames[1].raw.lineno, Some(4));
    assert_eq!(frames[1].raw.colno, Some(0));
}

#[tokio::test]
async fn test_inlined_sources() {
    let input_frames = r#"[
        {
            "abs_path": "http://example.com/test.min.js",
            "filename": "test.js",
            "lineno": 1,
            "colno": 1
        }
    ]"#;

    let (symbolication, _) = setup_service(|_| ());
    let srv = symbolicator_test::sourcemap_server("05_inlined_sources");

    let stacktraces: Vec<JsProcessingStacktrace> =
        serde_json::from_str(&format!(r#"[{{ "frames": {input_frames} }}]"#)).unwrap();
    let request = JsProcessingSymbolicateStacktraces {
        source: Arc::new(SentrySourceConfig {
            id: SourceId::new("sentry:project"),
            url: srv.url("/files/"),
            token: String::new(),
        }),
        stacktraces,
        dist: None,
    };
    let response = symbolication
        .js_processing_symbolicate(request)
        .await
        .unwrap();

    let frames = &response.stacktraces[0].frames;
    assert_eq!(frames.len(), 1);

    let pre_context: &[String] = &[];
    let context_line = Some("console.log('hello, World!')".to_string());
    let post_context: &[String] = &[];
    assert_eq!(frames[0].raw.pre_context, pre_context);
    assert_eq!(frames[0].raw.context_line, context_line);
    assert_eq!(frames[0].raw.post_context, post_context);
    assert_eq!(frames[0].raw.lineno, Some(1));
    assert_eq!(frames[0].raw.colno, Some(1));
}

#[tokio::test]
async fn test_sourcemap_nofiles_source_expansion() {
    let input_frames = r#"[
        {
            "abs_path": "app:///nofiles.js",
            "lineno": 1,
            "colno": 39
        }
    ]"#;

    let (symbolication, _) = setup_service(|_| ());
    let srv = symbolicator_test::sourcemap_server("06_sourcemap_nofiles_source_expansion");

    let stacktraces: Vec<JsProcessingStacktrace> =
        serde_json::from_str(&format!(r#"[{{ "frames": {input_frames} }}]"#)).unwrap();
    let request = JsProcessingSymbolicateStacktraces {
        source: Arc::new(SentrySourceConfig {
            id: SourceId::new("sentry:project"),
            url: srv.url("/files/"),
            token: String::new(),
        }),
        stacktraces,
        dist: None,
    };
    let response = symbolication
        .js_processing_symbolicate(request)
        .await
        .unwrap();

    let frames = &response.stacktraces[0].frames;
    assert_eq!(frames.len(), 1);

    let pre_context = &["function add(a, b) {", "\t\"use strict\";"];
    let context_line = Some("\treturn a + b; // fôo".to_string());
    let post_context = &["}", ""];
    assert_eq!(frames[0].raw.pre_context, pre_context);
    assert_eq!(frames[0].raw.context_line, context_line);
    assert_eq!(frames[0].raw.post_context, post_context);
    assert_eq!(frames[0].raw.lineno, Some(3));
    assert_eq!(frames[0].raw.colno, Some(9));
    assert_eq!(frames[0].raw.abs_path, "app:///nofiles.js");
}

#[tokio::test]
async fn test_indexed_sourcemap_source_expansion() {
    let input_frames = r#"[
        {
            "abs_path": "http://example.com/indexed.min.js",
            "filename": "indexed.min.js",
            "lineno": 1,
            "colno": 39
        },
        {
            "abs_path": "http://example.com/indexed.min.js",
            "filename": "indexed.min.js",
            "lineno": 2,
            "colno": 44
        }
    ]"#;

    let (symbolication, _) = setup_service(|_| ());
    let srv = symbolicator_test::sourcemap_server("07_indexed_sourcemap_source_expansion");

    let stacktraces: Vec<JsProcessingStacktrace> =
        serde_json::from_str(&format!(r#"[{{ "frames": {input_frames} }}]"#)).unwrap();
    let request = JsProcessingSymbolicateStacktraces {
        source: Arc::new(SentrySourceConfig {
            id: SourceId::new("sentry:project"),
            url: srv.url("/files/"),
            token: String::new(),
        }),
        stacktraces,
        dist: None,
    };
    let response = symbolication
        .js_processing_symbolicate(request)
        .await
        .unwrap();

    let frames = &response.stacktraces[0].frames;
    let raw_frames = &response.raw_stacktraces[0].frames;
    assert_eq!(frames.len(), 2);

    // Raw frames
    let pre_context: &[String] = &[];
    let context_line = Some("function add(a,b){\"use strict\";return a+b}".to_string());
    let post_context = &[
        "function multiply(a,b){\"use strict\";return a*b}function divide(a,b){\"use strict\";try{return multiply(add(a,b),a,b)/c}catch(e){Raven.captureE {snip}",
        "//# sourceMappingURL=indexed.min.js.map",
        "",
    ];

    assert_eq!(raw_frames[0].pre_context, pre_context);
    assert_eq!(raw_frames[0].context_line, context_line);
    assert_eq!(raw_frames[0].post_context, post_context);
    assert_eq!(raw_frames[0].lineno, Some(1));
    assert_eq!(raw_frames[0].colno, Some(39));

    let pre_context = &["function add(a,b){\"use strict\";return a+b}"];
    let context_line = Some("function multiply(a,b){\"use strict\";return a*b}function divide(a,b){\"use strict\";try{return multiply(add(a,b),a,b)/c}catch(e){Raven.captureE {snip}".to_string());
    let post_context = &["//# sourceMappingURL=indexed.min.js.map", ""];
    assert_eq!(raw_frames[1].pre_context, pre_context);
    assert_eq!(raw_frames[1].context_line, context_line);
    assert_eq!(raw_frames[1].post_context, post_context);
    assert_eq!(raw_frames[1].lineno, Some(2));
    assert_eq!(raw_frames[1].colno, Some(44));

    // Processed frames
    let pre_context = &["function add(a, b) {", "\t\"use strict\";"];
    let context_line = Some("\treturn a + b; // fôo".to_string());
    let post_context = &["}", ""];
    assert_eq!(frames[0].raw.pre_context, pre_context);
    assert_eq!(frames[0].raw.context_line, context_line);
    assert_eq!(frames[0].raw.post_context, post_context);
    assert_eq!(frames[0].raw.lineno, Some(3));
    assert_eq!(frames[0].raw.colno, Some(9));

    let pre_context = &["function multiply(a, b) {", "\t\"use strict\";"];
    let context_line = Some("\treturn a * b;".to_string());
    let post_context = &[
        "}",
        "function divide(a, b) {",
        "\t\"use strict\";",
        "\ttry {",
        "\t\treturn multiply(add(a, b), a, b) / c;",
    ];
    assert_eq!(frames[1].raw.pre_context, pre_context);
    assert_eq!(frames[1].raw.context_line, context_line);
    assert_eq!(frames[1].raw.post_context, post_context);
    assert_eq!(frames[1].raw.lineno, Some(3));
    assert_eq!(frames[1].raw.colno, Some(9));
}
