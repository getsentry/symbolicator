use std::str::FromStr;
use std::sync::Arc;

use reqwest::Url;
use serde_json::json;
use symbolicator_js::interface::{JsFrame, JsModule, JsStacktrace, SymbolicateJsStacktraces};
use symbolicator_service::types::{Scope, ScrapingConfig};
use symbolicator_sources::{SentrySourceConfig, SourceId};

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
    modules: &str,
    release: impl Into<Option<String>>,
    dist: impl Into<Option<String>>,
) -> SymbolicateJsStacktraces {
    let frames: Vec<JsFrame> = serde_json::from_str(frames).unwrap();
    let modules: Vec<JsModule> = serde_json::from_str(modules).unwrap();
    let stacktraces = vec![JsStacktrace { frames }];

    SymbolicateJsStacktraces {
        platform: None,
        scope: Scope::Global,
        source: Arc::new(source),
        release: release.into(),
        dist: dist.into(),
        scraping: ScrapingConfig {
            enabled: false,
            ..Default::default()
        },
        apply_source_context: true,

        stacktraces,
        modules,
    }
}

fn sourcemap_server<L>(
    fixtures_dir: &str,
    lookup: L,
) -> (symbolicator_test::Server, SentrySourceConfig)
where
    L: Fn(&str, &str) -> serde_json::Value + Clone + Send + 'static,
{
    let fixtures_dir = symbolicator_test::fixture(format!("sourcemaps/{fixtures_dir}"));
    symbolicator_test::sourcemap_server(fixtures_dir, lookup)
}

