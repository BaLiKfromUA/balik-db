//! Binding and validation: turn a parsed `Statement` into a `LogicalPlan`,
//! checking it against the catalog along the way.
//!
//! This is the only part of the planner that reads the catalog (via the
//! `Storage` trait's `describe_table`). It performs no data-plane operations —
//! the plan is logical, so values are never read or written here. Validation
//! covers what the spec asks for: that referenced tables and columns exist,
//! that an INSERT supplies the right number of well-typed values, and that a
//! CREATE TABLE is internally consistent.

use crate::catalog::schema::{Column, ColumnType, Schema};
use crate::error::Error;
use crate::parser::ast::{self, DataType, Expr, Literal, Projection, Statement};
use crate::storage::Storage;

use super::plan::LogicalPlan;

/// Bind `stmt` against `storage`'s catalog, producing a validated plan.
pub fn bind(stmt: &Statement, storage: &dyn Storage) -> Result<LogicalPlan, Error> {
    match stmt {
        Statement::CreateTable(ct) => bind_create_table(ct),
        Statement::Insert(ins) => bind_insert(ins, storage),
        Statement::Select(sel) => bind_select(sel, storage),
    }
}

fn bind_create_table(ct: &ast::CreateTable) -> Result<LogicalPlan, Error> {
    // Reuse the catalog's schema validation — identifier rules, non-empty
    // column list, no duplicate names — by building the schema this statement
    // would create and validating it. No catalog read is needed.
    let schema = Schema {
        columns: ct
            .columns
            .iter()
            .map(|c| Column {
                name: c.name.clone(),
                ty: data_type_to_column_type(c.ty),
                nullable: c.nullable,
            })
            .collect(),
    };
    schema.validate(&ct.table)?;
    Ok(LogicalPlan::CreateTable {
        table: ct.table.clone(),
        columns: ct.columns.clone(),
    })
}

fn bind_insert(ins: &ast::Insert, storage: &dyn Storage) -> Result<LogicalPlan, Error> {
    let desc = storage.describe_table(&ins.table)?;
    let columns = &desc.schema.columns;
    if ins.values.len() != columns.len() {
        return Err(Error::invalid_query(format!(
            "INSERT into '{}' expects {} value(s), got {}",
            ins.table,
            columns.len(),
            ins.values.len()
        )));
    }
    for (value, col) in ins.values.iter().zip(columns) {
        check_value(value, col, &ins.table)?;
    }
    Ok(LogicalPlan::Insert {
        table: ins.table.clone(),
        values: ins.values.clone(),
    })
}

/// Check one INSERT literal against the column it lands in: NULL only on
/// nullable columns, and matching scalar type otherwise.
fn check_value(value: &Literal, col: &Column, table: &str) -> Result<(), Error> {
    match (value, col.ty) {
        (Literal::Null, _) => {
            if !col.nullable {
                return Err(Error::invalid_query(format!(
                    "column '{}' of '{table}' is NOT NULL but got NULL",
                    col.name
                )));
            }
        }
        (Literal::Int(_), ColumnType::Int) | (Literal::Text(_), ColumnType::Text) => {}
        (lit, ty) => {
            return Err(Error::invalid_query(format!(
                "column '{}' of '{table}' expects {}, got {}",
                col.name,
                ty.as_str(),
                literal_type_name(lit)
            )));
        }
    }
    Ok(())
}

