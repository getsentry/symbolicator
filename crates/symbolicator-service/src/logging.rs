use std::cell::RefCell;
use std::collections::BTreeMap;
use std::error::Error;

use sentry::integrations::tracing::{breadcrumb_from_event, event_from_event};
use sentry::protocol::Exception;
use sentry::{event_from_error, TransactionOrSpan};
use serde::ser::SerializeMap as _;
use serde::{Serialize, Serializer};
use serde_json::Value;
use time::OffsetDateTime;
use tracing::field::{Field, Visit};
use tracing::{span, Event, Level, Subscriber};
use tracing_serde::AsSerde;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::{LookupSpan, SpanRef};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{prelude::*, registry, EnvFilter, Registry};

pub fn init_json_logging<W>(env_filter: &str, make_writer: W)
where
    W: for<'writer> MakeWriter<'writer> + Send + Sync + 'static,
{
    Registry::default()
        .with(SymbolicatorLayer(make_writer).with_filter(EnvFilter::new(env_filter)))
        .init();
}

struct SymbolicatorLayer<W>(W);

/// Data that is attached to the tracing Spans `extensions`, in order to
/// `finish` the corresponding sentry span `on_close`, and re-set its parent as
/// the *current* span.
struct SymbolicatorSpanData {
    span_attrs: BTreeMap<&'static str, Value>,
    sentry_span: TransactionOrSpan,
    parent_sentry_span: Option<TransactionOrSpan>,
}

#[derive(Serialize)]
struct JsonFormattedLog<'s, 'c, Span>
where
    Span: Subscriber + for<'lookup> registry::LookupSpan<'lookup>,
{
    level: tracing_serde::SerializeLevel<'s>,
    target: &'s str,
    #[serde(skip_serializing_if = "Option::is_none")]
    filename: Option<&'s str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    line_number: Option<u32>,

    #[serde(with = "time::serde::rfc3339")]
    timestamp: OffsetDateTime,

    #[serde(flatten)]
    event: EventFieldsAdapter<'s>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(bound(serialize = ""))]
    span: Option<SerializableSpan<'s, 'c, Span>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(bound(serialize = ""))]
    spans: Option<SerializableContext<'s, 'c, Span>>,
}

struct SerializableContext<'a, 'b, Span>(&'b Context<'a, Span>)
where
    Span: Subscriber + for<'lookup> LookupSpan<'lookup>;

impl<'a, 'b, Span> serde::ser::Serialize for SerializableContext<'a, 'b, Span>
where
    Span: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn serialize<Ser>(&self, serializer_o: Ser) -> Result<Ser::Ok, Ser::Error>
    where
        Ser: serde::ser::Serializer,
    {
        use serde::ser::SerializeSeq;
        let mut serializer = serializer_o.serialize_seq(None)?;

        if let Some(leaf_span) = self.0.lookup_current() {
            for span in leaf_span.scope().from_root() {
                serializer.serialize_element(&SerializableSpan(&span))?;
            }
        }

        serializer.end()
    }
}

struct SerializableSpan<'a, 'b, Span>(&'b SpanRef<'a, Span>)
where
    Span: for<'lookup> LookupSpan<'lookup>;

impl<'a, 'b, Span> serde::ser::Serialize for SerializableSpan<'a, 'b, Span>
where
    Span: for<'lookup> LookupSpan<'lookup>,
{
    fn serialize<Ser>(&self, serializer: Ser) -> Result<Ser::Ok, Ser::Error>
    where
        Ser: serde::ser::Serializer,
    {
        let mut serializer = serializer.serialize_map(None)?;

        let ext = self.0.extensions();
        let data = ext
            .get::<SymbolicatorSpanData>()
            .expect("Unable to find `SymbolicatorSpanData` in extensions; this is a bug");

        for (key, value) in &data.span_attrs {
            serializer.serialize_entry(*key, value)?;
        }
        serializer.serialize_entry("name", self.0.metadata().name())?;
        serializer.end()
    }
}

struct EventFieldsAdapter<'s>(&'s Event<'s>);

impl Serialize for EventFieldsAdapter<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let serializer = serializer.serialize_map(None)?;
        let mut visitor = tracing_serde::SerdeMapVisitor::new(serializer);
        self.0.record(&mut visitor);
        let serializer = visitor.take_serializer()?;
        serializer.end()
    }
}

