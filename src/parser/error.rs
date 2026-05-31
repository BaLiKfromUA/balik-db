//! Error type for the parser front end.
//!
//! Kept independent of the storage-layer `crate::error::Error`: the parser is a
//! self-contained subsystem that turns text into an AST and must not depend on
//! storage or the catalog.

use std::fmt;

/// A failure to turn a SQL string into an internal AST.
///
/// Carries a human-readable description and, when known, an approximate source
/// position. Syntax errors surfaced by the underlying SQL grammar usually embed
/// a `Line/Column` hint in the message itself; structural rejections (an empty
/// column list, an unsupported construct) describe the problem and what was
/// expected instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    message: String,
}

impl ParseError {
    pub fn new(message: impl Into<String>) -> Self {
        ParseError {
            message: message.into(),
        }
    }

    /// A construct that parses as valid SQL but falls outside the supported
    /// subset (JOIN, GROUP BY, multi-row INSERT, ...). Phrased so the reader
    /// learns which feature is missing rather than just "syntax error".
    pub fn unsupported(what: impl fmt::Display) -> Self {
        ParseError::new(format!("unsupported: {what}"))
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ParseError {}
