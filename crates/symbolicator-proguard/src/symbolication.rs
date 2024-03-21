use crate::interface::{
    CompletedJvmSymbolicationResponse, JvmException, JvmFrame, JvmStacktrace, ProguardError,
    ProguardErrorKind, SymbolicateJvmStacktraces,
};
use crate::ProguardService;

use futures::future;
use symbolicator_service::caching::CacheError;

impl ProguardService {
    /// Symbolicates a JVM event.
    ///
    /// "Symbolicate" here means that exceptions and stack
    /// frames are remapped using proguard files.
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
            ..
        } = request;

        let maybe_mappers = future::join_all(modules.iter().map(|module| async {
            let file = self
                .download_proguard_file(&sources, &scope, module.uuid)
                .await;
            (module.uuid, file)
        }))
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

        let remapped_exceptions = exceptions
            .into_iter()
            .map(|raw_exception| {
                Self::map_exception(&mappers, &raw_exception).unwrap_or(raw_exception)
            })
            .collect();

        let remapped_stacktraces = stacktraces
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
        let proguard_frame =
            proguard::StackFrame::new(&frame.module, &frame.function, frame.lineno as usize);
        let mut mapped_frames = Vec::new();

        // first, try to remap complete frames
        for mapper in mappers {
            mapped_frames.clear();
            mapped_frames.extend(mapper.remap_frame(&proguard_frame));

            if mapped_frames.is_empty() {
                continue;
            }

            let bottom_class = mapped_frames[mapped_frames.len() - 1].class();

            // sentry expects stack traces in reverse order
            return mapped_frames
                .iter()
                .rev()
                .map(|new_frame| {
                    let mut mapped_frame = JvmFrame {
                        module: new_frame.class().to_owned(),
                        function: new_frame.method().to_owned(),
                        lineno: new_frame.line() as u32,
                        ..frame.clone()
                    };

                    // clear the filename for all *foreign* classes
                    if mapped_frame.module != bottom_class {
                        mapped_frame.filename = None;
                        mapped_frame.abs_path = None;
                    }

                    // mark the frame as in_app after deobfuscation based on the release package name
                    // only if it's not present
                    if let Some(package) = release_package {
                        if mapped_frame.module.starts_with(package) && mapped_frame.in_app.is_none()
                        {
                            mapped_frame.in_app = Some(true);
                        }
                    }
                    mapped_frame
                })
                .collect();
        }

        // second, if that is not possible, try to re-map only the class-name
        for mapper in mappers {
            let Some(mapped_class) = mapper.remap_class(&frame.module) else {
                continue;
            };

            let mut mapped_frame = JvmFrame {
                module: mapped_class.to_owned(),
                ..frame.clone()
            };

            // mark the frame as in_app after deobfuscation based on the release package name
            // only if it's not present
            if let Some(package) = release_package {
                if mapped_frame.module.starts_with(package) && mapped_frame.in_app.is_none() {
                    mapped_frame.in_app = Some(true);
                }
            }

            return vec![mapped_frame];
        }

        // Return the raw frame if remapping didn't work
        vec![frame.clone()]
    }
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
                lineno: 2,
                index: 0,
                ..Default::default()
            },
            JvmFrame {
                function: "t".to_owned(),
                module: "io.sentry.sample.MainActivity".to_owned(),
                filename: Some("MainActivity.java".to_owned()),
                lineno: 1,
                index: 1,
                ..Default::default()
            },
        ];

        let mapping = ProguardMapping::new(proguard_source);
        let mapper = ProguardMapper::new(mapping);

        let mapped_frames: Vec<_> = frames
            .iter()
            .flat_map(|frame| ProguardService::map_frame(&[&mapper], frame, None).into_iter())
            .collect();

        assert_eq!(mapped_frames.len(), 4);

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
        assert_eq!(mapped_frames[1].lineno, 40);
        assert_eq!(mapped_frames[1].index, 1);

        assert_eq!(mapped_frames[2].function, "foo");
        assert_eq!(mapped_frames[2].lineno, 44);
        assert_eq!(mapped_frames[2].index, 1);

        assert_eq!(mapped_frames[3].function, "bar");
        assert_eq!(mapped_frames[3].lineno, 54);
        assert_eq!(
            mapped_frames[3].filename,
            Some("MainActivity.java".to_owned())
        );
        assert_eq!(mapped_frames[3].module, "io.sentry.sample.MainActivity");
        assert_eq!(mapped_frames[3].index, 1);
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
                lineno: 67,
                ..Default::default()
            },
            JvmFrame {
                function: "a".to_owned(),
                module: "org.a.b.g$a".to_owned(),
                lineno: 69,
                in_app: Some(false),
                ..Default::default()
            },
            JvmFrame {
                function: "a".to_owned(),
                module: "org.a.b.g$a".to_owned(),
                lineno: 68,
                in_app: Some(true),
                ..Default::default()
            },
            JvmFrame {
                function: "init".to_owned(),
                module: "com.android.Zygote".to_owned(),
                lineno: 62,
                ..Default::default()
            },
            JvmFrame {
                function: "a".to_owned(),
                module: "org.a.b.g$b".to_owned(),
                lineno: 70,
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
}
