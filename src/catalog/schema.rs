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

    pub fn parse(s: &str) -> Result<Self, Error> {
        match s.trim().to_ascii_uppercase().as_str() {
            "INT" => Ok(Self::Int),
            "TEXT" => Ok(Self::Text),
            other => Err(Error::invalid_schema(format!(
                "unsupported column type '{other}'"
            ))),
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

    /// Parse the CLI `--columns "id:INT,name:TEXT"` mini-DSL.
    /// Whitespace around tokens is ignored. Trailing commas and empty entries
    /// are rejected.
    pub fn parse_columns_dsl(s: &str) -> Result<Self, Error> {
        tracing::debug!(input = %s, "parsing columns DSL");
        if s.trim().is_empty() {
            return Err(Error::invalid_schema("column list is empty"));
        }
        let mut columns = Vec::new();
        for part in s.split(',') {
            let trimmed = part.trim();
            if trimmed.is_empty() {
                return Err(Error::invalid_schema(
                    "empty column entry (extra or trailing comma?)",
                ));
            }
            let (name, ty_str) = trimmed.split_once(':').ok_or_else(|| {
                Error::invalid_schema(format!(
                    "column '{trimmed}' is missing ':TYPE' (expected 'name:TYPE')"
                ))
            })?;
            let name = name.trim();
            if name.is_empty() {
                return Err(Error::invalid_schema(format!(
                    "column entry '{trimmed}' has no name"
                )));
            }
            let ty = ColumnType::parse(ty_str)?;
            columns.push(Column {
                name: name.to_string(),
                ty,
                nullable: false,
            });
        }
        tracing::debug!(parsed_columns = columns.len(), "columns DSL parsed");
        Ok(Schema { columns })
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
    fn column_type_parse_accepts_known_types_case_insensitively() {
        assert_eq!(ColumnType::parse("INT").unwrap(), ColumnType::Int);
        assert_eq!(ColumnType::parse("int").unwrap(), ColumnType::Int);
        assert_eq!(ColumnType::parse("  Int  ").unwrap(), ColumnType::Int);
        assert_eq!(ColumnType::parse("TEXT").unwrap(), ColumnType::Text);
        assert_eq!(ColumnType::parse("text").unwrap(), ColumnType::Text);
    }

    #[test]
    fn column_type_parse_rejects_unknown_types() {
        let err = ColumnType::parse("FLOAT").unwrap_err();
        assert!(err.to_string().contains("unsupported column type"));
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

    #[test]
    fn dsl_parses_simple_pair() {
        let schema = Schema::parse_columns_dsl("id:INT,name:TEXT").unwrap();
        assert_eq!(
            schema.columns,
            vec![
                col("id", ColumnType::Int, false),
                col("name", ColumnType::Text, false),
            ]
        );
    }

    #[test]
    fn dsl_tolerates_whitespace() {
        let schema = Schema::parse_columns_dsl("  id : INT ,  name : TEXT ").unwrap();
        assert_eq!(schema.columns.len(), 2);
        assert_eq!(schema.columns[0].name, "id");
        assert_eq!(schema.columns[1].name, "name");
    }

    #[test]
    fn dsl_rejects_empty_input() {
        let err = Schema::parse_columns_dsl("").unwrap_err();
        assert!(err.to_string().contains("empty"));
        let err = Schema::parse_columns_dsl("   ").unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn dsl_rejects_trailing_comma() {
        let err = Schema::parse_columns_dsl("id:INT,").unwrap_err();
        assert!(err.to_string().contains("empty column entry"));
    }

    #[test]
    fn dsl_rejects_missing_type() {
        let err = Schema::parse_columns_dsl("id").unwrap_err();
        assert!(err.to_string().contains("missing ':TYPE'"));
    }

    #[test]
    fn dsl_rejects_unknown_type() {
        let err = Schema::parse_columns_dsl("id:BLOB").unwrap_err();
        assert!(err.to_string().contains("unsupported column type"));
    }
}
