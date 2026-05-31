//! Error type for the parser front end.
//!
//! Kept independent of the storage-layer `crate::error::Error`: the parser is a
//! self-contained subsystem that turns text into an AST and must not depend on
//! storage or the catalog.

use std::fmt;

/// A failure to turn a SQL string into an internal AST.
///
/// Carries a human-readable description and, when known, an approximate source
/// location. Syntax errors surfaced by the underlying SQL grammar embed a
/// `Line/Column` hint in the message itself; structural rejections raised
/// during lowering (an empty column list, a missing VALUES list) can attach the
/// nearest identifier via [`ParseError::near`] so the reader still gets a
/// location anchor. Either way the message says what was expected instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    message: String,
    /// The token or name the error sits nearest to, when the grammar did not
    /// already pin down a `Line/Column`. Rendered as `(near \`…\`)`.
    near: Option<String>,
}

impl ParseError {
    pub fn new(message: impl Into<String>) -> Self {
        ParseError {
            message: message.into(),
            near: None,
        }
    }

    /// A construct that parses as valid SQL but falls outside the supported
    /// subset (JOIN, GROUP BY, multi-row INSERT, ...). Phrased so the reader
    /// learns which feature is missing rather than just "syntax error".
    pub fn unsupported(what: impl fmt::Display) -> Self {
        ParseError::new(format!("unsupported: {what}"))
    }

    /// Attach an approximate location — the name or token the error is nearest
    /// to — for structural errors the grammar reports without a position.
    pub fn near(mut self, token: impl fmt::Display) -> Self {
        self.near = Some(token.to_string());
        self
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)?;
        if let Some(near) = &self.near {
            write!(f, " (near `{near}`)")?;
        }
        Ok(())
    }
}

impl std::error::Error for ParseError {}
