use crate::interface::{
    CompletedJvmSymbolicationResponse, JvmException, JvmFrame, SymbolicateJvmStacktraces,
};
use crate::ProguardService;

use futures::future;

impl ProguardService {
    pub async fn symbolicate_jvm(
        &self,
        request: SymbolicateJvmStacktraces,
    ) -> CompletedJvmSymbolicationResponse {
        let SymbolicateJvmStacktraces {
            scope,
            sources,
            exceptions,
            modules,
            ..
        } = request;
        let mappers = future::join_all(
            modules
                .iter()
                .map(|module| self.download_proguard_file(&sources, &scope, module.uuid)),
        )
        .await;

        // TODO: error handling/reporting
        let mappers: Vec<_> = mappers
            .iter()
            .filter_map(|res| match res {
                Ok(mapper) => Some(mapper.get()),
                Err(_e) => None,
            })
            .collect();

        let mut remapped_exceptions = Vec::with_capacity(exceptions.len());

        for raw_exception in exceptions {
            remapped_exceptions
                .push(Self::map_exception(&mappers, &raw_exception).unwrap_or(raw_exception));
        }

        CompletedJvmSymbolicationResponse {
            exceptions: remapped_exceptions,
        }
    }

    fn map_exception(
        mappers: &[&proguard::ProguardMapper],
        exception: &JvmException,
    ) -> Option<JvmException> {
        if mappers.is_empty() {
            return None;
        }

        let key = format!("{}.{}", exception.module, exception.ty);

        let mapped = mappers.iter().find_map(|mapper| mapper.remap_class(&key))?;

        // TOOD: Capture/log error
        let (new_module, new_ty) = mapped.rsplit_once('.')?;

        Some(JvmException {
            ty: new_ty.into(),
            module: new_module.into(),
        })
    }

    fn map_frame(mappers: &[&proguard::ProguardMapper], frame: &JvmFrame) -> Vec<JvmFrame> {
        let proguard_frame =
            proguard::StackFrame::new(&frame.class, &frame.method, frame.lineno as usize);
        let mut mapped_frames = Vec::new();

        // first, try to remap complete frames
        for mapper in mappers {
            mapped_frames.clear();
            mapped_frames.extend(mapper.remap_frame(&proguard_frame));

            if !mapped_frames.is_empty() {
                let mut result = Vec::new();
                let bottom_class = mapped_frames[mapped_frames.len() - 1].class();

                // sentry expects stack traces in reverse order
                for new_frame in mapped_frames.iter().rev() {
                    let mut mapped_frame = JvmFrame {
                        class: new_frame.class().to_owned(),
                        method: new_frame.method().to_owned(),
                        lineno: new_frame.line() as u32,
                        ..frame.clone()
                    };

                    // clear the filename for all *foreign* classes
                    if mapped_frame.class != bottom_class {
                        mapped_frame.filename = None;
                        mapped_frame.abs_path = None;
                    }

                    // TODO: in_app handing based on release
                    // // mark the frame as in_app after deobfuscation based on the release package name
                    // // only if it's not present
                    // if release and release.package and frame.get("in_app") is None:
                    //     if frame["module"].startswith(release.package):
                    //         frame["in_app"] = True

                    result.push(mapped_frame);
                }

                return result;
            }
        }

        // second, if that is not possible, try to re-map only the class-name
        for mapper in mappers {
            if let Some(mapped_class) = mapper.remap_class(&frame.class) {
                let mapped_frame = JvmFrame {
                    class: mapped_class.to_owned(),
                    ..frame.clone()
                };

                // // mark the frame as in_app after deobfuscation based on the release package name
                // // only if it's not present
                // if release and release.package and frame.get("in_app") is None:
                //     if frame["module"].startswith(release.package):
                //         frame["in_app"] = True

                return vec![mapped_frame];
            }
        }

        Vec::new()
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
                method: "onClick".to_owned(),
                class: "e.a.c.a".to_owned(),
                lineno: 2,
                ..Default::default()
            },
            JvmFrame {
                method: "t".to_owned(),
                class: "io.sentry.sample.MainActivity".to_owned(),
                filename: Some("MainActivity.java".to_owned()),
                lineno: 1,
                ..Default::default()
            },
        ];

        let mapping = ProguardMapping::new(proguard_source);
        let mapper = ProguardMapper::new(mapping);

        let mapped_frames: Vec<_> = frames
            .iter()
            .flat_map(|frame| ProguardService::map_frame(&[&mapper], frame).into_iter())
            .collect();

        assert_eq!(mapped_frames.len(), 4);

        assert_eq!(mapped_frames[0].method, "onClick");
        assert_eq!(
            mapped_frames[0].class,
            "io.sentry.sample.-$$Lambda$r3Avcbztes2hicEObh02jjhQqd4"
        );

        assert_eq!(
            mapped_frames[1].filename,
            Some("MainActivity.java".to_owned())
        );
        assert_eq!(mapped_frames[1].class, "io.sentry.sample.MainActivity");
        assert_eq!(mapped_frames[1].method, "onClickHandler");
        assert_eq!(mapped_frames[1].lineno, 40);

        assert_eq!(mapped_frames[2].method, "foo");
        assert_eq!(mapped_frames[2].lineno, 44);

        assert_eq!(mapped_frames[3].method, "bar");
        assert_eq!(mapped_frames[3].lineno, 54);
        assert_eq!(
            mapped_frames[3].filename,
            Some("MainActivity.java".to_owned())
        );
        assert_eq!(mapped_frames[3].class, "io.sentry.sample.MainActivity");
    }
}
