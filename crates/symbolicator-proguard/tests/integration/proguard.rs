use std::sync::Arc;
use std::{collections::HashMap, str::FromStr};

use serde_json::json;
use symbolic::common::DebugId;
use symbolicator_proguard::interface::{
    CompletedJvmSymbolicationResponse, JvmException, JvmFrame, JvmModule, JvmStacktrace,
    SymbolicateJvmStacktraces,
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
        modules: vec![JvmModule { uuid: debug_id }],
        apply_source_context: false,
        release_package: None,
    };

    let CompletedJvmSymbolicationResponse { exceptions, .. } =
        symbolication.symbolicate_jvm(request).await;

    let remapped_exception = JvmException {
        ty: "Util$ClassContextSecurityManager".into(),
        module: "org.slf4j.helpers".into(),
    };

    assert_eq!(exceptions, [remapped_exception]);
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
                ..Default::default()
            },
            JvmFrame {
                function: "t".into(),
                module: "io.sentry.sample.MainActivity".into(),
                filename: Some("MainActivity.java".into()),
                lineno: 1,
                ..Default::default()
            },
        ],
    }];

    let request = SymbolicateJvmStacktraces {
        scope: Scope::Global,
        sources: Arc::new([source]),
        exceptions: vec![exception.clone()],
        stacktraces,
        modules: vec![JvmModule { uuid: debug_id }],
        apply_source_context: false,
        release_package: None,
    };

    let CompletedJvmSymbolicationResponse { stacktraces, .. } =
        symbolication.symbolicate_jvm(request).await;

    let frames = &stacktraces[0].frames;

    assert_eq!(frames.len(), 4);
    assert_eq!(frames[0].function, "onClick");
    assert_eq!(
        frames[0].module,
        "io.sentry.sample.-$$Lambda$r3Avcbztes2hicEObh02jjhQqd4"
    );
    assert_eq!(frames[1].filename, Some("MainActivity.java".into()));
    assert_eq!(frames[1].module, "io.sentry.sample.MainActivity");
    assert_eq!(frames[1].function, "onClickHandler");
    assert_eq!(frames[1].lineno, 40);
    assert_eq!(frames[2].function, "foo");
    assert_eq!(frames[2].lineno, 44);
    assert_eq!(frames[3].function, "bar");
    assert_eq!(frames[3].lineno, 54);
    assert_eq!(frames[3].filename, Some("MainActivity.java".into()));
    assert_eq!(frames[3].module, "io.sentry.sample.MainActivity");
}