fn bind_select(sel: &ast::Select, storage: &dyn Storage) -> Result<LogicalPlan, Error> {
    let desc = storage.describe_table(&sel.from)?;
    let schema = &desc.schema;

    // Resolve the projection: `*` expands to the table's columns; an explicit
    // list must reference only columns that exist.
    let projection_columns = match &sel.projections {
        Projection::All => schema.columns.iter().map(|c| c.name.clone()).collect(),
        Projection::Columns(cols) => {
            for name in cols {
                ensure_column_exists(schema, name, &sel.from, "SELECT")?;
            }
            cols.clone()
        }
    };

    // Validate columns referenced by WHERE and ORDER BY against the table.
    if let Some(filter) = &sel.filter {
        let mut referenced = Vec::new();
        collect_columns(filter, &mut referenced);
        for name in &referenced {
            ensure_column_exists(schema, name, &sel.from, "WHERE")?;
        }
    }
    if let Some(order_by) = &sel.order_by {
        ensure_column_exists(schema, &order_by.column, &sel.from, "ORDER BY")?;
    }

    // Stack operators innermost → outermost: Scan → Filter → Projection →
    // Sort → Limit.
    let mut plan = LogicalPlan::Scan {
        table: sel.from.clone(),
    };
    if let Some(filter) = &sel.filter {
        plan = LogicalPlan::Filter {
            predicate: filter.clone(),
            input: Box::new(plan),
        };
    }
    plan = LogicalPlan::Projection {
        columns: projection_columns,
        input: Box::new(plan),
    };
    if let Some(order_by) = &sel.order_by {
        plan = LogicalPlan::Sort {
            column: order_by.column.clone(),
            descending: order_by.descending,
            input: Box::new(plan),
        };
    }
    if let Some(count) = sel.limit {
        plan = LogicalPlan::Limit {
            count,
            input: Box::new(plan),
        };
    }
    Ok(plan)
}

fn ensure_column_exists(
    schema: &Schema,
    name: &str,
    table: &str,
    clause: &str,
) -> Result<(), Error> {
    if schema.columns.iter().any(|c| c.name == name) {
        Ok(())
    } else {
        Err(Error::invalid_query(format!(
            "{clause} references unknown column '{name}' in table '{table}'"
        )))
    }
}

/// Collect every column reference in a WHERE expression, in left-to-right order.
fn collect_columns(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::Column(name) => out.push(name.clone()),
        Expr::Literal(_) => {}
        Expr::Compare { left, right, .. } | Expr::Logical { left, right, .. } => {
            collect_columns(left, out);
            collect_columns(right, out);
        }
    }
}

fn data_type_to_column_type(ty: DataType) -> ColumnType {
    match ty {
        DataType::Int => ColumnType::Int,
        DataType::Text => ColumnType::Text,
    }
}

