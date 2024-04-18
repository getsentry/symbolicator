use crate::interface::{
    CompletedJvmSymbolicationResponse, JvmException, JvmFrame, JvmModuleType, JvmStacktrace,
    ProguardError, ProguardErrorKind, SymbolicateJvmStacktraces,
};
use crate::ProguardService;

use futures::future;
use symbolic::debuginfo::sourcebundle::SourceBundleDebugSession;
use symbolic::debuginfo::ObjectDebugSession;
use symbolicator_service::caching::CacheError;
use symbolicator_service::source_context::get_context_lines;

impl ProguardService {
    /// Symbolicates a JVM event.
    ///
    /// "Symbolicate" here means that exceptions and stack
    /// frames are remapped using proguard files.
    #[tracing::instrument(skip_all,
        fields(
            exceptions = request.exceptions.len(),
            frames = request.stacktraces.iter().map(|st| st.frames.len()).sum::<usize>())
        )
    ]
    pub async fn symbolicate_jvm(
        &self,
        request: SymbolicateJvmStacktraces,
    ) -> CompletedJvmSymbolicationResponse {
        let SymbolicateJvmStacktraces {
            scope,
            sources,
            exceptions,
            stacktraces,
            modules,
            release_package,
            apply_source_context,
        } = request;

        let maybe_mappers = future::join_all(
            modules
                .iter()
                .filter(|module| module.r#type == JvmModuleType::Proguard)
                .map(|module| async {
                    let file = self
                        .download_proguard_file(&sources, &scope, module.uuid)
                        .await;
                    (module.uuid, file)
                }),
        )
        .await;

        let (mut mappers, mut errors) = (Vec::new(), Vec::new());
        for (debug_id, res) in &maybe_mappers {
            match res {
                Ok(mapper) => mappers.push(mapper.get()),
                Err(e) => {
                    tracing::error!(%debug_id, error = %e, "Error reading Proguard file");
                    let kind = match e {
                        CacheError::Malformed(msg) => match msg.as_str() {
                            "The file is not a valid ProGuard file" => ProguardErrorKind::Invalid,
                            "The ProGuard file doesn't contain any line mappings" => {
                                ProguardErrorKind::NoLineInfo
                            }
                            _ => unreachable!(),
                        },
                        _ => ProguardErrorKind::Missing,
                    };
                    errors.push(ProguardError {
                        uuid: *debug_id,
                        kind,
                    });
                }
            }
        }

        let maybe_source_bundles = future::join_all(
            modules
                .iter()
                .filter(|module| module.r#type == JvmModuleType::Source)
                .map(|module| async {
                    let file = self
                        .download_source_bundle(sources.clone(), &scope, module.uuid)
                        .await;
                    (module.uuid, file)
                }),
        )
        .await;

        let source_bundles: Vec<_> = maybe_source_bundles
            .into_iter()
            .filter_map(|(debug_id, maybe_bundle)| match maybe_bundle {
                Ok(b) => Some((debug_id, b)),
                Err(e) => {
                    tracing::debug!(%debug_id, error=%e, "Failed to download source bundle");
                    None
                }
            })
            .collect();

        let source_bundle_sessions: Vec<_> = source_bundles
            .iter()
            .filter_map(|(debug_id, bundle)| match bundle.object().debug_session() {
                Ok(ObjectDebugSession::SourceBundle(session)) => Some(session),
                _ => {
                    tracing::debug!(%debug_id, "Failed to open debug session for source bundle");
                    None
                }
            })
            .collect();

        let remapped_exceptions = exceptions
            .into_iter()
            .map(|raw_exception| {
                Self::map_exception(&mappers, &raw_exception).unwrap_or(raw_exception)
            })
            .collect();

        let mut remapped_stacktraces: Vec<_> = stacktraces
            .into_iter()
            .map(|raw_stacktrace| {
                let remapped_frames = raw_stacktrace
                    .frames
                    .iter()
                    .flat_map(|frame| {
                        Self::map_frame(&mappers, frame, release_package.as_deref()).into_iter()
                    })
                    .collect();
                JvmStacktrace {
                    frames: remapped_frames,
                }
            })
            .collect();

        if apply_source_context {
            for frame in remapped_stacktraces
                .iter_mut()
                .flat_map(|st| st.frames.iter_mut())
            {
                Self::apply_source_context(&source_bundle_sessions, frame);
            }
        }

        CompletedJvmSymbolicationResponse {
            exceptions: remapped_exceptions,
            stacktraces: remapped_stacktraces,
            errors,
        }
    }

    /// Remaps an exception using the provided mappers.
    ///
    /// This returns a new exception with the deobfuscated module and class names.
    /// Returns `None` if none of the `mappers` can remap the exception.
    fn map_exception(
        mappers: &[&proguard::ProguardMapper],
        exception: &JvmException,
    ) -> Option<JvmException> {
        if mappers.is_empty() {
            return None;
        }

        let key = format!("{}.{}", exception.module, exception.ty);

        let mapped = mappers.iter().find_map(|mapper| mapper.remap_class(&key))?;

        // In the Python implementation, we just split by `.` here with no check. I assume
        // this error can not actually occur.
        let Some((new_module, new_ty)) = mapped.rsplit_once('.') else {
            tracing::error!(
                original = key,
                remapped = mapped,
                "Invalid remapped class name"
            );
            return None;
        };

        Some(JvmException {
            ty: new_ty.into(),
            module: new_module.into(),
        })
    }

    /// Remaps a frame using the provided mappers.
    ///
    /// This returns a list of frames because remapping may
    /// expand a frame into several. The returned list is always
    /// nonempty; if none of the mappers can remap the frame, the original
    /// frame is returned.
    fn map_frame(
        mappers: &[&proguard::ProguardMapper],
        frame: &JvmFrame,
        release_package: Option<&str>,
    ) -> Vec<JvmFrame> {
        let deobfuscated_signature: Option<proguard::DeobfuscatedSignature> =
            frame.signature.as_ref().and_then(|signature| {
                for mapper in mappers {
                    if let Some(deobfuscated_signature) = mapper.deobfuscate_signature(signature) {
                        return Some(deobfuscated_signature);
                    }
                }
                None
            });

        let params = deobfuscated_signature
            .as_ref()
            .map(|sig| sig.parameters_types().collect::<Vec<_>>().join(","));

        // We create the proguard frame according to these priorities:
        // * Use the frame's line number if it exists
        // * Use the frame's parameters if they exist
        // * Use line number 0
        let proguard_frame = frame
            .lineno
            .map(|lineno| {
                proguard::StackFrame::new(&frame.module, &frame.function, lineno as usize)
            })
            .or_else(|| {
                params.as_ref().map(|p| {
                    proguard::StackFrame::with_parameters(
                        &frame.module,
                        &frame.function,
                        p.as_str(),
                    )
                })
            })
            // This is for parity with the Python implementation. It's unclear why remapping a frame with line 0
            // would produce useful information, and I have no conclusive evidence that it does.
            // See the `line_0_1` and `line_0_2` unit tests in this file for examples of the results this produces.
            //
            // TODO(@loewenheim): Find out if this is useful and remove it otherwise.
            // The PR that introduced this was https://github.com/getsentry/symbolicator/pull/1434.
            .unwrap_or_else(|| proguard::StackFrame::new(&frame.module, &frame.function, 0));

        // First, try to remap the whole frame.
        let mut mapped_frames = Vec::new();
        let mut frames = mappers
            .iter()
            .find_map(|mapper| {
                Self::map_full_frame(mapper, frame, &proguard_frame, &mut mapped_frames)
            })
            // Second, try to remap the frame's method.
            .or_else(|| {
                mappers
                    .iter()
                    .find_map(|mapper| Self::map_class_method(mapper, frame))
            })
            // Third, try to remap just the frame's class.
            .or_else(|| {
                mappers
                    .iter()
                    .find_map(|mapper| Self::map_class(mapper, frame))
            });

        // Fix up the frames' in-app fields only if they were actually mapped
        if let Some(frames) = frames.as_mut() {
            for frame in frames {
                // mark the frame as in_app after deobfuscation based on the release package name
                // only if it's not present
                if let Some(package) = release_package {
                    if frame.module.starts_with(package) && frame.in_app.is_none() {
                        frame.in_app = Some(true);
                    }
                }
            }
        }

        // If all else fails, just return the original frame.
        let mut frames = frames.unwrap_or_else(|| vec![frame.clone()]);

        for frame in &mut frames {
            // add the signature if we received one and we were
            // able to translate/deobfuscate it
            if let Some(signature) = &deobfuscated_signature {
                frame.signature = Some(signature.format_signature());
            }
        }

        frames
    }

    /// Tries to remap a `JvmFrame` using a `proguard::StackFrame`
    /// constructed from it.
    ///
    /// The `buf` parameter is used as a buffer for the frames returned
    /// by `remap_frame`.
    fn map_full_frame<'a>(
        mapper: &'a proguard::ProguardMapper<'a>,
        original_frame: &JvmFrame,
        proguard_frame: &proguard::StackFrame<'a>,
        buf: &mut Vec<proguard::StackFrame<'a>>,
    ) -> Option<Vec<JvmFrame>> {
        buf.clear();
        buf.extend(mapper.remap_frame(proguard_frame));

        if buf.is_empty() {
            return None;
        }

        let bottom_class = buf[buf.len() - 1].class();

        // sentry expects stack traces in reverse order
        let res = buf
            .iter()
            .rev()
            .map(|new_frame| {
                let mut mapped_frame = JvmFrame {
                    module: new_frame.class().to_owned(),
                    function: new_frame.method().to_owned(),
                    lineno: Some(new_frame.line() as u32),
                    ..original_frame.clone()
                };

                // clear the filename for all *foreign* classes
                if mapped_frame.module != bottom_class {
                    mapped_frame.filename = None;
                    mapped_frame.abs_path = None;
                }

                mapped_frame
            })
            .collect();
        Some(res)
    }

    /// Tries to remap a frame's class and method.
    fn map_class_method(
        mapper: &proguard::ProguardMapper,
        frame: &JvmFrame,
    ) -> Option<Vec<JvmFrame>> {
        let (mapped_class, mapped_method) = mapper.remap_method(&frame.module, &frame.function)?;

        Some(vec![JvmFrame {
            module: mapped_class.to_owned(),
            function: mapped_method.to_owned(),
            ..frame.clone()
        }])
    }

    /// Tries to remap a frame's class.
    fn map_class(mapper: &proguard::ProguardMapper, frame: &JvmFrame) -> Option<Vec<JvmFrame>> {
        let mapped_class = mapper.remap_class(&frame.module)?;

        Some(vec![JvmFrame {
            module: mapped_class.to_owned(),
            ..frame.clone()
        }])
    }

    /// Applies source context from the given list of source bundles to a frame.
    ///
    /// If one of the source bundles contains the correct file name, we apply it, otherwise
    /// the frame stays unmodified.
    fn apply_source_context(source_bundles: &[SourceBundleDebugSession<'_>], frame: &mut JvmFrame) {
        let lineno = match frame.lineno {
            // can't apply source context without line number
            None | Some(0) => return,
            Some(n) => n,
        };

        let source_file_name = build_source_file_name(frame);
        for session in source_bundles {
            let Ok(Some(source)) = session.source_by_url(&source_file_name) else {
                continue;
            };

            let Some(contents) = source.contents() else {
                continue;
            };

            if let Some((pre_context, context_line, post_context)) =
                get_context_lines(contents, lineno as usize, 0, None)
            {
                frame.pre_context = pre_context;
                frame.context_line = Some(context_line);
                frame.post_context = post_context;
                break;
            }
        }
    }
}

/// Checks whether `abs_path` is a valid path, and if so, returns the part
/// of `abs_path` before the rightmost `.`.
///
/// An `abs_path` is valid if it contains a `.` and doesn't contain a `$`.
fn is_valid_path(abs_path: &str) -> Option<&str> {
    if abs_path.contains('$') {
        return None;
    }
    let (before, _) = abs_path.rsplit_once('.')?;
    Some(before)
}

/// Constructs a source file name out of a frame's `abs_path` and `module`.
fn build_source_file_name(frame: &JvmFrame) -> String {
    let abs_path = frame.abs_path.as_deref();
    let module = &frame.module;
    let mut source_file_name = String::from("~/");

    match abs_path.and_then(is_valid_path) {
        Some(abs_path_before_dot) => {
            if let Some((module_before_dot, _)) = module.rsplit_once('.') {
                source_file_name.push_str(&module_before_dot.replace('.', "/"));
                source_file_name.push('/');
            }
            source_file_name.push_str(abs_path_before_dot);
        }
        None => {
            let module_before_dollar = module.split_once('$').map(|p| p.0).unwrap_or(module);
            source_file_name.push_str(&module_before_dollar.replace('.', "/"));
        }
    };

    // fake extension because we don't know whether it's .java, .kt or something else
    source_file_name.push_str(".jvm");
    source_file_name
}

#[cfg(test)]
mod tests {
    use super::*;
    use proguard::{ProguardMapper, ProguardMapping};

    #[test]
    fn remap_exception_simple() {
        let proguard_source = b"org.slf4j.helpers.Util$ClassContextSecurityManager -> org.a.b.g$a:
    65:65:void <init>() -> <init>
    67:67:java.lang.Class[] getClassContext() -> a
    69:69:java.lang.Class[] getExtraClassContext() -> a
    68:68:java.lang.Class[] getContext() -> a
    65:65:void <init>(org.slf4j.helpers.Util$1) -> <init>
org.slf4j.helpers.Util$ClassContext -> org.a.b.g$b:
    65:65:void <init>() -> <init>
";

        let exception = JvmException {
            ty: "g$a".into(),
            module: "org.a.b".into(),
        };

        let mapping = ProguardMapping::new(proguard_source);
        let mapper = ProguardMapper::new(mapping);

        let exception = ProguardService::map_exception(&[&mapper], &exception).unwrap();

        assert_eq!(exception.ty, "Util$ClassContextSecurityManager");
        assert_eq!(exception.module, "org.slf4j.helpers");
    }

    // based on the Python test `test_resolving_inline`
    #[test]
    fn remap_frames_simple() {
        let proguard_source = b"# compiler: R8
# compiler_version: 2.0.74
# min_api: 16
# pg_map_id: 5b46fdc
# common_typos_disable
$r8$backportedMethods$utility$Objects$2$equals -> a:
    boolean equals(java.lang.Object,java.lang.Object) -> a
$r8$twr$utility -> b:
    void $closeResource(java.lang.Throwable,java.lang.Object) -> a
android.support.v4.app.RemoteActionCompatParcelizer -> android.support.v4.app.RemoteActionCompatParcelizer:
    1:1:void <init>():11:11 -> <init>
io.sentry.sample.-$$Lambda$r3Avcbztes2hicEObh02jjhQqd4 -> e.a.c.a:
    io.sentry.sample.MainActivity f$0 -> b
io.sentry.sample.MainActivity -> io.sentry.sample.MainActivity:
    1:1:void <init>():15:15 -> <init>
    1:1:boolean onCreateOptionsMenu(android.view.Menu):60:60 -> onCreateOptionsMenu
    1:1:boolean onOptionsItemSelected(android.view.MenuItem):69:69 -> onOptionsItemSelected
    2:2:boolean onOptionsItemSelected(android.view.MenuItem):76:76 -> onOptionsItemSelected
    1:1:void bar():54:54 -> t
    1:1:void foo():44 -> t
    1:1:void onClickHandler(android.view.View):40 -> t";

        let frames = [
            JvmFrame {
                function: "onClick".to_owned(),
                module: "e.a.c.a".to_owned(),
                lineno: Some(2),
                index: 0,
                ..Default::default()
            },
            JvmFrame {
                function: "t".to_owned(),
                module: "io.sentry.sample.MainActivity".to_owned(),
                filename: Some("MainActivity.java".to_owned()),
                lineno: Some(1),
                index: 1,
                ..Default::default()
            },
            JvmFrame {
                function: "t".to_owned(),
                module: "io.sentry.sample.MainActivity".to_owned(),
                signature: Some("(Landroid/view/View;)V".to_owned()),
                index: 2,
                ..Default::default()
            },
            JvmFrame {
                // this function map is onClickHandler(android.view.View):40 -> t
                // not onClickHandler(android.view.View):40 -> onClickHandler
                // hence the class remapping should fail,
                // but the signature should still be properly translated to java type
                function: "onClickHandler".to_owned(),
                module: "io.sentry.sample.MainActivity".to_owned(),
                signature: Some("(Landroid/view/View;)V".to_owned()),
                index: 3,
                ..Default::default()
            },
            JvmFrame {
                // this module (Class) does not exist in the mapping,
                // hence the whole frame remapping should fail,
                // but the signature should still be properly translated to java type
                function: "onClickHandler".to_owned(),
                module: "io.sentry.sample.ClassDoesNotExist".to_owned(),
                signature: Some("(Landroid/view/View;)V".to_owned()),
                index: 4,
                ..Default::default()
            },
        ];

        let mapping = ProguardMapping::new(proguard_source);
        let mapper = ProguardMapper::new_with_param_mapping(mapping, true);

        let mapped_frames: Vec<_> = frames
            .iter()
            .flat_map(|frame| ProguardService::map_frame(&[&mapper], frame, None).into_iter())
            .collect();

        assert_eq!(mapped_frames.len(), 7);

        assert_eq!(mapped_frames[0].function, "onClick");
        assert_eq!(
            mapped_frames[0].module,
            "io.sentry.sample.-$$Lambda$r3Avcbztes2hicEObh02jjhQqd4"
        );
        assert_eq!(mapped_frames[0].index, 0);

        assert_eq!(
            mapped_frames[1].filename,
            Some("MainActivity.java".to_owned())
        );
        assert_eq!(mapped_frames[1].module, "io.sentry.sample.MainActivity");
        assert_eq!(mapped_frames[1].function, "onClickHandler");
        assert_eq!(mapped_frames[1].lineno, Some(40));
        assert_eq!(mapped_frames[1].index, 1);

        assert_eq!(mapped_frames[2].function, "foo");
        assert_eq!(mapped_frames[2].lineno, Some(44));
        assert_eq!(mapped_frames[2].index, 1);

        assert_eq!(mapped_frames[3].function, "bar");
        assert_eq!(mapped_frames[3].lineno, Some(54));
        assert_eq!(
            mapped_frames[3].filename,
            Some("MainActivity.java".to_owned())
        );
        assert_eq!(mapped_frames[3].module, "io.sentry.sample.MainActivity");
        assert_eq!(mapped_frames[3].index, 1);

        assert_eq!(mapped_frames[4].function, "onClickHandler");
        assert_eq!(
            mapped_frames[4].signature.as_ref().unwrap(),
            "(android.view.View)"
        );

        assert_eq!(mapped_frames[5].function, "onClickHandler");
        assert_eq!(
            mapped_frames[5].signature.as_ref().unwrap(),
            "(android.view.View)"
        );

        assert_eq!(mapped_frames[6].function, "onClickHandler");
        assert_eq!(
            mapped_frames[6].signature.as_ref().unwrap(),
            "(android.view.View)"
        );
    }

    // based on the Python test `test_sets_inapp_after_resolving`.
    #[test]
    fn sets_in_app_after_resolving() {
        let proguard_source = b"org.slf4j.helpers.Util$ClassContextSecurityManager -> org.a.b.g$a:
    65:65:void <init>() -> <init>
    67:67:java.lang.Class[] getClassContext() -> a
    69:69:java.lang.Class[] getExtraClassContext() -> a
    68:68:java.lang.Class[] getContext() -> a
    65:65:void <init>(org.slf4j.helpers.Util$1) -> <init>
org.slf4j.helpers.Util$ClassContext -> org.a.b.g$b:
    65:65:void <init>() -> <init>
";

        let mapping = ProguardMapping::new(proguard_source);
        let mapper = ProguardMapper::new(mapping);

        let frames = [
            JvmFrame {
                function: "a".to_owned(),
                module: "org.a.b.g$a".to_owned(),
                lineno: Some(67),
                ..Default::default()
            },
            JvmFrame {
                function: "a".to_owned(),
                module: "org.a.b.g$a".to_owned(),
                lineno: Some(69),
                in_app: Some(false),
                ..Default::default()
            },
            JvmFrame {
                function: "a".to_owned(),
                module: "org.a.b.g$a".to_owned(),
                lineno: Some(68),
                in_app: Some(true),
                ..Default::default()
            },
            JvmFrame {
                function: "init".to_owned(),
                module: "com.android.Zygote".to_owned(),
                lineno: Some(62),
                ..Default::default()
            },
            JvmFrame {
                function: "a".to_owned(),
                module: "org.a.b.g$b".to_owned(),
                lineno: Some(70),
                ..Default::default()
            },
        ];

        let mapped_frames: Vec<_> = frames
            .iter()
            .flat_map(|frame| {
                ProguardService::map_frame(&[&mapper], frame, Some("org.slf4j")).into_iter()
            })
            .collect();

        assert_eq!(mapped_frames[0].in_app, Some(true));
        assert_eq!(mapped_frames[1].in_app, Some(false));
        assert_eq!(mapped_frames[2].in_app, Some(true));

        // According to the Python test, this should be `Some(false)`, but
        // based just on the code in this file, this is not possible. We never set `in_app` to `false`,
        // this must happen somewhere else in `sentry`.
        // assert_eq!(mapped_frames[3].in_app, Some(false));
        assert_eq!(mapped_frames[4].in_app, Some(true));
    }

    #[test]
    fn doesnt_set_in_app_if_not_resolved() {
        let frame = JvmFrame {
            function: "main".into(),
            module: "android.app.ActivityThread".into(),
            lineno: Some(8918),
            ..Default::default()
        };

        let remapped = ProguardService::map_frame(&[], &frame, Some("android"));

        assert_eq!(remapped.len(), 1);
        // The frame didn't get mapped, so we shouldn't set `in_app` even though
        // the condition is satisfied.
        assert!(remapped[0].in_app.is_none());
    }

    #[test]
    fn line_0_1() {
        let proguard_source = br#"com.example.App -> com.example.App:
# {"id":"sourceFile","fileName":"App.java"}
    boolean injected -> g
    foo.bar.android.internal.managers.ApplicationComponentManager componentManager -> h
    0:3:void <init>():18:18 -> <init>
    4:5:void <init>():19:19 -> <init>
    6:18:void <init>():21:21 -> <init>
    1:1:foo.bar.internal.GeneratedComponentManager componentManager():17:17 -> componentManager
    2:2:foo.bar.android.internal.managers.ApplicationComponentManager componentManager():31:31 -> componentManager
    0:6:java.lang.Object generatedComponent():36:36 -> generatedComponent
    0:4:void barInternalInject():47:47 -> onCreate
    0:4:void onCreate():42 -> onCreate
    5:6:void barInternalInject():48:48 -> onCreate
    5:6:void onCreate():42 -> onCreate
    7:12:java.lang.Object generatedComponent():36:36 -> onCreate
    7:12:void barInternalInject():51 -> onCreate
    7:12:void onCreate():42 -> onCreate
    13:20:void barInternalInject():51:51 -> onCreate
    13:20:void onCreate():42 -> onCreate
    21:24:void onCreate():43:43 -> onCreate
"#;

        let mapping = ProguardMapping::new(proguard_source);
        let mapper = ProguardMapper::new(mapping);

        let frame = JvmFrame {
            function: "onCreate".into(),
            module: "com.example.App".into(),
            index: 0,
            ..Default::default()
        };

        let mapped_frames = ProguardService::map_frame(&[&mapper], &frame, None);

        assert_eq!(mapped_frames.len(), 2);

        assert_eq!(
            mapped_frames[0],
            JvmFrame {
                function: "onCreate".into(),
                module: "com.example.App".into(),
                lineno: Some(0),
                index: 0,
                ..Default::default()
            }
        );

        // Without the "line 0" change, this frame doesn't exist.
        assert_eq!(
            mapped_frames[1],
            JvmFrame {
                function: "barInternalInject".into(),
                module: "com.example.App".into(),
                lineno: Some(0),
                index: 0,
                ..Default::default()
            }
        );
    }

    #[test]
    fn line_0_2() {
        let proguard_source = br#"com.google.firebase.concurrent.CustomThreadFactory$$ExternalSyntheticLambda0 -> com.google.firebase.concurrent.a:
# {"id":"sourceFile","fileName":"R8$$SyntheticClass"}
# {"id":"com.android.tools.r8.synthesized"}
    int com.google.firebase.concurrent.CustomThreadFactory$$InternalSyntheticLambda$1$53203795c28a6fcdb3bac755806c9ee73cb3e8dcd4c9bbf8ca5d25d4d9c378dd$0.$r8$classId -> d
    com.google.firebase.concurrent.CustomThreadFactory com.google.firebase.concurrent.CustomThreadFactory$$InternalSyntheticLambda$1$53203795c28a6fcdb3bac755806c9ee73cb3e8dcd4c9bbf8ca5d25d4d9c378dd$0.f$0 -> e
    java.lang.Runnable com.google.firebase.concurrent.CustomThreadFactory$$InternalSyntheticLambda$1$53203795c28a6fcdb3bac755806c9ee73cb3e8dcd4c9bbf8ca5d25d4d9c378dd$0.f$1 -> f
    0:9:void com.google.firebase.concurrent.CustomThreadFactory$$InternalSyntheticLambda$1$53203795c28a6fcdb3bac755806c9ee73cb3e8dcd4c9bbf8ca5d25d4d9c378dd$0.<init>(com.google.firebase.concurrent.CustomThreadFactory,java.lang.Runnable):0:0 -> <init>
    0:9:void com.google.firebase.concurrent.CustomThreadFactory$$InternalSyntheticLambda$1$53203795c28a6fcdb3bac755806c9ee73cb3e8dcd4c9bbf8ca5d25d4d9c378dd$0.$r8$init$synthetic(java.lang.Object,java.lang.Object,int):0 -> <init>
      # {"id":"com.android.tools.r8.synthesized"}
      # {"id":"com.android.tools.r8.residualsignature","signature":"(ILjava/lang/Object;Ljava/lang/Object;)V"}
    0:25:void com.google.firebase.concurrent.CustomThreadFactory$$InternalSyntheticLambda$1$53203795c28a6fcdb3bac755806c9ee73cb3e8dcd4c9bbf8ca5d25d4d9c378dd$0.run$bridge():0:0 -> run
      # {"id":"com.android.tools.r8.synthesized"}
y.b -> y.b:
# {"id":"sourceFile","fileName":"FutureExt.kt"}
    0:4:void a(com.google.common.util.concurrent.ListenableFuture,com.drivit.core.DrivitCloud$OperationListener):1:1 -> a
    5:8:void a(com.google.common.util.concurrent.ListenableFuture,com.drivit.core.DrivitCloud$OperationListener):2:2 -> a
"#;

        let mapping = ProguardMapping::new(proguard_source);
        let mapper = ProguardMapper::new(mapping);

        let frame = JvmFrame {
            function: "run".into(),
            module: "com.google.firebase.concurrent.a".into(),
            index: 0,
            ..Default::default()
        };

        let mapped_frames = ProguardService::map_frame(&[&mapper], &frame, None);

        assert_eq!(mapped_frames.len(), 1);

        assert_eq!(
            mapped_frames[0],
            JvmFrame {
                function: "run$bridge".into(),
                // Without the "line 0" change, this is "com.google.firebase.concurrent.CustomThreadFactory$$ExternalSyntheticLambda0".
                module: "com.google.firebase.concurrent.CustomThreadFactory$$InternalSyntheticLambda$1$53203795c28a6fcdb3bac755806c9ee73cb3e8dcd4c9bbf8ca5d25d4d9c378dd$0".into(),
                lineno: Some(0),
                index: 0,
                ..Default::default()
            }
        );
    }

    #[test]
    fn test_build_source_file_name() {
        let frame = JvmFrame {
            function: "run".into(),
            module: "com.foo.bar.ui.activities.base.BaseActivity$$ExternalSyntheticLambda2".into(),
            ..Default::default()
        };

        assert_eq!(
            build_source_file_name(&frame),
            "~/com/foo/bar/ui/activities/base/BaseActivity.jvm"
        );
    }
}