#[tokio::test]
async fn test_sourcemap_expansion() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) = sourcemap_server("01_sourcemap_expansion", |url, _query| {
        json!([{
            "type": "file",
            "id": "1",
            "url": format!("{url}/test.min.js"),
            "abs_path": "~/test.min.js",
            "resolved_with": "release",
        }, {
            "type": "file",
            "id": "2",
            "url": format!("{url}/test.min.js.map"),
            "abs_path": "~/test.min.js.map",
            "resolved_with": "release",
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
        "abs_path": "async http://example.com/test.min.js",
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

    let request = make_js_request(source, frames, "[]", String::from("release"), None);
    let response = symbolication.symbolicate_js(request).await;

    assert_snapshot!(response);
}

#[tokio::test]
async fn test_sourcemap_source_expansion() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) = sourcemap_server("02_sourcemap_source_expansion", |url, _query| {
        json!([{
            "type": "file",
            "id": "1",
            "url": format!("{url}/file.min.js"),
            "abs_path": "~/file.min.js",
            "resolved_with": "release-old",
        }, {
            "type": "file",
            "id": "2",
            "url": format!("{url}/file.min.js.map"),
            "abs_path": "~/file.min.js.map",
            "resolved_with": "release-old",
        }, {
            "type": "file",
            "id": "3",
            "url": format!("{url}/file1.js"),
            "abs_path": "~/file1.js",
            "resolved_with": "release-old",
        }, {
            "type": "file",
            "id": "4",
            "url": format!("{url}/file2.js"),
            "abs_path": "~/file2.js",
            "resolved_with": "release-old",
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

    let request = make_js_request(source, frames, "[]", String::from("release"), None);
    let response = symbolication.symbolicate_js(request).await;

    assert_snapshot!(response);
}

#[tokio::test]
async fn test_sourcemap_embedded_source_expansion() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) =
        sourcemap_server("03_sourcemap_embedded_source_expansion", |url, _query| {
            json!([{
                "type": "file",
                "id": "1",
                "url": format!("{url}/embedded.js"),
                "abs_path": "~/embedded.js",
                "resolved_with": "release",
            }, {
                "type": "file",
                "id": "2",
                "url": format!("{url}/embedded.js.map"),
                "abs_path": "~/embedded.js.map",
                "resolved_with": "release",
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
        "abs_path": "http://example.com/embedded.js",
        "filename": "embedded.js",
        "lineno": 1,
        "colno": 39
    }]"#;

    let request = make_js_request(source, frames, "[]", String::from("release"), None);
    let response = symbolication.symbolicate_js(request).await;

    assert_snapshot!(response);
}

#[tokio::test]
async fn test_source_expansion() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) = sourcemap_server("04_source_expansion", |url, _query| {
        json!([{
            "type": "file",
            "id": "1",
            "url": format!("{url}/foo.js"),
            "abs_path": "~/foo.js",
            "resolved_with": "release-old",
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

    let request = make_js_request(source, frames, "[]", String::from("release"), None);
    let response = symbolication.symbolicate_js(request).await;

    assert_snapshot!(response);
}

#[tokio::test]
async fn test_inlined_sources() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) = sourcemap_server("05_inlined_sources", |url, _query| {
        json!([{
            "type": "file",
            "id": "1",
            "url": format!("{url}/test.min.js"),
            "abs_path": "~/test.min.js",
            "resolved_with": "release",
        }])
    });

    let frames = r#"[{
        "abs_path": "http://example.com/test.min.js",
        "filename": "test.js",
        "lineno": 1,
        "colno": 1
    }]"#;

    let request = make_js_request(source, frames, "[]", String::from("release"), None);
    let response = symbolication.symbolicate_js(request).await;

    assert_snapshot!(response);
}

#[tokio::test]
async fn test_sourcemap_nofiles_source_expansion() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) =
        sourcemap_server("06_sourcemap_nofiles_source_expansion", |url, _query| {
            json!([{
                "type": "file",
                "id": "1",
                "url": format!("{url}/nofiles.js"),
                "abs_path": "~/nofiles.js",
                "resolved_with": "release",
            }, {
                "type": "file",
                "id": "2",
                "url": format!("{url}/nofiles.js.map"),
                "abs_path": "~/nofiles.js.map",
                "resolved_with": "release",
            }])
        });

    let frames = r#"[{
        "abs_path": "app:///nofiles.js",
        "lineno": 1,
        "colno": 39
    }]"#;

    let request = make_js_request(source, frames, "[]", String::from("release"), None);
    let response = symbolication.symbolicate_js(request).await;

    assert_snapshot!(response);
}

#[tokio::test]
async fn test_indexed_sourcemap_source_expansion() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) =
        sourcemap_server("07_indexed_sourcemap_source_expansion", |url, _query| {
            json!([{
                "type": "file",
                "id": "1",
                "url": format!("{url}/indexed.min.js"),
                "abs_path": "~/indexed.min.js",
                "resolved_with": "release-old",
            }, {
                "type": "file",
                "id": "2",
                "url": format!("{url}/indexed.min.js.map"),
                "abs_path": "~/indexed.min.js.map",
                "resolved_with": "release-old",
            }, {
                "type": "file",
                "id": "3",
                "url": format!("{url}/file1.js"),
                "abs_path": "~/file1.js",
                "resolved_with": "release-old",
            }, {
                "type": "file",
                "id": "4",
                "url": format!("{url}/file2.js"),
                "abs_path": "~/file2.js",
                "resolved_with": "release-old",
            }])
        });

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

    let request = make_js_request(source, frames, "[]", String::from("release"), None);
    let response = symbolication.symbolicate_js(request).await;

    assert_snapshot!(response);
}

#[tokio::test]
async fn test_malformed_abs_path() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) = sourcemap_server("", |_url, _query| json!([]));

    // Missing colon was removed on purpose.
    let frames = r#"[{
        "abs_path": "http//example.com/test.min.js",
        "filename": "test.js",
        "lineno": 1,
        "colno": 1
    }]"#;

    let request = make_js_request(source, frames, "[]", String::from("release"), None);
    let response = symbolication.symbolicate_js(request).await;

    assert_snapshot!(response);
}

