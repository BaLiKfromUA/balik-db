use std::fmt;
use std::io;

#[derive(Debug)]
pub struct Error(pub String);

impl Error {
    pub fn invalid_schema(msg: impl Into<String>) -> Self {
        Error(format!("invalid schema: {}", msg.into()))
    }

    pub fn table_exists(name: &str) -> Self {
        Error(format!("table '{name}' already exists"))
    }

    pub fn no_such_table(name: &str) -> Self {
        Error(format!("no such table '{name}'"))
    }

    pub fn io(ctx: &str, e: io::Error) -> Self {
        Error(format!("{ctx}: {e}"))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error(e.to_string())
    }
}
