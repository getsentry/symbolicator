use crate::interface::{
    CompletedJvmSymbolicationResponse, JvmException, SymbolicateJvmStacktraces,
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
