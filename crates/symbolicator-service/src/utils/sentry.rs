use symbolicator_sources::ObjectId;

/// Write own data to [`sentry::Scope`], only the subset that is considered useful for debugging.
pub trait ConfigureScope {
    /// Writes information to the given scope.
    fn to_scope(&self, scope: &mut sentry::Scope);

    /// Configures the current scope.
    fn configure_scope(&self) {
        sentry::configure_scope(|scope| self.to_scope(scope));
    }
}

impl ConfigureScope for ObjectId {
    fn to_scope(&self, scope: &mut sentry::Scope) {
        scope.set_tag(
            "object_id.code_id",
            self.code_id
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "None".to_string()),
        );
        scope.set_tag(
            "object_id.code_file_basename",
            self.code_file_basename().unwrap_or("None"),
        );
        scope.set_tag(
            "object_id.debug_id",
            self.debug_id
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "None".to_string()),
        );
        scope.set_tag(
            "object_id.debug_file_basename",
            self.debug_file_basename().unwrap_or("None"),
        );
        scope.set_tag("object_id.object_type", self.object_type.to_string());
    }
}
