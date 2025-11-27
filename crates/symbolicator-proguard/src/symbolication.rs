use std::sync::Arc;

use crate::ProguardService;
use crate::interface::{
    CompletedJvmSymbolicationResponse, JvmException, JvmFrame, JvmModuleType, JvmStacktrace,
    ProguardError, ProguardErrorKind, SymbolicateJvmStacktraces,
};
use crate::metrics::{SymbolicationStats, record_symbolication_metrics};

use futures::future;
use symbolic::debuginfo::ObjectDebugSession;
use symbolic::debuginfo::sourcebundle::SourceBundleDebugSession;
use symbolicator_service::caching::CacheError;
use symbolicator_service::source_context::get_context_lines;
use symbolicator_service::types::FrameOrder;

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
            platform,
            scope,
            sources,
            exceptions,
            mut stacktraces,
            modules,
            release_package,
            apply_source_context,
            classes,
            frame_order,
        } = request;

        let mut stats = SymbolicationStats::default();

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
                    if !matches!(e, CacheError::NotFound) {
                        tracing::error!(%debug_id, "Error reading Proguard file: {e}");
                    }
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

        let remapped_exceptions: Vec<_> = exceptions
            .into_iter()
            .map(
                |raw_exception| match Self::map_exception(&mappers, &raw_exception) {
                    Some(exc) => {
                        stats.symbolicated_exceptions += 1;
                        exc
                    }
                    None => {
                        stats.unsymbolicated_exceptions += 1;
                        raw_exception
                    }
                },
            )
            .collect();

        if frame_order == FrameOrder::CallerFirst {
            // Stack frames were sent in "caller first" order. We want to process them
            // in "callee first" order.
            for st in &mut stacktraces {
                st.frames.reverse();
            }
        }

        let mut remapped_stacktraces: Vec<_> = stacktraces
            .into_iter()
            .map(|raw_stacktrace| {
                let mut frames = Self::map_stacktrace(
                    &mappers,
                    &raw_stacktrace.frames,
                    release_package.as_deref(),
                    &mut stats,
                );

                if frame_order == FrameOrder::CallerFirst {
                    // The symbolicated frames are expected in "caller first" order.
                    frames.reverse();
                }

                JvmStacktrace { frames }
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

        let remapped_classes = classes
            .into_iter()
            .filter_map(|class| {
                match mappers.iter().find_map(|mapper| mapper.remap_class(&class)) {
                    Some(remapped) => {
                        stats.symbolicated_classes += 1;
                        Some((class, Arc::from(remapped)))
                    }
                    None => {
                        stats.unsymbolicated_classes += 1;
                        None
                    }
                }
            })
            .collect();

        stats.num_stacktraces = remapped_stacktraces.len() as u64;

        record_symbolication_metrics(platform, stats);

        CompletedJvmSymbolicationResponse {
            exceptions: remapped_exceptions,
            stacktraces: remapped_stacktraces,
            classes: remapped_classes,
            errors,
        }
    }

    fn map_stacktrace(
        mappers: &[&proguard::ProguardCache],
        stacktrace: &[JvmFrame],
        release_package: Option<&str>,
        stats: &mut SymbolicationStats,
    ) -> Vec<JvmFrame> {
        let mut carried_outline_pos = vec![None; mappers.len()];
        let mut remapped_frames = Vec::new();

        'frames: for frame in stacktrace {
            let deobfuscated_signature = frame.signature.as_ref().and_then(|signature| {
                mappers
                    .iter()
                    .find_map(|mapper| mapper.deobfuscate_signature(signature))
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
                //
                // UPDATE(@loewenheim): The retrace implementation at https://dl.google.com/android/repository/commandlinetools-mac-11076708_latest.zip
                // returns the same value whether you give it line 0 or no line at all, and it is the same result that our implementation
                // gives with line 0. This indicates that the _behavior_ is correct, but we should be able to get there without
                // backfilling the line number with 0.
                .unwrap_or_else(|| proguard::StackFrame::new(&frame.module, &frame.function, 0));

            let mut mapped_result = None;

            let mut remap_buffer = Vec::new();
            for (mapper_idx, mapper) in mappers.iter().enumerate() {
                if mapper.is_outline_frame(proguard_frame.class(), proguard_frame.method()) {
                    carried_outline_pos[mapper_idx] = Some(proguard_frame.line());
                    continue 'frames;
                }

                let effective = mapper.prepare_frame_for_mapping(
                    &proguard_frame,
                    &mut carried_outline_pos[mapper_idx],
                );

                // First, try to remap the whole frame.
                if let Some(frames_out) =
                    Self::map_full_frame(mapper, frame, &effective, &mut remap_buffer)
                {
                    mapped_result = Some(frames_out);
                    break;
                }

                // Second, try to remap the frame's method.
                if let Some(frames_out) = Self::map_class_method(mapper, frame) {
                    mapped_result = Some(frames_out);
                    break;
                }

                // Third, try to remap just the frame's class.
                if let Some(frames_out) = Self::map_class(mapper, frame) {
                    mapped_result = Some(frames_out);
                    break;
                }
            }

            // Fix up the frames' in-app fields only if they were actually mapped
            if let Some(frames) = mapped_result.as_mut() {
                for mapped_frame in frames {
                    // mark the frame as in_app after deobfuscation based on the release package name
                    // only if it's not present
                    if let Some(package) = release_package
                        && mapped_frame.module.starts_with(package)
                        && mapped_frame.in_app.is_none()
                    {
                        mapped_frame.in_app = Some(true);
                    }

                    // Also count the frames as symbolicated at this point
                    *stats
                        .symbolicated_frames
                        .entry(mapped_frame.platform.clone())
                        .or_default() += 1;
                }
            }

            // If all else fails, just return the original frame.
            let mut frames = mapped_result.unwrap_or_else(|| {
                *stats
                    .unsymbolicated_frames
                    .entry(frame.platform.clone())
                    .or_default() += 1;
                vec![frame.clone()]
            });

            for mapped_frame in &mut frames {
                // add the signature if we received one and we were
                // able to translate/deobfuscate it
                if let Some(signature) = &deobfuscated_signature {
                    mapped_frame.signature = Some(signature.format_signature());
                }
            }

            remapped_frames.extend(frames);
        }

        remapped_frames
    }

    /// Remaps an exception using the provided mappers.
    ///
    /// This returns a new exception with the deobfuscated module and class names.
    /// Returns `None` if none of the `mappers` can remap the exception.
    #[tracing::instrument(skip_all)]
    fn map_exception(
        mappers: &[&proguard::ProguardCache],
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

    /// Tries to remap a `JvmFrame` using a `proguard::StackFrame`
    /// constructed from it.
    ///
    /// The `buf` parameter is used as a buffer for the frames returned
    /// by `remap_frame`.
    ///
    /// This function returns a list of frames because one frame may be expanded into
    /// a series of inlined frames. The returned list is sorted so that inlinees come before their callers.
    #[tracing::instrument(skip_all)]
    fn map_full_frame<'a>(
        mapper: &'a proguard::ProguardCache<'a>,
        original_frame: &JvmFrame,
        proguard_frame: &proguard::StackFrame<'a>,
        buf: &mut Vec<proguard::StackFrame<'a>>,
    ) -> Option<Vec<JvmFrame>> {
        buf.clear();
        buf.extend(mapper.remap_frame(proguard_frame));

        if buf.is_empty() {
            return None;
        }

        let res = buf
            .iter()
            .map(|new_frame| JvmFrame {
                module: new_frame.class().to_owned(),
                function: new_frame.method().to_owned(),
                lineno: Some(new_frame.line() as u32),
                abs_path: new_frame
                    .file()
                    .map(String::from)
                    .or_else(|| original_frame.abs_path.clone()),
                filename: new_frame
                    .file()
                    .map(String::from)
                    .or_else(|| original_frame.filename.clone()),
                method_synthesized: new_frame.method_synthesized(),
                ..original_frame.clone()
            })
            .collect();
        Some(res)
    }

    /// Tries to remap a frame's class and method.
    #[tracing::instrument(skip_all)]
    fn map_class_method(
        mapper: &proguard::ProguardCache,
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
    fn map_class(mapper: &proguard::ProguardCache, frame: &JvmFrame) -> Option<Vec<JvmFrame>> {
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
    #[tracing::instrument(skip_all)]
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
    use proguard::{ProguardCache, ProguardMapping};

    static MAPPING_OUTLINE_COMPLEX: &[u8] = include_bytes!("res/mapping-outline-complex.txt");

    fn remap_stacktrace_caller_first(
        proguard_source: &[u8],
        release_package: Option<&str>,
        frames: &mut [JvmFrame],
    ) -> Vec<JvmFrame> {
        frames.reverse();
        let mapping = ProguardMapping::new(proguard_source);
        let mut cache = Vec::new();
        ProguardCache::write(&mapping, &mut cache).unwrap();
        let cache = ProguardCache::parse(&cache).unwrap();
        cache.test();

        let mut remapped_frames = ProguardService::map_stacktrace(
            &[&cache],
            frames,
            release_package,
            &mut SymbolicationStats::default(),
        );

        remapped_frames.reverse();
        remapped_frames
    }

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
        let mut cache = Vec::new();
        ProguardCache::write(&mapping, &mut cache).unwrap();
        let cache = ProguardCache::parse(&cache).unwrap();
        cache.test();

        let exception = ProguardService::map_exception(&[&cache], &exception).unwrap();

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

        let mut frames = [
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

        let mapped_frames = remap_stacktrace_caller_first(proguard_source, None, &mut frames);

        insta::assert_yaml_snapshot!(mapped_frames, @r###"
        - function: onClick
          module: io.sentry.sample.-$$Lambda$r3Avcbztes2hicEObh02jjhQqd4
          lineno: 2
          index: 0
        - function: onClickHandler
          filename: MainActivity.java
          module: io.sentry.sample.MainActivity
          lineno: 40
          index: 1
        - function: foo
          filename: MainActivity.java
          module: io.sentry.sample.MainActivity
          lineno: 44
          index: 1
        - function: bar
          filename: MainActivity.java
          module: io.sentry.sample.MainActivity
          lineno: 54
          index: 1
        - function: onClickHandler
          module: io.sentry.sample.MainActivity
          lineno: 0
          index: 2
          signature: (android.view.View)
        - function: onClickHandler
          module: io.sentry.sample.MainActivity
          index: 3
          signature: (android.view.View)
        - function: onClickHandler
          module: io.sentry.sample.ClassDoesNotExist
          index: 4
          signature: (android.view.View)
        "###);
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

        let mut frames = [
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

        let mapped_frames =
            remap_stacktrace_caller_first(proguard_source, Some("org.slf4j"), &mut frames);

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

        let remapped = remap_stacktrace_caller_first(b"", Some("android"), &mut [frame]);

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

        let frame = JvmFrame {
            function: "onCreate".into(),
            module: "com.example.App".into(),
            abs_path: Some("App.java".into()),
            filename: Some("App.java".into()),
            index: 0,
            ..Default::default()
        };

        let mapped_frames = remap_stacktrace_caller_first(proguard_source, None, &mut [frame]);
        // Without the "line 0" change, the second frame doesn't exist.
        // The `retrace` implementation at
        // https://dl.google.com/android/repository/commandlinetools-mac-11076708_latest.zip
        // also returns this, no matter whether you give it line 0 or no line at all.
        insta::assert_yaml_snapshot!(mapped_frames, @r###"
        - function: onCreate
          filename: App.java
          module: com.example.App
          abs_path: App.java
          lineno: 0
          index: 0
        - function: barInternalInject
          filename: App.java
          module: com.example.App
          abs_path: App.java
          lineno: 0
          index: 0
        "###);
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

        let frame = JvmFrame {
            function: "run".into(),
            module: "com.google.firebase.concurrent.a".into(),
            abs_path: Some("CustomThreadFactory".into()),
            filename: Some("CustomThreadFactory".into()),
            index: 0,
            ..Default::default()
        };

        let mapped_frames = remap_stacktrace_caller_first(proguard_source, None, &mut [frame]);

        // Without the "line 0" change, the module is "com.google.firebase.concurrent.CustomThreadFactory$$ExternalSyntheticLambda0".
        // The `retrace` implementation at
        // https://dl.google.com/android/repository/commandlinetools-mac-11076708_latest.zip
        // also returns this, no matter whether you give it line 0 or no line at all.
        insta::assert_yaml_snapshot!(mapped_frames, @r###"
        - function: run$bridge
          filename: CustomThreadFactory
          module: com.google.firebase.concurrent.CustomThreadFactory$$InternalSyntheticLambda$1$53203795c28a6fcdb3bac755806c9ee73cb3e8dcd4c9bbf8ca5d25d4d9c378dd$0
          abs_path: CustomThreadFactory
          lineno: 0
          index: 0
          method_synthesized: true
        "###);
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

    #[test]
    fn remap_filename() {
        let proguard_source = br#"# compiler: R8
# compiler_version: 8.3.36
# min_api: 24
# common_typos_disable
# {"id":"com.android.tools.r8.mapping","version":"2.2"}
# pg_map_id: 48ffd94
# pg_map_hash: SHA-256 48ffd9478fda293e1c713db4cc7c449781a9e799fa504e389ee32ed19775a3ba
io.wzieba.r8fullmoderenamessources.Foobar -> a.a:
# {"id":"sourceFile","fileName":"Foobar.kt"}
    1:3:void <init>():3:3 -> <init>
    4:11:void <init>():5:5 -> <init>
    1:7:void foo():9:9 -> a
    8:15:void foo():10:10 -> a
io.wzieba.r8fullmoderenamessources.FoobarKt -> a.b:
# {"id":"sourceFile","fileName":"Foobar.kt"}
    1:5:void main():15:15 -> a
    6:9:void main():16:16 -> a
    1:4:void main(java.lang.String[]):0:0 -> b
io.wzieba.r8fullmoderenamessources.MainActivity -> io.wzieba.r8fullmoderenamessources.MainActivity:
# {"id":"sourceFile","fileName":"MainActivity.kt"}
    1:4:void <init>():7:7 -> <init>
    1:1:void $r8$lambda$pOQDVg57r6gG0-DzwbGf17BfNbs(android.view.View):0:0 -> a
      # {"id":"com.android.tools.r8.synthesized"}
    1:9:void onCreate$lambda$1$lambda$0(android.view.View):14:14 -> b
    1:3:void onCreate(android.os.Bundle):10:10 -> onCreate
    4:8:void onCreate(android.os.Bundle):12:12 -> onCreate
    9:16:void onCreate(android.os.Bundle):13:13 -> onCreate
    17:20:void onCreate(android.os.Bundle):12:12 -> onCreate
io.wzieba.r8fullmoderenamessources.MainActivity$$ExternalSyntheticLambda0 -> a.c:
# {"id":"sourceFile","fileName":"R8$$SyntheticClass"}
# {"id":"com.android.tools.r8.synthesized"}
    1:4:void onClick(android.view.View):0:0 -> onClick
      # {"id":"com.android.tools.r8.synthesized"}
io.wzieba.r8fullmoderenamessources.R -> a.d:
    void <init>() -> <init>
      # {"id":"com.android.tools.r8.synthesized"}"#;

        let mut frames: Vec<JvmFrame> = serde_json::from_str(
            r#"[{
            "function": "a",
            "abs_path": "SourceFile",
            "module": "a.a",
            "filename": "SourceFile",
            "lineno": 12,
            "index": 0
        }, {
            "function": "b",
            "abs_path": "SourceFile",
            "module": "io.wzieba.r8fullmoderenamessources.MainActivity",
            "filename": "SourceFile",
            "lineno": 6,
            "index": 1
        }, {
            "function": "a",
            "abs_path": "SourceFile",
            "module": "io.wzieba.r8fullmoderenamessources.MainActivity",
            "filename": "SourceFile",
            "lineno": 1,
            "index": 2
        }, {
            "function": "onClick",
            "abs_path": "SourceFile",
            "module": "a.c",
            "filename": "SourceFile",
            "lineno": 1,
            "index": 3
        }, {
            "function": "performClick",
            "abs_path": "View.java",
            "module": "android.view.View",
            "filename": "View.java",
            "lineno": 7659,
            "index": 4
        }, {
            "function": "performClickInternal",
            "abs_path": "View.java",
            "module": "android.view.View",
            "filename": "View.java",
            "lineno": 7636,
            "index": 5
        }, {
            "function": "performClickInternal",
            "abs_path": "Unknown Source",
            "module": "android.view.View.-$$Nest$m",
            "filename": "Unknown Source",
            "lineno": 0,
            "index": 6
        }]"#,
        )
        .unwrap();

        let mapped_frames = remap_stacktrace_caller_first(proguard_source, None, &mut frames);

        insta::assert_yaml_snapshot!(mapped_frames, @r###"
        - function: foo
          filename: Foobar.kt
          module: io.wzieba.r8fullmoderenamessources.Foobar
          abs_path: Foobar.kt
          lineno: 10
          index: 0
        - function: onCreate$lambda$1$lambda$0
          filename: MainActivity.kt
          module: io.wzieba.r8fullmoderenamessources.MainActivity
          abs_path: MainActivity.kt
          lineno: 14
          index: 1
        - function: $r8$lambda$pOQDVg57r6gG0-DzwbGf17BfNbs
          filename: MainActivity.kt
          module: io.wzieba.r8fullmoderenamessources.MainActivity
          abs_path: MainActivity.kt
          lineno: 0
          index: 2
          method_synthesized: true
        - function: onClick
          filename: MainActivity
          module: io.wzieba.r8fullmoderenamessources.MainActivity$$ExternalSyntheticLambda0
          abs_path: MainActivity
          lineno: 0
          index: 3
          method_synthesized: true
        - function: performClick
          filename: View.java
          module: android.view.View
          abs_path: View.java
          lineno: 7659
          index: 4
        - function: performClickInternal
          filename: View.java
          module: android.view.View
          abs_path: View.java
          lineno: 7636
          index: 5
        - function: performClickInternal
          filename: Unknown Source
          module: android.view.View.-$$Nest$m
          abs_path: Unknown Source
          lineno: 0
          index: 6
        "###);
    }

    #[test]
    fn remap_filename_inlined() {
        let proguard_source = br#"# compiler: R8
# compiler: R8
# compiler_version: 8.11.18
# min_api: 24
# common_typos_disable
# {"id":"com.android.tools.r8.mapping","version":"2.2"}
# pg_map_id: 7e6e8e1
# pg_map_hash: SHA-256 7e6e8e1cad51270880af7ead018948d8156402b211c30e81ba8b310f002689dd
com.mycompany.android.StuffKt$$ExternalSyntheticOutline0 -> ev.h:
# {"id":"sourceFile","fileName":"R8$$SyntheticClass"}
# {"id":"com.android.tools.r8.synthesized"}
    1:1:void ev.StuffKt$$ExternalSyntheticOutline0.m(android.media.MediaMetadataRetriever):0:0 -> a
      # {"id":"com.android.tools.r8.synthesized"}
    1:2:void ev.StuffKt$$ExternalSyntheticOutline0.m(java.lang.String,me.company.android.logging.L):0:0 -> b
      # {"id":"com.android.tools.r8.synthesized"}
      # {"id":"com.android.tools.r8.outline"}
    3:5:void ev.StuffKt$$ExternalSyntheticOutline0.m(java.lang.String,me.company.android.logging.L):1:1 -> b
    6:9:void ev.StuffKt$$ExternalSyntheticOutline0.m(java.lang.String,me.company.android.logging.L):2:2 -> b
com.mycompany.android.MapAnnotations -> uu0.k:
# {"id":"sourceFile","fileName":"MapAnnotations.kt"}
    43:46:com.mycompany.android.IProjectionMarker createProjectionMarker(com.mycompany.android.IProjectionMarkerOptions):0:0 -> l
    43:46:lv0.IProjectionMarker uu0.MapAnnotations.createProjectionMarker(lv0.IProjectionMarkerOptions):0 -> l
      # {"id":"com.android.tools.r8.outlineCallsite","positions":{"1":50,"3":52,"6":55},"outline":"Lev/h;b(Ljava/lang/String;Lme/company/android/logging/L;)V"}
com.mycompany.android.Renderer -> b80.f:
# {"id":"sourceFile","fileName":"Renderer.kt"}
    33:40:com.mycompany.android.ViewProjectionMarker com.mycompany.android.Delegate.createProjectionMarker():101:101 -> a
    33:40:void com.mycompany.android.Delegate.render():34 -> a
    33:40:void render():39 -> a
com.mycompany.android.Delegate -> b80.h:
# {"id":"sourceFile","fileName":"Delegate.kt"}
    com.mycompany.android.IMapAnnotations mapAnnotations -> a
      # {"id":"com.android.tools.r8.residualsignature","signature":"Lbv0/b;"}"#;

        let mut frames: Vec<JvmFrame> = serde_json::from_str(
            r#"[
            {
              "function": "a",
              "module": "b80.f",
              "filename": "SourceFile",
              "abs_path": "SourceFile",
              "lineno": 33,
              "index": 0
            },
            {
              "function": "l",
              "module": "uu0.k",
              "filename": "SourceFile",
              "abs_path": "SourceFile",
              "lineno": 43,
              "index": 1
            },
            {
              "function": "b",
              "module": "ev.h",
              "filename": "SourceFile",
              "abs_path": "SourceFile",
              "lineno": 3,
              "index": 2
            }]"#,
        )
        .unwrap();

        let mapped_frames = remap_stacktrace_caller_first(proguard_source, None, &mut frames);

        insta::assert_yaml_snapshot!(mapped_frames, @r###"
        - function: render
          filename: Renderer.kt
          module: com.mycompany.android.Renderer
          abs_path: Renderer.kt
          lineno: 39
          index: 0
        - function: render
          filename: Delegate.kt
          module: com.mycompany.android.Delegate
          abs_path: Delegate.kt
          lineno: 34
          index: 0
        - function: createProjectionMarker
          filename: Delegate.kt
          module: com.mycompany.android.Delegate
          abs_path: Delegate.kt
          lineno: 101
          index: 0
        - function: createProjectionMarker
          filename: SourceFile
          module: com.mycompany.android.MapAnnotations
          abs_path: SourceFile
          lineno: 43
          index: 1
        "###);
    }

    #[test]
    fn remap_outline() {
        let proguard_source = br#"# compiler: R8
# compiler_version: 2.0
# min_api: 15
outline.Class -> a:
    1:2:int outline() -> a
# {"id":"com.android.tools.r8.outline"}
some.Class -> b:
    4:4:int outlineCaller(int):98:98 -> s
    5:5:int outlineCaller(int):100:100 -> s
    27:27:int outlineCaller(int):0:0 -> s
# {"id":"com.android.tools.r8.outlineCallsite","positions":{"1":4,"2":5},"outline":"La;a()I"}
"#;

        let mut frames = [
            JvmFrame {
                function: "s".to_owned(),
                module: "b".to_owned(),
                lineno: Some(27),
                index: 1,
                ..Default::default()
            },
            JvmFrame {
                function: "a".to_owned(),
                module: "a".to_owned(),
                lineno: Some(1),
                index: 0,
                ..Default::default()
            },
        ];

        let remapped = remap_stacktrace_caller_first(proguard_source, None, &mut frames);

        assert_eq!(remapped.len(), 1);
        assert_eq!(remapped[0].module, "some.Class");
        assert_eq!(remapped[0].function, "outlineCaller");
        assert_eq!(remapped[0].lineno, Some(98));
        assert_eq!(remapped[0].index, 1);
    }

    #[test]
    fn remap_outline_complex() {
        let mut frames: Vec<JvmFrame> = serde_json::from_str(
            r#"[
            {
              "function": "run",
              "module": "android.view.Choreographer$CallbackRecord",
              "filename": "Choreographer.java",
              "lineno": 1899,
              "index": 13
            },
            {
              "function": "doFrame",
              "module": "w2.q0$c",
              "filename": "SourceFile",
              "abs_path": "SourceFile",
              "lineno": 48,
              "index": 12
            },
            {
              "function": "doFrame",
              "module": "w2.r0$c",
              "filename": "SourceFile",
              "abs_path": "SourceFile",
              "lineno": 7,
              "index": 11
            },
            {
              "function": "invoke",
              "module": "h1.e3",
              "filename": "SourceFile",
              "abs_path": "SourceFile",
              "lineno": 231,
              "index": 10
            },
            {
              "function": "m",
              "module": "h1.y",
              "filename": "SourceFile",
              "abs_path": "SourceFile",
              "lineno": 6,
              "index": 9
            },
            {
              "function": "A",
              "module": "h1.y",
              "filename": "SourceFile",
              "abs_path": "SourceFile",
              "lineno": 111,
              "index": 8
            },
            {
              "function": "c",
              "module": "p1.k",
              "filename": "SourceFile",
              "abs_path": "SourceFile",
              "lineno": 135,
              "index": 7
            },
            {
              "function": "d",
              "module": "h1.p0",
              "filename": "SourceFile",
              "abs_path": "SourceFile",
              "lineno": 5,
              "index": 6
            },
            {
              "function": "invoke",
              "module": "er3.g$a",
              "filename": "SourceFile",
              "abs_path": "SourceFile",
              "lineno": 36,
              "index": 5
            },
            {
              "function": "d",
              "module": "yv0.g",
              "filename": "SourceFile",
              "abs_path": "SourceFile",
              "lineno": 17,
              "index": 4
            },
            {
              "function": "invoke",
              "module": "er3.f",
              "filename": "SourceFile",
              "abs_path": "SourceFile",
              "lineno": 3,
              "index": 3
            },
            {
              "function": "a",
              "module": "b80.f",
              "filename": "SourceFile",
              "abs_path": "SourceFile",
              "lineno": 33,
              "index": 2
            },
            {
              "function": "l",
              "module": "uu0.k",
              "filename": "SourceFile",
              "abs_path": "SourceFile",
              "lineno": 43,
              "index": 1
            },
            {
              "function": "b",
              "module": "ev.h",
              "filename": "SourceFile",
              "abs_path": "SourceFile",
              "lineno": 3,
              "index": 0
            }]"#,
        )
        .unwrap();

        let remapped_frames =
            remap_stacktrace_caller_first(MAPPING_OUTLINE_COMPLEX, None, &mut frames);

        insta::assert_yaml_snapshot!(
            remapped_frames,
            @r###"
        - function: run
          filename: Choreographer.java
          module: android.view.Choreographer$CallbackRecord
          lineno: 1899
          index: 13
        - function: doFrame
          filename: AndroidUiDispatcher.android.kt
          module: androidx.compose.ui.platform.AndroidUiDispatcher$dispatchCallback$1
          abs_path: AndroidUiDispatcher.android.kt
          lineno: 69
          index: 12
        - function: access$performFrameDispatch
          filename: AndroidUiDispatcher.android.kt
          module: androidx.compose.ui.platform.AndroidUiDispatcher
          abs_path: AndroidUiDispatcher.android.kt
          lineno: 41
          index: 12
        - function: performFrameDispatch
          filename: AndroidUiDispatcher.android.kt
          module: androidx.compose.ui.platform.AndroidUiDispatcher
          abs_path: AndroidUiDispatcher.android.kt
          lineno: 108
          index: 12
        - function: doFrame
          filename: AndroidUiFrameClock.android.kt
          module: androidx.compose.ui.platform.AndroidUiFrameClock$withFrameNanos$2$callback$1
          abs_path: AndroidUiFrameClock.android.kt
          lineno: 39
          index: 11
        - function: invokeSuspend$lambda$22
          filename: Recomposer.kt
          module: androidx.compose.runtime.Recomposer$runRecomposeAndApplyChanges$2
          abs_path: Recomposer.kt
          lineno: 705
          index: 10
        - function: applyChanges
          filename: Composition.kt
          module: androidx.compose.runtime.CompositionImpl
          abs_path: Composition.kt
          lineno: 1149
          index: 9
        - function: applyChangesInLocked
          filename: Composition.kt
          module: androidx.compose.runtime.CompositionImpl
          abs_path: Composition.kt
          lineno: 1122
          index: 8
        - function: dispatchRememberObservers
          filename: RememberEventDispatcher.kt
          module: androidx.compose.runtime.internal.RememberEventDispatcher
          abs_path: RememberEventDispatcher.kt
          lineno: 225
          index: 7
        - function: dispatchRememberList
          filename: RememberEventDispatcher.kt
          module: androidx.compose.runtime.internal.RememberEventDispatcher
          abs_path: RememberEventDispatcher.kt
          lineno: 253
          index: 7
        - function: onRemembered
          filename: Effects.kt
          module: androidx.compose.runtime.DisposableEffectImpl
          abs_path: Effects.kt
          lineno: 85
          index: 6
        - function: invoke
          filename: CurrentLocationMarkerMapCollection.kt
          module: com.example.map.internal.CurrentLocationMarkerMapCollectionKt$CurrentLocationMarkerMapCollection$1$1
          abs_path: CurrentLocationMarkerMapCollection.kt
          lineno: 35
          index: 5
        - function: invoke
          filename: CurrentLocationMarkerMapCollection.kt
          module: com.example.map.internal.CurrentLocationMarkerMapCollectionKt$CurrentLocationMarkerMapCollection$1$1
          abs_path: CurrentLocationMarkerMapCollection.kt
          lineno: 40
          index: 5
        - function: addMapReadyCallback
          filename: MapboxMapView.kt
          module: com.example.mapbox.MapboxMapView
          abs_path: MapboxMapView.kt
          lineno: 368
          index: 4
        - function: invoke
          filename: CurrentLocationMarkerMapCollection.kt
          module: com.example.map.internal.CurrentLocationMarkerMapCollectionKt$CurrentLocationMarkerMapCollection$1$1$mapReadyCallback$1
          abs_path: CurrentLocationMarkerMapCollection.kt
          lineno: 36
          index: 3
        - function: invoke
          filename: CurrentLocationMarkerMapCollection.kt
          module: com.example.map.internal.CurrentLocationMarkerMapCollectionKt$CurrentLocationMarkerMapCollection$1$1$mapReadyCallback$1
          abs_path: CurrentLocationMarkerMapCollection.kt
          lineno: 36
          index: 3
        - function: render
          filename: CurrentLocationRenderer.kt
          module: com.example.mapcomponents.marker.currentlocation.CurrentLocationRenderer
          abs_path: CurrentLocationRenderer.kt
          lineno: 39
          index: 2
        - function: render
          filename: DotRendererDelegate.kt
          module: com.example.mapcomponents.marker.currentlocation.DotRendererDelegate
          abs_path: DotRendererDelegate.kt
          lineno: 34
          index: 2
        - function: createCurrentLocationProjectionMarker
          filename: DotRendererDelegate.kt
          module: com.example.mapcomponents.marker.currentlocation.DotRendererDelegate
          abs_path: DotRendererDelegate.kt
          lineno: 101
          index: 2
        - function: createProjectionMarker
          filename: MapAnnotations.kt
          module: com.example.MapAnnotations
          abs_path: MapAnnotations.kt
          lineno: 63
          index: 1
        - function: createProjectionMarker
          filename: MapProjectionViewController.kt
          module: com.example.projection.MapProjectionViewController
          abs_path: MapProjectionViewController.kt
          lineno: 79
          index: 1
        - function: createProjectionMarkerInternal
          filename: MapProjectionViewController.kt
          module: com.example.projection.MapProjectionViewController
          abs_path: MapProjectionViewController.kt
          lineno: 133
          index: 1
        - function: onProjectionView
          filename: MapProjectionViewController.kt
          module: com.example.projection.MapProjectionViewController
          abs_path: MapProjectionViewController.kt
          lineno: 160
          index: 1
        "###);
    }
}
