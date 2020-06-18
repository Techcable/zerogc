#[derive(Clone)]
pub enum ThreadId {
    Nop,
    Enabled {
        id: std::thread::ThreadId,
        name: Option<String>
    }
}
impl ThreadId {
    pub fn current() -> ThreadId {
        let thread = std::thread::current();
        ThreadId::Enabled {
            id: thread.id(),
            name: thread.name().map(String::from)
        }
    }
}
impl slog::Value for ThreadId {
    fn serialize(
        &self, _record: &slog::Record,
        key: &'static str,
        serializer: &mut dyn slog::Serializer
    ) -> slog::Result<()> {
        let (id, name) = match *self {
            ThreadId::Nop => return Ok(()),
            ThreadId::Enabled { ref id, ref name } => (id, name)
        };
        match *name {
            Some(ref name) => {
                serializer.emit_arguments(key, &format_args!(
                    "{}: {:?}", *name, id
                ))
            },
            None => {
                serializer.emit_arguments(key, &format_args!(
                    "{:?}", id
                ))
            },
        }
    }
}