#[tokio::test]
async fn test_fetch_error() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (srv, source) = sourcemap_server("", |_url, _query| json!([]));

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

    let mut request = make_js_request(source, &frames, "[]", String::from("release"), None);
    request.scraping.enabled = true;
    let response = symbolication.symbolicate_js(request).await;

    assert_snapshot!(response);
}

#[tokio::test]
async fn test_invalid_location() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) = sourcemap_server("08_sourcemap_invalid_location", |url, _query| {
        json!([{
            "type": "file",
            "id": "1",
            "url": format!("{url}/invalidlocation.js"),
            "abs_path": "~/invalidlocation.js",
            "resolved_with": "release-old",
        }, {
            "type": "file",
            "id": "2",
            "url": format!("{url}/invalidlocation.js.map"),
            "abs_path": "~/invalidlocation.js.map",
            "resolved_with": "release-old",
        }])
    });

    let frames = r#"[{
        "abs_path": "http://example.com/invalidlocation.js",
        "filename": "invalidlocation.js",
        "lineno": 0,
        "colno": 0,
        "function": "e"
    }]"#;

    let request = make_js_request(source, frames, "[]", String::from("release"), None);
    let response = symbolication.symbolicate_js(request).await;

    assert_snapshot!(response);
}

#[tokio::test]
async fn test_webpack() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) = sourcemap_server("09_webpack", |url, _query| {
        json!([{
            "type": "file",
            "id": "1",
            "url": format!("{url}/test1.min.js"),
            "abs_path": "~/test1.min.js",
            "resolved_with": "release-old",
        }, {
            "type": "file",
            "id": "2",
            "url": format!("{url}/test1.min.js.map"),
            "abs_path": "~/test1.min.js.map",
            "resolved_with": "release-old",
        }, {
            "type": "file",
            "id": "3",
            "url": format!("{url}/test2.min.js"),
            "abs_path": "~/test2.min.js",
            "resolved_with": "release-old",
        }, {
            "type": "file",
            "id": "4",
            "url": format!("{url}/test2.min.js.map"),
            "abs_path": "~/test2.min.js.map",
            "resolved_with": "release-old",
        }])
    });

    let frames = r#"[{
        "abs_path": "http://example.com/test1.min.js",
        "filename": "test1.min.js",
        "lineno": 1,
        "colno": 183,
        "function": "i"
    }, {
        "abs_path": "http://example.com/test2.min.js",
        "filename": "test2.min.js",
        "lineno": 1,
        "colno": 183,
        "function": "i"
    }]"#;

    let request = make_js_request(source, frames, "[]", String::from("release"), None);
    let response = symbolication.symbolicate_js(request).await;

    assert_snapshot!(response);
}

#[tokio::test]
async fn test_dart_async_name() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) = sourcemap_server("10_dart_async_name", |url, _query| {
        json!([{
            "type": "file",
            "id": "1",
            "url": format!("{url}/out.js"),
            "abs_path": "~/out.js",
            "resolved_with": "release",
        }, {
            "type": "file",
            "id": "2",
            "url": format!("{url}/out.js.map"),
            "abs_path": "~/out.js.map",
            "resolved_with": "release",
        }])
    });

    let frames = r#"[{
        "abs_path": "http://example.com/out.js",
        "filename": "out.js",
        "lineno": 1314,
        "colno": 16,
        "function": "<fn>"
    }]"#;

    let request = make_js_request(source, frames, "[]", String::from("release"), None);
    let response = symbolication.symbolicate_js(request).await;

    assert_eq!(
        response.stacktraces[0].frames[0].function,
        // Without implemented workaround, it would yield `$async$be` here.
        // We want to assert that it uses token name instead of scope name in case of async rewrite.
        Some("main".into())
    );
}

