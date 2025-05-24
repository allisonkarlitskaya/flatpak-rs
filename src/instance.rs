use std::process;

#[derive(Debug)]
pub(crate) struct Instance {
    id: String,
}

impl Instance {
    /// Create an instance ID based on the PID of the caller.
    /// TODO: we probably want something better...
    pub(crate) fn new_pid() -> Self {
        Self {
            id: format!("{}", process::id()),
        }
    }

    pub(crate) fn get_id(&self) -> &str {
        &self.id
    }
}
