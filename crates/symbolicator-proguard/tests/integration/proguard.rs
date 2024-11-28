use std::sync::Arc;
use std::{collections::HashMap, str::FromStr};

use serde_json::json;
use symbolic::common::{DebugId, Uuid};
use symbolicator_proguard::interface::{
    JvmFrame, JvmModule, JvmStacktrace, SymbolicateJvmStacktraces,
};
use symbolicator_service::types::Scope;
use symbolicator_sources::{SentrySourceConfig, SourceConfig};
use symbolicator_test::assert_snapshot;

use crate::setup_service;

fn make_jvm_request(
    source: SourceConfig,
    exception: &str,
    frames: &str,
    modules: &str,
    release_package: impl Into<Option<String>>,
) -> SymbolicateJvmStacktraces {
    let exceptions = vec![serde_json::from_str(exception).unwrap()];
    let frames: Vec<JvmFrame> = serde_json::from_str(frames).unwrap();
    let modules: Vec<JvmModule> = serde_json::from_str(modules).unwrap();
    let stacktraces = vec![JvmStacktrace { frames }];

    SymbolicateJvmStacktraces {
        platform: None,
        scope: Scope::Global,
        sources: Arc::new([source]),
        apply_source_context: true,
        release_package: release_package.into(),

        exceptions,
        stacktraces,
        modules,
        classes: Vec::new(),
    }
}

fn proguard_server<L>(
    fixtures_dir: &str,
    lookup: L,
) -> (symbolicator_test::Server, SentrySourceConfig)
where
    L: Fn(&str, &HashMap<String, String>) -> serde_json::Value + Clone + Send + 'static,
{
    let fixtures_dir = symbolicator_test::fixture(format!("proguard/{fixtures_dir}"));
    symbolicator_test::sentry_server(fixtures_dir, lookup)
}

#[tokio::test]
async fn test_download_proguard_file() {
    symbolicator_test::setup();
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) = proguard_server("download_proguard_file", |_url, _query| {
        json!([{
            "id":"proguard.txt",
            "uuid":"246fb328-fc4e-406a-87ff-fc35f6149d8f",
            "debugId":"246fb328-fc4e-406a-87ff-fc35f6149d8f",
            "codeId":null,
            "cpuName":"any",
            "objectName":"proguard-mapping",
            "symbolType":"proguard",
            "headers": {
                "Content-Type":"text/x-proguard+plain"
            },
            "size":3619,
            "sha1":"deba83e73fd18210a830db372a0e0a2f2293a989",
            "dateCreated":"2024-02-14T10:49:38.770116Z",
            "data":{
                "features":["mapping"]
            }
        }])
    });

    let source = SourceConfig::Sentry(Arc::new(source));
    let debug_id = DebugId::from_str("246fb328-fc4e-406a-87ff-fc35f6149d8f").unwrap();

    assert!(symbolication
        .download_proguard_file(&[source], &Scope::Global, debug_id)
        .await
        .is_ok());
}

#[tokio::test]
async fn test_remap_exception() {
    symbolicator_test::setup();
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) = proguard_server("remap_exception", |_url, _query| {
        json!([{
            "id":"proguard.txt",
            "uuid":"246fb328-fc4e-406a-87ff-fc35f6149d8f",
            "debugId":"246fb328-fc4e-406a-87ff-fc35f6149d8f",
            "codeId":null,
            "cpuName":"any",
            "objectName":"proguard-mapping",
            "symbolType":"proguard",
            "headers": {
                "Content-Type":"text/x-proguard+plain"
            },
            "size":3619,
            "sha1":"deba83e73fd18210a830db372a0e0a2f2293a989",
            "dateCreated":"2024-02-14T10:49:38.770116Z",
            "data":{
                "features":["mapping"]
            }
        }])
    });

    let source = SourceConfig::Sentry(Arc::new(source));

    let request = make_jvm_request(
        source,
        r#"{"type": "g$a", "module": "org.a.b"}"#,
        "[]",
        r#"[{"uuid": "246fb328-fc4e-406a-87ff-fc35f6149d8f", "type": "proguard"}]"#,
        None,
    );

    let response = symbolication.symbolicate_jvm(request).await;

    assert_snapshot!(response);
}

#[tokio::test]
async fn test_resolving_inline() {
    symbolicator_test::setup();
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) = proguard_server("resolving_inline", |_url, _query| {
        json!([{
            "id":"proguard.txt",
            "uuid":"246fb328-fc4e-406a-87ff-fc35f6149d8f",
            "debugId":"246fb328-fc4e-406a-87ff-fc35f6149d8f",
            "codeId":null,
            "cpuName":"any",
            "objectName":"proguard-mapping",
            "symbolType":"proguard",
            "headers": {
                "Content-Type":"text/x-proguard+plain"
            },
            "size":3619,
            "sha1":"deba83e73fd18210a830db372a0e0a2f2293a989",
            "dateCreated":"2024-02-14T10:49:38.770116Z",
            "data":{
                "features":["mapping"]
            }
        }])
    });

    let source = SourceConfig::Sentry(Arc::new(source));
    let frames = r#"[{
        "function": "onClick",
        "module": "e.a.c.a",
        "lineno": 2,
        "index": 0
    }, {
        "function": "t",
        "module": "io.sentry.sample.MainActivity",
        "filename": "MainActivity.java",
        "lineno": 1,
        "index": 1
    }]"#;

    let request = make_jvm_request(
        source,
        r#"{"type": "g$a", "module": "org.a.b"}"#,
        frames,
        r#"[{"uuid": "246fb328-fc4e-406a-87ff-fc35f6149d8f", "type": "proguard"}]"#,
        None,
    );

    let response = symbolication.symbolicate_jvm(request).await;

    assert_snapshot!(response);
}

