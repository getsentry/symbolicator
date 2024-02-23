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
                let bottom_class = mapped_frames[mapped_frames.len()].class();

                // sentry expects stack traces in reverse order
                for new_frame in mapped_frames.iter().rev() {
                    let mut mapped_frame = JvmFrame {
                        class: new_frame.class().to_owned(),
                        method: new_frame.class().to_owned(),
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
}
