use std::sync::Arc;
use std::{collections::HashMap, str::FromStr};

use serde_json::json;
use symbolic::common::{DebugId, Uuid};
use symbolicator_proguard::interface::{
    CompletedJvmSymbolicationResponse, JvmException, JvmFrame, JvmModule, JvmModuleType,
    JvmStacktrace, SymbolicateJvmStacktraces,
};
use symbolicator_service::types::Scope;
use symbolicator_sources::{SentrySourceConfig, SourceConfig};

use crate::setup_service;

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
    let debug_id = DebugId::from_str("246fb328-fc4e-406a-87ff-fc35f6149d8f").unwrap();

    let exception = JvmException {
        ty: "g$a".into(),
        module: "org.a.b".into(),
    };

    let request = SymbolicateJvmStacktraces {
        scope: Scope::Global,
        sources: Arc::new([source]),
        exceptions: vec![exception.clone()],
        stacktraces: vec![],
        modules: vec![JvmModule {
            uuid: debug_id,
            r#type: JvmModuleType::Proguard,
        }],
        apply_source_context: false,
        release_package: None,
    };

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
    let debug_id = DebugId::from_str("246fb328-fc4e-406a-87ff-fc35f6149d8f").unwrap();

    let exception = JvmException {
        ty: "g$a".into(),
        module: "org.a.b".into(),
    };

    let stacktraces = vec![JvmStacktrace {
        frames: vec![
            JvmFrame {
                function: "onClick".into(),
                module: "e.a.c.a".into(),
                lineno: 2,
                index: 0,
                ..Default::default()
            },
            JvmFrame {
                function: "t".into(),
                module: "io.sentry.sample.MainActivity".into(),
                filename: Some("MainActivity.java".into()),
                lineno: 1,
                index: 1,
                ..Default::default()
            },
        ],
    }];

    let request = SymbolicateJvmStacktraces {
        scope: Scope::Global,
        sources: Arc::new([source]),
        exceptions: vec![exception.clone()],
        stacktraces,
        modules: vec![JvmModule {
            uuid: debug_id,
            r#type: JvmModuleType::Proguard,
        }],
        apply_source_context: false,
        release_package: None,
    };

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
    let debug_id = DebugId::from_str("246fb328-fc4e-406a-87ff-fc35f6149d8f").unwrap();

    let exception = JvmException {
        ty: "RuntimeException".into(),
        module: "io.sentry.samples".into(),
    };

    let stacktraces = vec![JvmStacktrace {
        frames: vec![
            JvmFrame {
                function: "otherMethod".into(),
                abs_path: Some("OtherActivity.java".into()),
                module: "OtherActivity".into(),
                filename: Some("OtherActivity.java".into()),
                lineno: 100,
                index: 0,
                ..Default::default()
            },
            JvmFrame {
                function: "differentMethod".into(),
                abs_path: Some("DifferentActivity".into()),
                module: "DifferentActivity".into(),
                filename: Some("DifferentActivity".into()),
                lineno: 200,
                index: 1,
                ..Default::default()
            },
            JvmFrame {
                function: "onCreate".into(),
                abs_path: None,
                module: "io.sentry.samples.MainActivity".into(),
                filename: None,
                lineno: 11,
                index: 2,
                ..Default::default()
            },
            JvmFrame {
                function: "whoops".into(),
                abs_path: Some("MainActivity.kt".into()),
                module: "io.sentry.samples.MainActivity$InnerClass".into(),
                filename: Some("MainActivity.kt".into()),
                lineno: 20,
                index: 3,
                ..Default::default()
            },
            JvmFrame {
                function: "whoops2".into(),
                abs_path: None,
                module: "io.sentry.samples.MainActivity$AnotherInnerClass".into(),
                filename: None,
                lineno: 26,
                index: 4,
                ..Default::default()
            },
            JvmFrame {
                function: "whoops3".into(),
                abs_path: Some("MainActivity.kt".into()),
                module: "io.sentry.samples.MainActivity$AdditionalInnerClass".into(),
                filename: Some("MainActivity.kt".into()),
                lineno: 32,
                index: 5,
                ..Default::default()
            },
            JvmFrame {
                function: "whoops4".into(),
                abs_path: Some("SourceFile".into()),
                module: "io.sentry.samples.MainActivity$OneMoreInnerClass".into(),
                filename: Some("SourceFile".into()),
                lineno: 38,
                index: 6,
                ..Default::default()
            },
        ],
    }];

    let request = SymbolicateJvmStacktraces {
        scope: Scope::Global,
        sources: Arc::new([source]),
        exceptions: vec![exception.clone()],
        stacktraces,
        modules: vec![JvmModule {
            uuid: debug_id,
            r#type: JvmModuleType::Source,
        }],
        apply_source_context: true,
        release_package: None,
    };

    let response = symbolication.symbolicate_jvm(request).await;

    assert_snapshot!(response);
}

