use tracing_subscriber::fmt::time::UtcTime;
use tracing_subscriber::fmt::{MakeWriter, fmt};
use tracing_subscriber::prelude::*;
use tracing_subscriber::util::SubscriberInitExt;

pub fn init_json_logging<W>(env_filter: &str, make_writer: W)
where
    W: for<'writer> MakeWriter<'writer> + Send + Sync + 'static,
{
    fmt()
        .with_timer(UtcTime::rfc_3339())
        .with_target(true)
        .with_env_filter(env_filter)
        .json()
        .flatten_event(true)
        .with_current_span(true)
        .with_span_list(true)
        .with_file(true)
        .with_line_number(true)
        .with_writer(make_writer)
        .finish()
        .with(sentry::integrations::tracing::layer())
        .init();
}
