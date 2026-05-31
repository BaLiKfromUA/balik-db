use crate::parser::{self, ParseError};

/// Parse a single SQL query and print its AST. The AST is rendered with the
/// pretty `Debug` formatter, which gives an indented, easy-to-scan tree.
///
/// On a parse error the `Err` is propagated; `main` prints it to stderr and
/// exits non-zero. This command never touches storage.
pub fn run(query: &str) -> Result<(), ParseError> {
    let statement = parser::parse(query)?;
    println!("{statement:#?}");
    Ok(())
}
