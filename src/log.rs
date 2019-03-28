use std::fmt;

use failure::AsFail;

/// Returns whether backtrace printing is enabled.
pub fn backtrace_enabled() -> bool {
    match std::env::var("RUST_BACKTRACE").as_ref().map(|x| x.as_str()) {
        Ok("1") | Ok("full") => true,
        _ => false,
    }
}
/// A wrapper around a `Fail` that prints its causes.
pub struct LogError<'a, E: AsFail>(pub &'a E);

impl<'a, E: AsFail> fmt::Display for LogError<'a, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let fail = self.0.as_fail();

        write!(f, "{}", fail)?;
        for cause in fail.iter_causes() {
            write!(f, "\n  caused by: {}", cause)?;
        }

        if backtrace_enabled() {
            if let Some(backtrace) = fail.backtrace() {
                write!(f, "\n\n{:?}", backtrace)?;
            }
        }

        Ok(())
    }
}
