/// Write own data to [`sentry::Scope`], only the subset that is considered useful for debugging.
pub trait ConfigureScope {
    /// Writes information to the given scope.
    fn to_scope(&self, scope: &mut sentry::Scope);

    /// Configures the current scope.
    fn configure_scope(&self) {
        sentry::configure_scope(|scope| self.to_scope(scope));
    }
}
