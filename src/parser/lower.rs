//! Conversion from the third-party SQL grammar's tree into our internal AST.
//!
//! All references to `sqlparser` types live here (and in `mod.rs`, which calls
//! the tokenizer). Everything the converter cannot represent in the supported
//! subset is rejected with a descriptive [`ParseError`] rather than silently
//! dropped, so a returned AST always means "fully understood".

use sqlparser::ast as sql;

use super::ast::*;
use super::error::ParseError;

type Result<T> = std::result::Result<T, ParseError>;

/// Lower one top-level statement.
pub fn lower_statement(stmt: sql::Statement) -> Result<Statement> {
    match stmt {
        sql::Statement::CreateTable(ct) => lower_create_table(ct).map(Statement::CreateTable),
        sql::Statement::Insert(ins) => lower_insert(ins).map(Statement::Insert),
        sql::Statement::Query(query) => lower_select(*query).map(Statement::Select),
        sql::Statement::Drop {
            object_type,
            if_exists,
            names,
            ..
        } => lower_drop_table(object_type, if_exists, names).map(Statement::DropTable),
        other => Err(ParseError::unsupported(format!(
            "only CREATE TABLE, INSERT, SELECT and DROP TABLE are supported, not `{}`",
            statement_keyword(&other)
        ))),
    }
}

// ---- DROP TABLE ------------------------------------------------------------

fn lower_drop_table(
    object_type: sql::ObjectType,
    if_exists: bool,
    names: Vec<sql::ObjectName>,
) -> Result<DropTable> {
    if object_type != sql::ObjectType::Table {
        return Err(ParseError::unsupported("DROP of anything but a TABLE"));
    }
    if if_exists {
        return Err(ParseError::unsupported("DROP TABLE IF EXISTS"));
    }
    let [name] = names.as_slice() else {
        return Err(ParseError::unsupported("dropping more than one table at once"));
    };
    let table = single_name(name)?;
    Ok(DropTable { table })
}

// ---- CREATE TABLE ----------------------------------------------------------

fn lower_create_table(ct: sql::CreateTable) -> Result<CreateTable> {
    if !ct.constraints.is_empty() {
        return Err(ParseError::unsupported(
            "table constraints (PRIMARY KEY, FOREIGN KEY, ...)",
        ));
    }
    let table = single_name(&ct.name)?;
    if ct.columns.is_empty() {
        return Err(
            ParseError::new("CREATE TABLE requires at least one column definition").near(&table),
        );
    }
    let mut columns = Vec::with_capacity(ct.columns.len());
    for col in ct.columns {
        let ty = lower_data_type(&col.data_type)?;
        let nullable = lower_nullability(&col.name, col.options)?;
        columns.push(ColumnDef {
            name: col.name.value,
            ty,
            nullable,
        });
    }
    Ok(CreateTable { table, columns })
}

/// Read `NULL` / `NOT NULL` from a column's options. Columns are nullable by
/// default (SQL convention); any other option (DEFAULT, UNIQUE, PRIMARY KEY,
/// ...) is outside the supported subset and rejected.
fn lower_nullability(name: &sql::Ident, options: Vec<sql::ColumnOptionDef>) -> Result<bool> {
    let mut nullable = true;
    for opt in options {
        match opt.option {
            sql::ColumnOption::Null => nullable = true,
            sql::ColumnOption::NotNull => nullable = false,
            other => {
                return Err(ParseError::unsupported(format!(
                    "column option `{other}` on `{name}` (only NULL / NOT NULL are supported)"
                )));
            }
        }
    }
    Ok(nullable)
}

fn lower_data_type(ty: &sql::DataType) -> Result<DataType> {
    use sql::DataType as D;
    match ty {
        D::Int(_) | D::Integer(_) => Ok(DataType::Int),
        D::Text | D::Varchar(_) | D::String(_) | D::Char(_) => Ok(DataType::Text),
        other => Err(ParseError::unsupported(format!(
            "column type `{other}` (supported: INT, TEXT)"
        ))),
    }
}