impl<S, W> Layer<S> for SymbolicatorLayer<W>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    W: for<'writer> MakeWriter<'writer> + 'static,
{
    fn on_event(&self, event: &Event, ctx: Context<'_, S>) {
        match *event.metadata().level() {
            Level::ERROR => {
                sentry::capture_event(event_from_event(event, None::<Context<'_, S>>));
            }
            Level::WARN | Level::INFO => {
                sentry::add_breadcrumb(breadcrumb_from_event(event));
            }
            _ => {}
        }

        let meta = event.metadata();
        let current_span = event
            .parent()
            .and_then(|id| ctx.span(id))
            .or_else(|| ctx.lookup_current());

        let json = JsonFormattedLog {
            level: meta.level().as_serde(),
            target: meta.target(),
            filename: meta.file(),
            line_number: meta.line(),
            timestamp: OffsetDateTime::now_utc(),
            event: EventFieldsAdapter(event),

            span: current_span.as_ref().map(SerializableSpan),
            spans: current_span.as_ref().map(|_| SerializableContext(&ctx)),
        };

        let writer = self.0.make_writer();
        let _ = serde_json::to_writer(writer, &json);
    }

    /// When a new Span gets created, run the filter and start a new sentry span
    /// if it passes, setting it as the *current* sentry span.
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        let span = match ctx.span(id) {
            Some(span) => span,
            None => return,
        };

        if *span.metadata().level() > Level::INFO {
            return;
        }

        let (description, span_attrs) = extract_span_data(attrs);
        let op = span.name();

        // Spans don't always have a description, this ensures our data is not empty,
        // therefore the Sentry UI will be a lot more valuable for navigating spans.
        let description = description.unwrap_or_else(|| {
            let target = span.metadata().target();
            if target.is_empty() {
                op.to_string()
            } else {
                format!("{target}::{op}")
            }
        });

        let parent_sentry_span = sentry::configure_scope(|s| s.get_span());
        let sentry_span: sentry::TransactionOrSpan = match &parent_sentry_span {
            Some(parent) => parent.start_child(op, &description).into(),
            None => {
                let ctx = sentry::TransactionContext::new(&description, op);
                sentry::start_transaction(ctx).into()
            }
        };
        // Add the data from the original span to the sentry span.
        // This comes from typically the `fields` in `tracing::instrument`.
        for (key, value) in &span_attrs {
            if *key != "message" {
                sentry_span.set_data(key, value.clone());
            }
        }

        sentry::configure_scope(|scope| scope.set_span(Some(sentry_span.clone())));

        let mut extensions = span.extensions_mut();
        extensions.insert(SymbolicatorSpanData {
            span_attrs,
            sentry_span,
            parent_sentry_span,
        });
    }

    /// When a span gets closed, finish the underlying sentry span, and set back
    /// its parent as the *current* sentry span.
    fn on_close(&self, id: span::Id, ctx: Context<'_, S>) {
        let span = match ctx.span(&id) {
            Some(span) => span,
            None => return,
        };

        let mut extensions = span.extensions_mut();
        let SymbolicatorSpanData {
            sentry_span,
            parent_sentry_span,
            ..
        } = match extensions.remove::<SymbolicatorSpanData>() {
            Some(data) => data,
            None => return,
        };

        sentry_span.finish();
        sentry::configure_scope(|scope| scope.set_span(parent_sentry_span));
    }

    /// Implement the writing of extra data to span
    fn on_record(&self, span: &span::Id, attrs: &span::Record<'_>, ctx: Context<'_, S>) {
        let span = match ctx.span(span) {
            Some(s) => s,
            _ => return,
        };

        let mut extensions = span.extensions_mut();
        let Some(data) = extensions.get_mut::<SymbolicatorSpanData>() else {
            return;
        };

        let json_values = VISITOR_BUFFER.with_borrow_mut(|buf| {
            let mut visitor = FieldVisitor::with_buf(buf);
            attrs.record(&mut visitor);
            visitor.json_values
        });

        for (key, value) in &json_values {
            data.sentry_span.set_data(key, value.clone());
        }
        data.span_attrs.extend(json_values);
    }
}

/// Extracts the message and metadata from a span
fn extract_span_data(attrs: &span::Attributes) -> (Option<String>, BTreeMap<&'static str, Value>) {
    let json_values = VISITOR_BUFFER.with_borrow_mut(|buf| {
        let mut visitor = FieldVisitor::with_buf(buf);
        attrs.record(&mut visitor);
        visitor.json_values
    });

    // Find message of the span, if any
    let message = json_values
        .get("message")
        .and_then(|v| v.as_str().map(|s| s.to_owned()));

    (message, json_values)
}

thread_local! {
    static VISITOR_BUFFER: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Records all fields of [`tracing_core::Event`] for easy access
struct FieldVisitor<'s> {
    buf: &'s mut String,
    json_values: BTreeMap<&'static str, Value>,
    exceptions: Vec<Exception>,
}

impl<'s> FieldVisitor<'s> {
    fn with_buf(buf: &'s mut String) -> Self {
        Self {
            buf,
            json_values: BTreeMap::new(),
            exceptions: vec![],
        }
    }
    fn record<T: Into<Value>>(&mut self, field: &Field, value: T) {
        self.json_values.insert(field.name(), value.into());
    }
}

impl Visit for FieldVisitor<'_> {
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.record(field, value);
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.record(field, value);
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.record(field, value);
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.record(field, value);
    }

    fn record_error(&mut self, _field: &Field, value: &(dyn Error + 'static)) {
        let event = event_from_error(value);
        for exception in event.exception {
            self.exceptions.push(exception);
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write;
        write!(self.buf, "{value:?}").unwrap();
        self.json_values
            .insert(field.name(), self.buf.as_str().into());
        self.buf.clear();
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex, MutexGuard};

    use tracing_subscriber::fmt::time::UtcTime;
    use tracing_subscriber::Registry;

    use super::*;

    pub(crate) struct MockWriter {
        buf: Arc<Mutex<Vec<u8>>>,
    }

    impl MockWriter {
        pub(crate) fn new(buf: Arc<Mutex<Vec<u8>>>) -> Self {
            Self { buf }
        }

        pub(crate) fn buf(&self) -> MutexGuard<'_, Vec<u8>> {
            self.buf.lock().unwrap()
        }
    }

    impl io::Write for MockWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.buf().write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.buf().flush()
        }
    }

    #[derive(Clone, Default)]
    pub(crate) struct MockMakeWriter {
        buf: Arc<Mutex<Vec<u8>>>,
    }

    impl MockMakeWriter {
        pub(crate) fn new(buf: Arc<Mutex<Vec<u8>>>) -> Self {
            Self { buf }
        }

        pub(crate) fn get_string(&self) -> String {
            let mut buf = self.buf.lock().expect("lock shouldn't be poisoned");
            let string = std::str::from_utf8(&buf[..])
                .expect("formatter should not have produced invalid utf-8")
                .to_owned();
            buf.clear();
            string
        }
    }

    impl<'a> MakeWriter<'a> for MockMakeWriter {
        type Writer = MockWriter;

        fn make_writer(&'a self) -> Self::Writer {
            MockWriter::new(self.buf.clone())
        }
    }

    fn event_without_timestamp(writer: &MockMakeWriter) -> Value {
        let mut value: Value = serde_json::from_str(&writer.get_string()).unwrap();
        if let Some(obj) = value.as_object_mut() {
            obj.remove("timestamp");
        }
        value
    }

    #[test]
    fn custom_layer_is_same_as_fmt() {
        let fmt_writer = MockMakeWriter::new(Default::default());
        let sym_writer = MockMakeWriter::new(Default::default());

        let fmt = tracing_subscriber::fmt::layer()
            .with_timer(UtcTime::rfc_3339())
            .with_target(true)
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_span_list(true)
            .with_file(true)
            .with_line_number(true)
            .with_writer(fmt_writer.clone());

        let subscriber = Registry::default()
            .with(fmt)
            .with(SymbolicatorLayer(sym_writer.clone()));

        {
            let _guard = subscriber.set_default();
            let _span = tracing::info_span!("some span").entered();
            let _child_span = tracing::info_span!("another span", with = ?"data").entered();
            tracing::error!("lets log some shit!");
        }

        let fmt = event_without_timestamp(&fmt_writer);
        let sym = event_without_timestamp(&sym_writer);

        dbg!(&fmt, &sym);
        assert_eq!(fmt, sym);
    }
}