#[tokio::test]
async fn test_basic_source_lookup() {
    symbolicator_test::setup();
    let (symbolication, _cache_dir) = setup_service(|_| ());
    let (_srv, source) = proguard_server("basic_source_lookup", |_url, _query| {
        json!([{
            "id":"bundle.zip",
            "uuid":"246fb328-fc4e-406a-87ff-fc35f6149d8f",
            "debugId":"246fb328-fc4e-406a-87ff-fc35f6149d8f",
            "codeId":null,
            "cpuName":"any",
            "objectName":"proguard-mapping",
            "symbolType":"sourcebundle",
            "headers": {
                "Content-Type":"application/x-sentry-bundle+zip"
            },
            "size":3619,
            "sha1":"deba83e73fd18210a830db372a0e0a2f2293a989",
            "dateCreated":"2024-02-14T10:49:38.770116Z",
        }])
    });

    let source = SourceConfig::Sentry(Arc::new(source));

    let frames = r#"[{
        "function": "otherMethod",
        "abs_path": "OtherActivity.java",
        "module": "OtherActivity",
        "filename": "OtherActivity.java",
        "lineno": 100,
        "index": 0
    }, {
        "function": "differentMethod",
        "abs_path": "DifferentActivity",
        "module": "DifferentActivity",
        "filename": "DifferentActivity",
        "lineno": 200,
        "index": 1
    }, {
        "function": "onCreate",
        "module": "io.sentry.samples.MainActivity",
        "lineno": 11,
        "index": 2
    }, {
        "function": "whoops",
        "abs_path": "MainActivity.kt",
        "module": "io.sentry.samples.MainActivity$InnerClass",
        "filename": "MainActivity.kt",
        "lineno": 20,
        "index": 3
    }, {
        "function": "whoops2",
        "module": "io.sentry.samples.MainActivity$AnotherInnerClass",
        "lineno": 26,
        "index": 4
    }, {
        "function": "whoops3",
        "abs_path": "MainActivity.kt",
        "module": "io.sentry.samples.MainActivity$AdditionalInnerClass",
        "filename": "MainActivity.kt",
        "lineno": 32,
        "index": 5
    }, {
        "function": "whoops4",
        "abs_path": "SourceFile",
        "module": "io.sentry.samples.MainActivity$OneMoreInnerClass",
        "filename": "SourceFile",
        "lineno": 38,
        "index": 6
    }]"#;

    let request = make_jvm_request(
        source,
        r#"{"type": "RuntimeException", "module": "io.sentry.samples"}"#,
        frames,
        r#"[{"uuid": "246fb328-fc4e-406a-87ff-fc35f6149d8f", "type": "source"}]"#,
        None,
    );

    let response = symbolication.symbolicate_jvm(request).await;

    assert_snapshot!(response);
}