// ---- INSERT ----------------------------------------------------------------

fn lower_insert(ins: sql::Insert) -> Result<Insert> {
    let table = match &ins.table {
        sql::TableObject::TableName(name) => single_name(name)?,
        _ => return Err(ParseError::unsupported("this INSERT target")),
    };
    if !ins.columns.is_empty() {
        return Err(ParseError::unsupported(
            "INSERT with an explicit column list",
        ));
    }
    if !ins.assignments.is_empty() {
        return Err(ParseError::unsupported("INSERT ... SET"));
    }

    let source = ins
        .source
        .ok_or_else(|| ParseError::new("INSERT requires a VALUES clause").near(&table))?;
    let rows = match *source.body {
        sql::SetExpr::Values(values) => values.rows,
        _ => return Err(ParseError::unsupported("INSERT ... SELECT")),
    };
    match rows.len() {
        0 => return Err(ParseError::new("INSERT requires at least one value").near(&table)),
        1 => {}
        _ => return Err(ParseError::unsupported("multi-row INSERT")),
    }
    let row = rows.into_iter().next().unwrap().content;
    if row.is_empty() {
        return Err(ParseError::new("INSERT requires at least one value").near(&table));
    }
    let values = row
        .into_iter()
        .map(lower_literal)
        .collect::<Result<Vec<_>>>()?;
    Ok(Insert { table, values })
}

// ---- SELECT ----------------------------------------------------------------

fn lower_select(query: sql::Query) -> Result<Select> {
    if query.with.is_some() {
        return Err(ParseError::unsupported("WITH (common table expressions)"));
    }
    if query.fetch.is_some() {
        return Err(ParseError::unsupported("FETCH"));
    }

    let order_by = query.order_by.map(lower_order_by).transpose()?;
    let limit = lower_limit(query.limit_clause)?;

    let select = match *query.body {
        sql::SetExpr::Select(select) => *select,
        sql::SetExpr::SetOperation { .. } => {
            return Err(ParseError::unsupported(
                "set operations (UNION / EXCEPT / INTERSECT)",
            ));
        }
        sql::SetExpr::Query(_) => return Err(ParseError::unsupported("nested SELECT")),
        sql::SetExpr::Values(_) => return Err(ParseError::unsupported("bare VALUES")),
        _ => return Err(ParseError::unsupported("this query form")),
    };

    if select.distinct.is_some() {
        return Err(ParseError::unsupported("SELECT DISTINCT"));
    }
    if select.having.is_some() {
        return Err(ParseError::unsupported("HAVING"));
    }
    if !group_by_is_empty(&select.group_by) {
        return Err(ParseError::unsupported("GROUP BY"));
    }

    let from = lower_from(select.from)?;
    let projections = lower_projection(select.projection)?;
    let filter = select.selection.map(lower_expr).transpose()?;

    Ok(Select {
        projections,
        from,
        filter,
        order_by,
        limit,
    })
}

fn lower_from(from: Vec<sql::TableWithJoins>) -> Result<String> {
    let mut iter = from.into_iter();
    let first = iter
        .next()
        .ok_or_else(|| ParseError::new("SELECT requires a FROM clause"))?;
    if iter.next().is_some() {
        return Err(ParseError::unsupported("more than one table in FROM"));
    }
    if !first.joins.is_empty() {
        return Err(ParseError::unsupported("JOIN"));
    }
    match first.relation {
        sql::TableFactor::Table {
            name, args: None, ..
        } => single_name(&name),
        sql::TableFactor::Table { .. } => {
            Err(ParseError::unsupported("table-valued function in FROM"))
        }
        _ => Err(ParseError::unsupported("subquery or derived table in FROM")),
    }
}