#[tokio::test]
async fn test_source_lookup_with_proguard() {
    symbolicator_test::setup();
    let (symbolication, _cache_dir) = setup_service(|_| ());

    let proguard_id = "05d96b1c-1786-477c-8615-d3cf83e027c7".parse().unwrap();
    let missing_proguard_id = "8236f5cf-52c8-4e35-a7cf-01421e4c2c88".parse().unwrap();
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

    let exception = JvmException {
        ty: "RuntimeException".into(),
        module: "java.lang".into(),
    };

    let stacktraces = vec![JvmStacktrace {
        frames: vec![
            JvmFrame {
                filename: Some("ZygoteInit.java".into()),
                function: "main".into(),
                module: "com.android.internal.os.ZygoteInit".into(),
                lineno: 698,
                index: 0,
                ..Default::default()
            },
            JvmFrame {
                filename: Some("ZygoteInit.java".into()),
                function: "run".into(),
                module: "com.android.internal.os.ZygoteInit$MethodAndArgsCaller".into(),
                lineno: 903,
                index: 1,
                ..Default::default()
            },
            JvmFrame {
                filename: Some("Method.java".into()),
                function: "invoke".into(),
                module: "java.lang.reflect.Method".into(),
                lineno: 372,
                index: 2,
                ..Default::default()
            },
            JvmFrame {
                filename: Some("Method.java".into()),
                function: "invoke".into(),
                module: "java.lang.reflect.Method".into(),
                index: 3,
                ..Default::default()
            },
            JvmFrame {
                filename: Some("ActivityThread.java".into()),
                function: "main".into(),
                module: "android.app.ActivityThread".into(),
                lineno: 5254,
                index: 4,
                ..Default::default()
            },
            JvmFrame {
                filename: Some("Looper.java".into()),
                function: "loop".into(),
                module: "android.os.Looper".into(),
                lineno: 135,
                index: 5,
                ..Default::default()
            },
            JvmFrame {
                filename: Some("Handler.java".into()),
                function: "dispatchMessage".into(),
                module: "android.os.Handler".into(),
                lineno: 95,
                index: 6,
                ..Default::default()
            },
            JvmFrame {
                filename: Some("Handler.java".into()),
                function: "handleCallback".into(),
                module: "android.os.Handler".into(),
                lineno: 739,
                index: 7,
                ..Default::default()
            },
            JvmFrame {
                filename: Some("View.java".into()),
                function: "run".into(),
                module: "android.view.View$PerformClick".into(),
                lineno: 19866,
                index: 8,
                ..Default::default()
            },
            JvmFrame {
                filename: Some("View.java".into()),
                function: "performClick".into(),
                module: "android.view.View".into(),
                lineno: 4780,
                index: 9,
                ..Default::default()
            },
            JvmFrame {
                filename: Some("ActionMenuItemView.java".into()),
                function: "onClick".into(),
                module: "androidx.appcompat.view.menu.ActionMenuItemView".into(),
                lineno: 7,
                index: 10,
                ..Default::default()
            },
            JvmFrame {
                filename: Some("ActionMenuView.java".into()),
                function: "invokeItem".into(),
                module: "androidx.appcompat.widget.ActionMenuView".into(),
                lineno: 4,
                index: 11,
                ..Default::default()
            },
            JvmFrame {
                filename: Some("MenuBuilder.java".into()),
                function: "performItemAction".into(),
                module: "androidx.appcompat.view.menu.MenuBuilder".into(),
                lineno: 1,
                index: 12,
                ..Default::default()
            },
            JvmFrame {
                filename: Some("MenuBuilder.java".into()),
                function: "performItemAction".into(),
                module: "androidx.appcompat.view.menu.MenuBuilder".into(),
                lineno: 4,
                index: 13,
                ..Default::default()
            },
            JvmFrame {
                filename: Some("MenuItemImpl.java".into()),
                function: "invoke".into(),
                module: "androidx.appcompat.view.menu.MenuItemImpl".into(),
                lineno: 15,
                index: 14,
                ..Default::default()
            },
            JvmFrame {
                filename: Some("MenuBuilder.java".into()),
                function: "dispatchMenuItemSelected".into(),
                module: "androidx.appcompat.view.menu.MenuBuilder".into(),
                lineno: 5,
                index: 15,
                ..Default::default()
            },
            JvmFrame {
                filename: Some("ActionMenuView.java".into()),
                function: "onMenuItemSelected".into(),
                module: "androidx.appcompat.widget.ActionMenuView$MenuBuilderCallback".into(),
                lineno: 7,
                index: 16,
                ..Default::default()
            },
            JvmFrame {
                filename: Some("Toolbar.java".into()),
                function: "onMenuItemClick".into(),
                module: "androidx.appcompat.widget.Toolbar$1".into(),
                lineno: 7,
                index: 17,
                ..Default::default()
            },
            JvmFrame {
                filename: Some("R8$$SyntheticClass".into()),
                function: "onMenuItemClick".into(),
                module: "io.sentry.samples.instrumentation.ui.g".into(),
                lineno: 40,
                in_app: Some(true),
                index: 18,
                ..Default::default()
            },
        ],
    }];

    let modules = vec![
        JvmModule {
            r#type: JvmModuleType::Source,
            uuid: source_id1,
        },
        JvmModule {
            r#type: JvmModuleType::Source,
            uuid: source_id2,
        },
        JvmModule {
            r#type: JvmModuleType::Proguard,
            uuid: proguard_id,
        },
        JvmModule {
            r#type: JvmModuleType::Proguard,
            uuid: missing_proguard_id,
        },
    ];

    let request = SymbolicateJvmStacktraces {
        scope: Scope::Global,
        sources: Arc::new([source]),
        exceptions: vec![exception.clone()],
        stacktraces,
        modules,
        apply_source_context: true,
        release_package: None,
    };

    let response = symbolication.symbolicate_jvm(request).await;

    assert_snapshot!(response);
}