fn literal_type_name(lit: &Literal) -> &'static str {
    match lit {
        Literal::Int(_) => "INT",
        Literal::Text(_) => "TEXT",
        Literal::Null => "NULL",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::metadata;
    use crate::parser;
    use crate::storage::Storage;
    use crate::storage::column_store::ColumnStore;
    use tempfile::TempDir;

    /// A column store with a `users(id INT NOT NULL, name TEXT, age INT)` table.
    fn seeded_store() -> (TempDir, ColumnStore) {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("db");
        metadata::initialize(&db).unwrap();
        let mut store = ColumnStore::open(&db).unwrap();
        let schema = Schema {
            columns: vec![
                Column {
                    name: "id".into(),
                    ty: ColumnType::Int,
                    nullable: false,
                },
                Column {
                    name: "name".into(),
                    ty: ColumnType::Text,
                    nullable: true,
                },
                Column {
                    name: "age".into(),
                    ty: ColumnType::Int,
                    nullable: true,
                },
            ],
        };
        store
            .create_table("users", schema, Default::default())
            .unwrap();
        (tmp, store)
    }

    fn plan_of(query: &str, store: &ColumnStore) -> Result<LogicalPlan, Error> {
        let stmt = parser::parse(query).unwrap();
        bind(&stmt, store)
    }

    #[test]
    fn create_table_builds_plan() {
        let (_tmp, store) = seeded_store();
        let plan = plan_of("CREATE TABLE t (a INT, b TEXT)", &store).unwrap();
        assert_eq!(
            plan.to_string(),
            "CreateTable t [a INT, b TEXT]",
            "columns default to nullable"
        );
    }

    #[test]
    fn create_table_rejects_duplicate_columns() {
        let (_tmp, store) = seeded_store();
        let err = plan_of("CREATE TABLE t (a INT, a TEXT)", &store).unwrap_err();
        assert!(err.to_string().contains("duplicate column"), "{err}");
    }

    #[test]
    fn insert_builds_plan() {
        let (_tmp, store) = seeded_store();
        let plan = plan_of("INSERT INTO users VALUES (1, 'Alice', 20)", &store).unwrap();
        assert_eq!(plan.to_string(), "Insert users [1, 'Alice', 20]");
    }

    #[test]
    fn insert_allows_null_in_nullable_column() {
        let (_tmp, store) = seeded_store();
        assert!(plan_of("INSERT INTO users VALUES (1, NULL, NULL)", &store).is_ok());
    }

    #[test]
    fn insert_rejects_unknown_table() {
        let (_tmp, store) = seeded_store();
        let err = plan_of("INSERT INTO ghosts VALUES (1)", &store).unwrap_err();
        assert!(err.to_string().contains("no such table"), "{err}");
    }

    #[test]
    fn insert_rejects_wrong_arity() {
        let (_tmp, store) = seeded_store();
        let err = plan_of("INSERT INTO users VALUES (1, 'Alice')", &store).unwrap_err();
        assert!(
            err.to_string().contains("expects 3 value(s), got 2"),
            "{err}"
        );
    }

    #[test]
    fn insert_rejects_null_in_not_null_column() {
        let (_tmp, store) = seeded_store();
        let err = plan_of("INSERT INTO users VALUES (NULL, 'Alice', 20)", &store).unwrap_err();
        assert!(
            err.to_string().contains("is NOT NULL but got NULL"),
            "{err}"
        );
    }

    #[test]
    fn insert_rejects_type_mismatch() {
        let (_tmp, store) = seeded_store();
        let err = plan_of("INSERT INTO users VALUES ('x', 'Alice', 20)", &store).unwrap_err();
        assert!(err.to_string().contains("expects INT, got TEXT"), "{err}");
    }

    #[test]
    fn select_star_expands_to_all_columns() {
        let (_tmp, store) = seeded_store();
        let plan = plan_of("SELECT * FROM users", &store).unwrap();
        assert_eq!(plan.to_string(), "Projection [id, name, age]\n  Scan users");
    }

    #[test]
    fn select_full_chain_nests_outermost_first() {
        let (_tmp, store) = seeded_store();
        let plan = plan_of(
            "SELECT id, name FROM users WHERE age > 18 ORDER BY name LIMIT 10",
            &store,
        )
        .unwrap();
        assert_eq!(
            plan.to_string(),
            "Limit 10\n  Sort [name]\n    Projection [id, name]\n      Filter [age > 18]\n        Scan users"
        );
    }

    #[test]
    fn select_rejects_unknown_table() {
        let (_tmp, store) = seeded_store();
        let err = plan_of("SELECT * FROM ghosts", &store).unwrap_err();
        assert!(err.to_string().contains("no such table"), "{err}");
    }

    #[test]
    fn select_rejects_unknown_projection_column() {
        let (_tmp, store) = seeded_store();
        let err = plan_of("SELECT nope FROM users", &store).unwrap_err();
        assert!(
            err.to_string()
                .contains("SELECT references unknown column 'nope'"),
            "{err}"
        );
    }

    #[test]
    fn select_rejects_unknown_where_column() {
        let (_tmp, store) = seeded_store();
        let err = plan_of("SELECT id FROM users WHERE nope = 1", &store).unwrap_err();
        assert!(
            err.to_string()
                .contains("WHERE references unknown column 'nope'"),
            "{err}"
        );
    }

    #[test]
    fn select_rejects_unknown_order_by_column() {
        let (_tmp, store) = seeded_store();
        let err = plan_of("SELECT id FROM users ORDER BY nope", &store).unwrap_err();
        assert!(
            err.to_string()
                .contains("ORDER BY references unknown column 'nope'"),
            "{err}"
        );
    }
}
