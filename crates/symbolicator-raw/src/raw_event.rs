use serde_json::value::{Map, Value};

#[derive(Debug)]
pub struct RawEvent {
    raw_event: Value,
}

pub type Object = Map<String, Value>;

impl RawEvent {
    pub fn new(raw_event: Value) -> Self {
        Self { raw_event }
    }

    pub fn pointer(&self, pointer: &str) -> Option<&Value> {
        self.raw_event.pointer(pointer)
    }

    pub fn debug_images(&mut self) -> Option<impl Iterator<Item = &mut Object>> {
        let images = self.raw_event.pointer_mut("/debug_meta/images")?;
        let images = images.as_array_mut()?;
        Some(images.iter_mut().filter_map(Value::as_object_mut))
    }

    pub fn for_each_stack_traces<F>(&mut self, mut f: F)
    where
        F: FnMut(StacktraceInfo),
    {
        // "exception", "values", ... "stacktrace"
        if let Some(exception_values) = self
            .raw_event
            .pointer_mut("/exception/values")
            .and_then(Value::as_array_mut)
        {
            for exc in exception_values {
                if let Some(sinfo) = StacktraceInfo::from_value(exc) {
                    f(sinfo);
                }
            }
        }

        // "stacktrace"
        if let Some(sinfo) = StacktraceInfo::from_value(&mut self.raw_event) {
            f(sinfo);
        }

        // "threads", "values", ... "stacktrace"
        if let Some(thread_values) = self
            .raw_event
            .pointer_mut("/threads/values")
            .and_then(Value::as_array_mut)
        {
            for exc in thread_values {
                if let Some(sinfo) = StacktraceInfo::from_value(exc) {
                    f(sinfo);
                }
            }
        }
    }
}

pub struct StacktraceInfo<'c> {
    container: &'c mut Value,
}

impl<'c> StacktraceInfo<'c> {
    fn from_value(container: &'c mut Value) -> Option<Self> {
        let _frames = container
            .pointer_mut("/stacktrace/frames")
            .and_then(Value::as_array_mut)?;

        Some(Self { container })
    }

    pub fn frames(&mut self) -> Option<&mut Vec<Value>> {
        self.container
            .pointer_mut("/stacktrace/frames")
            .and_then(Value::as_array_mut)
        //.expect("`StacktraceInfo` should have frames")
    }

    pub fn container(&mut self) -> Option<&mut Object> {
        self.container.as_object_mut()
        //.expect("`StacktraceInfo` should contain an object")
    }
}
