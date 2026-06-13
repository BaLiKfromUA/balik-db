//! The result of executing a physical plan, and the glue that turns the
//! executor's batch stream into one.
//!
//! A SELECT yields [`QueryResult::Rows`]; a CREATE TABLE or INSERT yields
//! [`QueryResult::Affected`] with a human-readable line. `Display` renders rows
//! as a simple aligned text table so the CLI can print them directly.

use std::fmt;

use crate::error::Error;
use crate::storage::Value;

use super::BatchStream;

/// What a single executed statement produces.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryResult {
    /// Rows from a SELECT: `names` are the output columns in order, and each
    /// entry of `rows` holds one row's values in that same order.
    Rows {
        names: Vec<String>,
        rows: Vec<Vec<Value>>,
    },
    /// A statement that changed catalog or data rather than returning rows.
    Affected(String),
}

/// Drain `stream` — column-major batches in `names` order — into a row-major
/// [`QueryResult::Rows`]. The batches are already deleted-row-compacted and
/// projected by the operators upstream, so this only transposes them.
pub fn collect_rows(names: Vec<String>, stream: BatchStream<'_>) -> Result<QueryResult, Error> {
    let mut rows = Vec::new();
    for batch in stream {
        let batch = batch?;
        for r in 0..batch.num_rows() {
            rows.push(batch.columns.iter().map(|col| col[r].clone()).collect());
        }
    }
    Ok(QueryResult::Rows { names, rows })
}

impl fmt::Display for QueryResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueryResult::Affected(msg) => write!(f, "{msg}"),
            QueryResult::Rows { names, rows } => {
                let rendered: Vec<Vec<String>> = rows
                    .iter()
                    .map(|row| row.iter().map(render_value).collect())
                    .collect();

                // Column width = the widest of the header and any cell below it.
                let mut widths: Vec<usize> = names.iter().map(String::len).collect();
                for row in &rendered {
                    for (i, cell) in row.iter().enumerate() {
                        if let Some(w) = widths.get_mut(i) {
                            *w = (*w).max(cell.len());
                        }
                    }
                }

                let mut lines = Vec::with_capacity(rendered.len() + 2);
                lines.push(pad_join(names.iter().map(String::as_str), &widths, " | "));
                lines.push(
                    widths
                        .iter()
                        .map(|w| "-".repeat(*w))
                        .collect::<Vec<_>>()
                        .join("-+-"),
                );
                for row in &rendered {
                    lines.push(pad_join(row.iter().map(String::as_str), &widths, " | "));
                }
                write!(f, "{}", lines.join("\n"))
            }
        }
    }
}

/// Join `cells` left-padded to `widths`, separated by `sep`.
fn pad_join<'a>(cells: impl Iterator<Item = &'a str>, widths: &[usize], sep: &str) -> String {
    cells
        .enumerate()
        .map(|(i, c)| format!("{:width$}", c, width = widths.get(i).copied().unwrap_or(0)))
        .collect::<Vec<_>>()
        .join(sep)
}

fn render_value(v: &Value) -> String {
    match v {
        Value::Int(n) => n.to_string(),
        Value::Text(s) => s.clone(),
        Value::Null => "NULL".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::ColumnBatch;

    fn stream_of(batches: Vec<ColumnBatch>) -> BatchStream<'static> {
        Box::new(batches.into_iter().map(Ok))
    }

    #[test]
    fn collect_rows_transposes_column_major_batches() {
        let batch = ColumnBatch {
            names: vec!["id".into(), "name".into()],
            columns: vec![
                vec![Value::Int(1), Value::Int(2)],
                vec![Value::Text("a".into()), Value::Text("b".into())],
            ],
        };
        let result =
            collect_rows(vec!["id".into(), "name".into()], stream_of(vec![batch])).unwrap();
        assert_eq!(
            result,
            QueryResult::Rows {
                names: vec!["id".into(), "name".into()],
                rows: vec![
                    vec![Value::Int(1), Value::Text("a".into())],
                    vec![Value::Int(2), Value::Text("b".into())],
                ],
            }
        );
    }

    #[test]
    fn collect_rows_concatenates_across_batches() {
        let b1 = ColumnBatch {
            names: vec!["id".into()],
            columns: vec![vec![Value::Int(1)]],
        };
        let b2 = ColumnBatch {
            names: vec!["id".into()],
            columns: vec![vec![Value::Int(2), Value::Int(3)]],
        };
        let result = collect_rows(vec!["id".into()], stream_of(vec![b1, b2])).unwrap();
        let QueryResult::Rows { rows, .. } = result else {
            panic!("expected rows");
        };
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn display_renders_aligned_table() {
        let result = QueryResult::Rows {
            names: vec!["id".into(), "name".into()],
            rows: vec![
                vec![Value::Int(1), Value::Text("Alice".into())],
                vec![Value::Int(20), Value::Null],
            ],
        };
        assert_eq!(
            result.to_string(),
            "id | name \n---+------\n1  | Alice\n20 | NULL "
        );
    }

    #[test]
    fn display_of_empty_rows_keeps_header_only() {
        let result = QueryResult::Rows {
            names: vec!["id".into(), "name".into()],
            rows: vec![],
        };
        assert_eq!(result.to_string(), "id | name\n---+-----");
    }

    #[test]
    fn display_of_affected_is_the_message() {
        let result = QueryResult::Affected("Inserted 1 row into 'users'".into());
        assert_eq!(result.to_string(), "Inserted 1 row into 'users'");
    }
}