#[tokio::test]
async fn test_no_source_contents() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) = sourcemap_server("11_no_source_contents", |url, _query| {
        json!([{
            "type": "file",
            "id": "1",
            "url": format!("{url}/embedded.js"),
            "abs_path": "~/embedded.js",
            "resolved_with": "release",
        }, {
            "type": "file",
            "id": "2",
            "url": format!("{url}/embedded.js.map"),
            "abs_path": "~/embedded.js.map",
            "resolved_with": "release",
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
        "abs_path": "http://example.com/embedded.js",
        "filename": "embedded.js",
        "lineno": 1,
        "colno": 39
    }]"#;

    let request = make_js_request(source, frames, "[]", String::from("release"), None);
    let response = symbolication.symbolicate_js(request).await;

    // The sourcemap doesn't contain source context, so the symbolicated frame shouldn't have source context.
    assert_snapshot!(response);
}

#[tokio::test]
async fn e2e_node_debugid() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) = sourcemap_server("e2e_node_debugid", |url, query| {
        assert_eq!(query, "debug_id=2f259f80-58b7-44cb-d7cd-de1505e7e718");
        json!([{
            "type": "bundle",
            "id": "1",
            "url": format!("{url}/bundle.zip"),
            "resolved_with": "debug-id",
        }])
    });

    let frames = r#"[{
        "abs_path": "/Users/lucaforstner/code/github/getsentry/sentry-javascript-bundler-plugins/packages/playground/öut path/rollup/entrypoint1.js",
        "lineno": 73,
        "colno": 36,
        "function": "Object.<anonymous>"
    }]"#;
    let modules = r#"[{
        "code_file": "/Users/lucaforstner/code/github/getsentry/sentry-javascript-bundler-plugins/packages/playground/öut path/rollup/entrypoint1.js",
        "debug_id": "2f259f80-58b7-44cb-d7cd-de1505e7e718",
        "type": "sourcemap"
    }]"#;

    let request = make_js_request(source, frames, modules, None, None);
    let response = symbolication.symbolicate_js(request).await;

    assert_snapshot!(response);
}

#[tokio::test]
async fn e2e_source_no_header() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) = sourcemap_server("e2e_source_no_header", |url, _query| {
        // NOTE: this bundle was manually constructed and has a `"source"` file without a
        // `Sourcemap` header, but with a `sourceMappingURL`.
        // The test files are the same as in `01_sourcemap_expansion`, but with a different
        // base url.
        json!([{
            "type": "bundle",
            "id": "1",
            "url": format!("{url}/bundle.zip"),
            "resolved_with": "release",
        }])
    });

    let frames = r#"[{
        "abs_path": "app:///_next/server/pages/_error.js",
        "lineno": 1,
        "colno": 64,
        "function": "e"
    }]"#;
    let modules = r#"[]"#;

    let request = make_js_request(source, frames, modules, String::from("some-release"), None);
    let response = symbolication.symbolicate_js(request).await;

    assert_snapshot!(response);
}

#[tokio::test]
async fn e2e_react_native() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) = sourcemap_server("e2e_react_native", |url, _query| {
        json!([{
            "type": "file",
            "id": "1",
            "url": format!("{url}/index.android.bundle"),
            "abs_path": "~/index.android.bundle",
            "headers": { "SoUrCemAp": "index.android.bundle.map" },
            "resolved_with": "release",
        }, {
            "type": "file",
            "id": "2",
            "url": format!("{url}/index.android.bundle.map"),
            "abs_path": "~/index.android.bundle.map",
            "headers": {},
            "resolved_with": "release",
        }])
    });

    let frames = r#"[{
        "abs_path": "app:///index.android.bundle",
        "lineno": 1,
        "colno": 11940
    }]"#;
    let modules = r#"[]"#;

    let request = make_js_request(source, frames, modules, String::from("some-release"), None);
    let response = symbolication.symbolicate_js(request).await;

    assert_snapshot!(response);
}

