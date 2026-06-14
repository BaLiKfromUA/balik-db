use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::error::Error;

const MAX_IDENT_LEN: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum ColumnType {
    Int,
    Text,
}

impl ColumnType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Int => "INT",
            Self::Text => "TEXT",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: ColumnType,
    #[serde(default)]
    pub nullable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schema {
    pub columns: Vec<Column>,
}

impl Schema {
    pub fn validate(&self, table_name: &str) -> Result<(), Error> {
        tracing::debug!(table = %table_name, columns = self.columns.len(), "validating schema");
        validate_identifier("table name", table_name)?;
        if self.columns.is_empty() {
            return Err(Error::invalid_schema(
                "schema must declare at least one column",
            ));
        }
        let mut seen = HashSet::new();
        for col in &self.columns {
            validate_identifier("column name", &col.name)?;
            let lower = col.name.to_ascii_lowercase();
            if !seen.insert(lower) {
                return Err(Error::invalid_schema(format!(
                    "duplicate column name '{}'",
                    col.name
                )));
            }
        }
        Ok(())
    }
}

fn validate_identifier(kind: &str, s: &str) -> Result<(), Error> {
    if s.is_empty() {
        return Err(Error::invalid_schema(format!("{kind} cannot be empty")));
    }
    if s.len() > MAX_IDENT_LEN {
        return Err(Error::invalid_schema(format!(
            "{kind} '{s}' exceeds {MAX_IDENT_LEN} characters"
        )));
    }
    let mut chars = s.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(Error::invalid_schema(format!(
            "{kind} '{s}' must start with a letter or underscore"
        )));
    }
    for c in chars {
        if !(c.is_ascii_alphanumeric() || c == '_') {
            return Err(Error::invalid_schema(format!(
                "{kind} '{s}' contains invalid character '{c}'"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: ColumnType, nullable: bool) -> Column {
        Column {
            name: name.to_string(),
            ty,
            nullable,
        }
    }

    #[test]
    fn column_type_round_trips_through_toml() {
        let schema = Schema {
            columns: vec![
                col("id", ColumnType::Int, false),
                col("name", ColumnType::Text, true),
            ],
        };
        let s = toml::to_string(&schema).unwrap();
        assert!(s.contains("type = \"INT\""));
        assert!(s.contains("type = \"TEXT\""));
        let back: Schema = toml::from_str(&s).unwrap();
        assert_eq!(back, schema);
    }

    #[test]
    fn validate_accepts_simple_schema() {
        let schema = Schema {
            columns: vec![
                col("id", ColumnType::Int, false),
                col("name", ColumnType::Text, true),
            ],
        };
        schema.validate("users").unwrap();
    }

    #[test]
    fn validate_rejects_empty_table_name() {
        let schema = Schema {
            columns: vec![col("id", ColumnType::Int, false)],
        };
        let err = schema.validate("").unwrap_err();
        assert!(err.to_string().contains("table name cannot be empty"));
    }

    #[test]
    fn validate_rejects_table_name_starting_with_digit() {
        let schema = Schema {
            columns: vec![col("id", ColumnType::Int, false)],
        };
        let err = schema.validate("1users").unwrap_err();
        assert!(err.to_string().contains("must start with a letter"));
    }

    #[test]
    fn validate_rejects_table_name_with_invalid_chars() {
        let schema = Schema {
            columns: vec![col("id", ColumnType::Int, false)],
        };
        let err = schema.validate("user-table").unwrap_err();
        assert!(err.to_string().contains("invalid character"));
    }

    #[test]
    fn validate_rejects_zero_columns() {
        let schema = Schema { columns: vec![] };
        let err = schema.validate("users").unwrap_err();
        assert!(err.to_string().contains("at least one column"));
    }

    #[test]
    fn validate_rejects_empty_column_name() {
        let schema = Schema {
            columns: vec![col("", ColumnType::Int, false)],
        };
        let err = schema.validate("users").unwrap_err();
        assert!(err.to_string().contains("column name cannot be empty"));
    }

    #[test]
    fn validate_rejects_duplicate_columns_case_insensitive() {
        let schema = Schema {
            columns: vec![
                col("Name", ColumnType::Text, false),
                col("name", ColumnType::Text, false),
            ],
        };
        let err = schema.validate("users").unwrap_err();
        assert!(err.to_string().contains("duplicate column name"));
    }

    #[test]
    fn validate_rejects_overlong_identifiers() {
        let long = "a".repeat(MAX_IDENT_LEN + 1);
        let schema = Schema {
            columns: vec![col("id", ColumnType::Int, false)],
        };
        let err = schema.validate(&long).unwrap_err();
        assert!(err.to_string().contains("exceeds"));
    }
}