fn lower_projection(items: Vec<sql::SelectItem>) -> Result<Projection> {
    if items.is_empty() {
        return Err(ParseError::new(
            "SELECT requires a projection list (column names or `*`)",
        ));
    }
    // `SELECT *` must stand alone.
    if items
        .iter()
        .any(|i| matches!(i, sql::SelectItem::Wildcard(_)))
    {
        if items.len() == 1 {
            return Ok(Projection::All);
        }
        return Err(ParseError::unsupported("`*` mixed with other projections"));
    }

    let mut columns = Vec::with_capacity(items.len());
    for item in items {
        match item {
            sql::SelectItem::UnnamedExpr(sql::Expr::Identifier(id)) => columns.push(id.value),
            sql::SelectItem::UnnamedExpr(_) => {
                return Err(ParseError::unsupported(
                    "expressions in the projection list (only column names are allowed)",
                ));
            }
            sql::SelectItem::ExprWithAlias { .. } | sql::SelectItem::ExprWithAliases { .. } => {
                return Err(ParseError::unsupported("column aliases (AS) in projection"));
            }
            sql::SelectItem::QualifiedWildcard(..) => {
                return Err(ParseError::unsupported("qualified wildcard (table.*)"));
            }
            sql::SelectItem::Wildcard(_) => unreachable!("handled above"),
        }
    }
    Ok(Projection::Columns(columns))
}

fn lower_order_by(order_by: sql::OrderBy) -> Result<OrderBy> {
    if order_by.interpolate.is_some() {
        return Err(ParseError::unsupported("ORDER BY ... INTERPOLATE"));
    }
    let exprs = match order_by.kind {
        sql::OrderByKind::Expressions(exprs) => exprs,
        sql::OrderByKind::All(_) => return Err(ParseError::unsupported("ORDER BY ALL")),
    };
    let mut iter = exprs.into_iter();
    let first = iter
        .next()
        .ok_or_else(|| ParseError::new("ORDER BY requires a column"))?;
    if iter.next().is_some() {
        return Err(ParseError::unsupported(
            "ORDER BY with more than one column",
        ));
    }
    if first.with_fill.is_some() {
        return Err(ParseError::unsupported("ORDER BY ... WITH FILL"));
    }
    let column = match first.expr {
        sql::Expr::Identifier(id) => id.value,
        _ => {
            return Err(ParseError::unsupported(
                "ORDER BY on a non-column expression",
            ));
        }
    };
    // `asc == Some(false)` means DESC; ASC and unspecified both sort ascending.
    let descending = first.options.asc == Some(false);
    Ok(OrderBy { column, descending })
}

fn lower_limit(limit: Option<sql::LimitClause>) -> Result<Option<u64>> {
    let Some(clause) = limit else {
        return Ok(None);
    };
    let (limit, offset, limit_by) = match clause {
        sql::LimitClause::LimitOffset {
            limit,
            offset,
            limit_by,
        } => (limit, offset, limit_by),
        sql::LimitClause::OffsetCommaLimit { .. } => {
            return Err(ParseError::unsupported("`LIMIT offset, count` syntax"));
        }
    };
    if offset.is_some() {
        return Err(ParseError::unsupported("OFFSET"));
    }
    if !limit_by.is_empty() {
        return Err(ParseError::unsupported("LIMIT ... BY"));
    }
    let limit = limit.ok_or_else(|| ParseError::new("LIMIT requires a number"))?;
    match lower_literal(limit)? {
        Literal::Int(n) if n >= 0 => Ok(Some(n as u64)),
        _ => Err(ParseError::new("LIMIT requires a non-negative integer")),
    }
}

// ---- expressions & literals ------------------------------------------------

fn lower_expr(expr: sql::Expr) -> Result<Expr> {
    match expr {
        sql::Expr::Identifier(id) => Ok(Expr::Column(id.value)),
        sql::Expr::Nested(inner) => lower_expr(*inner),
        sql::Expr::Value(v) => lower_value(v.value).map(Expr::Literal),
        // Only signed numeric literals (`-5`, `+5`) are values; other unary
        // operators are routed to their own message below.
        sql::Expr::UnaryOp {
            op: sql::UnaryOperator::Minus | sql::UnaryOperator::Plus,
            ..
        } => lower_literal(expr).map(Expr::Literal),
        sql::Expr::UnaryOp {
            op: sql::UnaryOperator::Not,
            ..
        } => Err(ParseError::unsupported("NOT in WHERE")),
        sql::Expr::CompoundIdentifier(_) => Err(ParseError::unsupported(
            "qualified column references (table.column)",
        )),
        sql::Expr::BinaryOp { left, op, right } => lower_binary_op(*left, op, *right),
        _ => Err(ParseError::unsupported("this WHERE expression")),
    }
}