#[tokio::test]
async fn test_source_lookup_with_proguard() {
    symbolicator_test::setup();
    let (symbolication, _cache_dir) = setup_service(|_| ());

    let proguard_id = "05d96b1c-1786-477c-8615-d3cf83e027c7".parse().unwrap();
    let missing_proguard_id: DebugId = "8236f5cf-52c8-4e35-a7cf-01421e4c2c88".parse().unwrap();
    let source_id1 = DebugId::from_uuid(Uuid::new_v4());
    let source_id2 = DebugId::from_uuid(Uuid::new_v4());

    let (_srv, source) = proguard_server("source_lookup_with_proguard", move |_url, query| {
        let debug_id: DebugId = query["debug_id"].parse().unwrap();

        match debug_id {
            _ if debug_id == source_id1 => {
                json!([{
                    "id":"edit_activity.zip",
                    "debugId":source_id1,
                    "codeId":null,
                    "cpuName":"any",
                    "objectName":"proguard-mapping",
                    "symbolType":"sourcebundle",
                    "headers": {
                        "Content-Type":"application/x-sentry-bundle+zip"
                    },
                    "size":3619,
                    "sha1":"deba83e73fd18210a830db372a0e0a2f2293a989",
                    "dateCreated":"2024-02-14T10:49:38.770116Z",
                }])
            }
            _ if debug_id == source_id2 => {
                json!([{
                    "id":"some_service.zip",
                    "debugId":source_id2,
                    "codeId":null,
                    "cpuName":"any",
                    "objectName":"proguard-mapping",
                    "symbolType":"sourcebundle",
                    "headers": {
                        "Content-Type":"application/x-sentry-bundle+zip"
                    },
                    "size":3619,
                    "sha1":"deba83e73fd18210a830db372a0e0a2f2293a989",
                    "dateCreated":"2024-02-14T10:49:38.770116Z",
                }])
            }
            _ if debug_id == proguard_id => {
                json!([{
                    "id":"proguard.txt",
                    "uuid":proguard_id,
                    "debugId":proguard_id,
                    "codeId":null,
                    "cpuName":"any",
                    "objectName":"proguard-mapping",
                    "symbolType":"proguard",
                    "headers": {
                        "Content-Type":"text/x-proguard+plain"
                    },
                    "size":3619,
                    "sha1":"deba83e73fd18210a830db372a0e0a2f2293a989",
                    "dateCreated":"2024-02-14T10:49:38.770116Z",
                    "data":{
                        "features":["mapping"]
                    }
                }])
            }
            _ => json!([]),
        }
    });

    let source = SourceConfig::Sentry(Arc::new(source));

    let frames = r#"[{
        "filename": "ZygoteInit.java",
        "function": "main",
        "module": "com.android.internal.os.ZygoteInit",
        "lineno": 698,
        "index": 0
    }, {
        "filename": "ZygoteInit.java",
        "function": "run",
        "module": "com.android.internal.os.ZygoteInit$MethodAndArgsCaller",
        "lineno": 903,
        "index": 1
    }, {
        "filename": "Method.java",
        "function": "invoke",
        "module": "java.lang.reflect.Method",
        "lineno": 372,
        "index": 2
    }, {
        "filename": "Method.java",
        "function": "invoke",
        "module": "java.lang.reflect.Method",
        "index": 3
    }, {
        "filename": "ActivityThread.java",
        "function": "main",
        "module": "android.app.ActivityThread",
        "lineno": 5254,
        "index": 4
    }, {
        "filename": "Looper.java",
        "function": "loop",
        "module": "android.os.Looper",
        "lineno": 135,
        "index": 5
    }, {
        "filename": "Handler.java",
        "function": "dispatchMessage",
        "module": "android.os.Handler",
        "lineno": 95,
        "index": 6
    }, {
        "filename": "Handler.java",
        "function": "handleCallback",
        "module": "android.os.Handler",
        "lineno": 739,
        "index": 7
    }, {
        "filename": "View.java",
        "function": "run",
        "module": "android.view.View$PerformClick",
        "lineno": 19866,
        "index": 8
    }, {
        "filename": "View.java",
        "function": "performClick",
        "module": "android.view.View",
        "lineno": 4780,
        "index": 9
    }, {
        "filename": "ActionMenuItemView.java",
        "function": "onClick",
        "module": "androidx.appcompat.view.menu.ActionMenuItemView",
        "lineno": 7,
        "index": 10
    }, {
        "filename": "ActionMenuView.java",
        "function": "invokeItem",
        "module": "androidx.appcompat.widget.ActionMenuView",
        "lineno": 4,
        "index": 11
    }, {
        "filename": "MenuBuilder.java",
        "function": "performItemAction",
        "module": "androidx.appcompat.view.menu.MenuBuilder",
        "lineno": 1,
        "index": 12
    }, {
        "filename": "MenuBuilder.java",
        "function": "performItemAction",
        "module": "androidx.appcompat.view.menu.MenuBuilder",
        "lineno": 4,
        "index": 13
    }, {
        "filename": "MenuItemImpl.java",
        "function": "invoke",
        "module": "androidx.appcompat.view.menu.MenuItemImpl",
        "lineno": 15,
        "index": 14
    }, {
        "filename": "MenuBuilder.java",
        "function": "dispatchMenuItemSelected",
        "module": "androidx.appcompat.view.menu.MenuBuilder",
        "lineno": 5,
        "index": 15
    }, {
        "filename": "ActionMenuView.java",
        "function": "onMenuItemSelected",
        "module": "androidx.appcompat.widget.ActionMenuView$MenuBuilderCallback",
        "lineno": 7,
        "index": 16
    }, {
        "filename": "Toolbar.java",
        "function": "onMenuItemClick",
        "module": "androidx.appcompat.widget.Toolbar$1",
        "lineno": 7,
        "index": 17
    }, {
        "filename": "R8$$SyntheticClass",
        "function": "onMenuItemClick",
        "module": "io.sentry.samples.instrumentation.ui.g",
        "lineno": 40,
        "in_app": true,
        "index": 18
    }]"#;

    let modules = format!(
        r#"[{{
        "type": "source",
        "uuid": "{source_id1}"
    }}, {{
        "type": "source",
        "uuid": "{source_id2}"
    }}, {{
        "type": "proguard",
        "uuid": "{proguard_id}"
    }}, {{
        "type": "proguard",
        "uuid": "{missing_proguard_id}"
    }}]"#
    );

    let request = make_jvm_request(
        source,
        r#"{"type": "RuntimeException", "module": "java.lang"}"#,
        frames,
        &modules,
        None,
    );

    let response = symbolication.symbolicate_jvm(request).await;

    assert_snapshot!(response);
}