#[tokio::test]
async fn e2e_multiple_smref_scraped() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (srv, source) = sourcemap_server("e2e_multiple_smref_scraped", |_url, _query| json!([]));

    let url = srv.url("/files/");
    let frames = format!(
        r#"[{{
        "abs_path": "{url}app.js",
        "lineno": 1,
        "colno": 64
    }}]"#
    );
    let modules = r#"[]"#;

    let mut request = make_js_request(source, &frames, modules, None, None);
    request.scraping.enabled = true;

    let response = symbolication.symbolicate_js(request).await;

    assert_snapshot!(response);
}

#[tokio::test]
async fn e2e_scraped_debugid() {
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (srv, source) = sourcemap_server("e2e_scraped_debugid", |url, query| {
        assert_eq!(query, "debug_id=2f259f80-58b7-44cb-d7cd-de1505e7e718");
        json!([{
            "type": "bundle",
            "id": "1",
            "url": format!("{url}/bundle.zip"),
            "resolved_with": "debug-id",
        }])
    });

    let url = srv.url("/files/");
    let frames = format!(
        r#"[{{
        "abs_path": "{url}app.js",
        "lineno": 1,
        "colno": 64
    }}]"#
    );
    let modules = r#"[]"#;

    let mut request = make_js_request(source, &frames, modules, None, None);
    request.scraping.enabled = true;

    let response = symbolication.symbolicate_js(request).await;

    assert_snapshot!(response);
}

#[tokio::test]
async fn sorted_bundles() {
    let (symbolication, _cache_dir) = setup_service(|_| ());

    let (_srv, source) = sourcemap_server("sorted_bundles", |url, _query| {
        json!([{
            "type": "bundle",
            "id": "1",
            "url": format!("{url}/01_wrong.zip"),
        }, {
            "type": "bundle",
            "id": "2",
            "url": format!("{url}/02_correct.zip"),
        }])
    });

    let frames = r#"[{
        "abs_path": "http://example.com/test.min.js",
        "lineno": 1,
        "colno": 183
    }]"#;
    let modules = r#"[]"#;

    let request = make_js_request(source, frames, modules, String::from("some-release"), None);
    let response = symbolication.symbolicate_js(request).await;

    assert_eq!(
        response.stacktraces[0].frames[0].function,
        // The `01_wrong` bundle would yield `thisIsWrong` here.
        // We want to assert that bundles have stable sort order according to their `url`.
        // The `url` contains their `id` as it comes from sentry. This is the best we can do right now.
        Some("test".into())
    );
}

// A manually triggered test that can be used to locally debug monolith behavior. Requires a list
// of frames, modules, release/dist pair and valid token, which can all be obtained from the event's
// JSON payload. We can modify this util to pull the data from JSON API directly if we use it more often.
//
// Run with:
// RUST_LOG=debug cargo test --package 'symbolicator-service' --test 'integration' -- 'sourcemap::test_manual_processing' --include-ignored
#[tokio::test]
#[ignore]
async fn test_manual_processing() {
    let token = "token";
    let org = "org";
    let project = "project";
    let release = None;
    let dist = None;
    let frames = r#"[{
        "function": "x",
        "module": "x",
        "filename": "x",
        "abs_path": "x",
        "lineno": 1,
        "colno": 1,
        "in_app": true
    }]"#;
    let modules = r#"[{
        "code_file": "x",
        "debug_id": "x",
        "type": "x"
    }]"#;

    let (symbolication, _) = setup_service(|_| ());
    let source = SentrySourceConfig {
        id: SourceId::new("sentry:project"),
        url: Url::from_str(&format!(
            "https://sentry.io/api/0/projects/{org}/{project}/artifact-lookup/"
        ))
        .unwrap(),
        token: token.to_string(),
    };

    let request = make_js_request(source, frames, modules, release, dist);
    let response = symbolication.symbolicate_js(request).await;

    assert_snapshot!(response);
}
