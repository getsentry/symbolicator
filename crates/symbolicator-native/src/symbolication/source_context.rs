use crate::interface::CompleteStacktrace;

use super::module_lookup::ModuleLookup;
use super::symbolicate::SymbolicationActor;

impl SymbolicationActor {
    pub async fn apply_source_context(
        &self,
        module_lookup: &mut ModuleLookup,
        stacktraces: &mut [CompleteStacktrace],
    ) {
        module_lookup
            .fetch_sources(self.objects.clone(), stacktraces)
            .await;

        let debug_sessions = module_lookup.prepare_debug_sessions();

        for trace in stacktraces {
            for frame in &mut trace.frames {
                module_lookup.try_set_source_context(&debug_sessions, &mut frame.raw)
            }
        }
    }
}
