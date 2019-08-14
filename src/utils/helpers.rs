/// Execute a callback on dropping of the container type.
pub struct CallOnDrop {
    f: Option<Box<dyn FnOnce() + 'static>>,
}

impl CallOnDrop {
    pub fn new<F: FnOnce() + 'static>(f: F) -> CallOnDrop {
        CallOnDrop {
            f: Some(Box::new(f)),
        }
    }
}

impl Drop for CallOnDrop {
    fn drop(&mut self) {
        (self.f.take().unwrap())();
    }
}
