//! Stopgap text<->value helpers for the CLI data-plane commands.
//!
//! `row-insert` / `row-update` need to turn a `--values` string into a typed
//! `Record`, and `row-get` / `table-scan` need to print records back. This is
//! a deliberately small, comma-delimited convenience layer — it is expected to
//! be retired wholesale once the SQL parser can produce values directly.
//!
//! This layer only handles *conversion*: token count (so input is never
//! silently truncated) and turning each token into a typed `Value`. Semantic
//! checks — nullability and type compatibility — are the storage layer's job
//! (`column_store::validate_record` runs on every insert), so they are not
//! repeated here.

use crate::catalog::schema::{ColumnType, Schema};
use crate::error::Error;
use crate::storage::{Record, Value};

/// Parse a CLI `--values "1,Alice,NULL"` row into a `Record`, mapping tokens
/// positionally onto `schema.columns`. The literal `NULL` (case-insensitive)
/// yields `Value::Null` and is rejected for NOT NULL columns. Tokens are split
/// on commas, so TEXT values cannot contain a comma through this interface.
pub fn parse_values(schema: &Schema, s: &str) -> Result<Record, Error> {
    let tokens: Vec<&str> = s.split(',').collect();
    if tokens.len() != schema.columns.len() {
        return Err(Error(format!(
            "expected {} value(s) to match the schema, got {}",
            schema.columns.len(),
            tokens.len()
        )));
    }
    let mut values = Vec::with_capacity(tokens.len());
    for (token, col) in tokens.iter().zip(&schema.columns) {
        let t = token.trim();
        // NULL is accepted here regardless of nullability; the storage layer
        // rejects it on NOT NULL columns at insert time.
        if t.eq_ignore_ascii_case("NULL") {
            values.push(Value::Null);
            continue;
        }
        let value = match col.ty {
            ColumnType::Int => Value::Int(t.parse::<i64>().map_err(|_| {
                Error(format!("column '{}' expects an INT, got '{t}'", col.name))
            })?),
            ColumnType::Text => Value::Text(t.to_string()),
        };
        values.push(value);
    }
    Ok(Record { values })
}

/// Render one value for human-readable CLI output. NULL prints as `NULL`.
pub fn render_value(value: &Value) -> String {
    match value {
        Value::Int(n) => n.to_string(),
        Value::Text(s) => s.clone(),
        Value::Null => "NULL".to_string(),
    }
}

/// Render a record as `col=value, col=value` in the schema's column order.
pub fn render_record(schema: &Schema, record: &Record) -> String {
    schema
        .columns
        .iter()
        .zip(&record.values)
        .map(|(col, value)| format!("{}={}", col.name, render_value(value)))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::schema::Column;

    fn schema() -> Schema {
        Schema {
            columns: vec![
                Column {
                    name: "id".to_string(),
                    ty: ColumnType::Int,
                    nullable: false,
                },
                Column {
                    name: "name".to_string(),
                    ty: ColumnType::Text,
                    nullable: true,
                },
            ],
        }
    }

    #[test]
    fn parses_ints_text_and_null() {
        let r = parse_values(&schema(), "1, alice").unwrap();
        assert_eq!(r.values, vec![Value::Int(1), Value::Text("alice".to_string())]);
        let r = parse_values(&schema(), "2,NULL").unwrap();
        assert_eq!(r.values, vec![Value::Int(2), Value::Null]);
    }

    #[test]
    fn rejects_wrong_arity() {
        let err = parse_values(&schema(), "1").unwrap_err();
        assert!(err.to_string().contains("expected 2 value"));
    }

    #[test]
    fn rejects_non_int_for_int_column() {
        let err = parse_values(&schema(), "x,alice").unwrap_err();
        assert!(err.to_string().contains("expects an INT"));
    }

    #[test]
    fn null_literal_parses_to_null_regardless_of_nullability() {
        // Nullability is enforced by the storage layer at insert time, not here:
        // "NULL" parses to Value::Null even for the NOT NULL `id` column.
        let r = parse_values(&schema(), "NULL,alice").unwrap();
        assert_eq!(r.values, vec![Value::Null, Value::Text("alice".to_string())]);
    }

    #[test]
    fn renders_record_in_column_order() {
        let r = parse_values(&schema(), "7,zed").unwrap();
        assert_eq!(render_record(&schema(), &r), "id=7, name=zed");
        let r = parse_values(&schema(), "7,NULL").unwrap();
        assert_eq!(render_record(&schema(), &r), "id=7, name=NULL");
    }
}
