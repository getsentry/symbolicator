use symbolicator_service::services::symbolication::SymbolicationActor;

#[derive(Debug)]
pub struct RawProcessor {
    pub(crate) service: SymbolicationActor,
}

impl RawProcessor {
    pub fn new(service: SymbolicationActor) -> Self {
        Self { service }
    }
}
