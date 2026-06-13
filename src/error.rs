use std::fmt;
use std::io;

/// All errors surfaced by the storage engine and CLI.
///
/// Each variant carries a human-readable message that `Display` renders. The
/// variants let callers — the query parser and executor especially — match on
/// the kind of failure instead of scraping strings.
#[derive(Debug)]
pub enum Error {
    /// No table with this name exists.
    TableNotFound(String),
    /// A table with this name already exists.
    TableExists(String),
    /// A schema or identifier failed validation at definition time.
    InvalidSchema(String),
    /// A record or value did not match its column's type, nullability, or the
    /// table's arity.
    InvalidValue(String),
    /// A query is syntactically valid but logically ill-formed: it references a
    /// column that does not exist, supplies the wrong number of values, or
    /// otherwise fails binding against the catalog.
    InvalidQuery(String),
    /// A stored CRC did not match a file's contents.
    ChecksumMismatch(String),
    /// An on-disk file is structurally invalid: bad magic, truncation, an
    /// unsupported version or encoding, or an unparseable body.
    Corrupt(String),
    /// An I/O operation failed, with context describing what was attempted.
    Io { context: String, source: io::Error },
    /// Anything that does not fit the cases above.
    Other(String),
}

impl Error {
    pub fn no_such_table(name: &str) -> Self {
        Error::TableNotFound(name.to_string())
    }

    pub fn table_exists(name: &str) -> Self {
        Error::TableExists(name.to_string())
    }

    pub fn invalid_schema(msg: impl Into<String>) -> Self {
        Error::InvalidSchema(msg.into())
    }

    pub fn invalid_value(msg: impl Into<String>) -> Self {
        Error::InvalidValue(msg.into())
    }

    pub fn invalid_query(msg: impl Into<String>) -> Self {
        Error::InvalidQuery(msg.into())
    }

    pub fn checksum_mismatch(msg: impl Into<String>) -> Self {
        Error::ChecksumMismatch(msg.into())
    }

    pub fn corrupt(msg: impl Into<String>) -> Self {
        Error::Corrupt(msg.into())
    }

    pub fn io(context: &str, source: io::Error) -> Self {
        Error::Io {
            context: context.to_string(),
            source,
        }
    }

    pub fn other(msg: impl Into<String>) -> Self {
        Error::Other(msg.into())
    }

    /// Prefix the message with `ctx` (e.g. a filename) while keeping the
    /// variant, so wrapping a lower-layer error preserves its kind:
    /// `verify(&bytes).map_err(|e| e.context("catalog.toml"))`.
    pub fn context(self, ctx: &str) -> Self {
        match self {
            Error::InvalidSchema(m) => Error::InvalidSchema(format!("{ctx}: {m}")),
            Error::InvalidValue(m) => Error::InvalidValue(format!("{ctx}: {m}")),
            Error::InvalidQuery(m) => Error::InvalidQuery(format!("{ctx}: {m}")),
            Error::ChecksumMismatch(m) => Error::ChecksumMismatch(format!("{ctx}: {m}")),
            Error::Corrupt(m) => Error::Corrupt(format!("{ctx}: {m}")),
            Error::Other(m) => Error::Other(format!("{ctx}: {m}")),
            Error::Io { context, source } => Error::Io {
                context: format!("{ctx}: {context}"),
                source,
            },
            // Identity-bearing variants are never contextualized.
            other => other,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::TableNotFound(name) => write!(f, "no such table '{name}'"),
            Error::TableExists(name) => write!(f, "table '{name}' already exists"),
            Error::InvalidSchema(msg) => write!(f, "invalid schema: {msg}"),
            Error::InvalidValue(msg) => write!(f, "{msg}"),
            Error::InvalidQuery(msg) => write!(f, "{msg}"),
            Error::ChecksumMismatch(msg) => write!(f, "{msg}"),
            Error::Corrupt(msg) => write!(f, "{msg}"),
            Error::Io { context, source } => write!(f, "{context}: {source}"),
            Error::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io {
            context: "io error".to_string(),
            source: e,
        }
    }
}