fn lower_binary_op(left: sql::Expr, op: sql::BinaryOperator, right: sql::Expr) -> Result<Expr> {
    use sql::BinaryOperator as B;
    // Each operand is lowered exactly once: the comparison/logical arm is
    // chosen first, then both sides are consumed. (Avoids cloning subtrees.)
    let compare_op = match op {
        B::Eq => Some(CompareOp::Eq),
        B::NotEq => Some(CompareOp::NotEq),
        B::Lt => Some(CompareOp::Lt),
        B::LtEq => Some(CompareOp::LtEq),
        B::Gt => Some(CompareOp::Gt),
        B::GtEq => Some(CompareOp::GtEq),
        _ => None,
    };
    if let Some(op) = compare_op {
        return Ok(Expr::Compare {
            left: Box::new(lower_expr(left)?),
            op,
            right: Box::new(lower_expr(right)?),
        });
    }
    let logical_op = match op {
        B::And => LogicalOp::And,
        B::Or => LogicalOp::Or,
        other => {
            return Err(ParseError::unsupported(format!(
                "operator `{other}` in WHERE"
            )));
        }
    };
    Ok(Expr::Logical {
        left: Box::new(lower_expr(left)?),
        op: logical_op,
        right: Box::new(lower_expr(right)?),
    })
}

/// Lower a literal value. Handles `-N` (parsed as a unary minus over a number)
/// so negative integers round-trip.
fn lower_literal(expr: sql::Expr) -> Result<Literal> {
    match expr {
        sql::Expr::Value(v) => lower_value(v.value),
        sql::Expr::UnaryOp {
            op: sql::UnaryOperator::Minus,
            expr,
        } => match lower_literal(*expr)? {
            Literal::Int(n) => Ok(Literal::Int(-n)),
            _ => Err(ParseError::new("`-` expects a number")),
        },
        sql::Expr::UnaryOp {
            op: sql::UnaryOperator::Plus,
            expr,
        } => lower_literal(*expr),
        _ => Err(ParseError::unsupported(
            "this value (expected an integer, a 'string', or NULL)",
        )),
    }
}

fn lower_value(value: sql::Value) -> Result<Literal> {
    match value {
        sql::Value::Number(n, _) => n
            .parse::<i64>()
            .map(Literal::Int)
            .map_err(|_| ParseError::new(format!("`{n}` is not a valid integer"))),
        sql::Value::SingleQuotedString(s) => Ok(Literal::Text(s)),
        sql::Value::Null => Ok(Literal::Null),
        other => Err(ParseError::unsupported(format!("literal `{other}`"))),
    }
}

// ---- helpers ---------------------------------------------------------------

/// Extract a bare, single-part table name; reject `schema.table` qualification.
fn single_name(name: &sql::ObjectName) -> Result<String> {
    match name.0.as_slice() {
        [part] => match part.as_ident() {
            Some(id) => Ok(id.value.clone()),
            None => Err(ParseError::unsupported("function name in this position")),
        },
        _ => Err(ParseError::unsupported(
            "qualified names (schema.table); use a bare table name",
        )),
    }
}

fn group_by_is_empty(group_by: &sql::GroupByExpr) -> bool {
    matches!(group_by, sql::GroupByExpr::Expressions(exprs, modifiers) if exprs.is_empty() && modifiers.is_empty())
}

fn statement_keyword(stmt: &sql::Statement) -> &'static str {
    match stmt {
        sql::Statement::Update { .. } => "UPDATE",
        sql::Statement::Delete(_) => "DELETE",
        sql::Statement::Drop { .. } => "DROP",
        _ => "this statement",
    }
}
