//! Internal abstract syntax tree for the supported SQL subset.
//!
//! This is the model the rest of the system works with — no third-party parser
//! types leak past this module. Every node derives `Debug` so the CLI can print
//! a readable, indented tree with `{:#?}`. The shape is deliberately small and
//! semantic (named fields, typed literals) rather than a bag of raw tokens.
//!
//! Several leaf types (`ColumnDef`, `DataType`, `Literal`, `Expr`, and the
//! operator enums) also derive `Serialize`. They are reused by the logical
//! plan, which serializes to JSON directly; the derive is purely additive and
//! keeps no dependency on storage or the catalog.

use serde::Serialize;

/// A single parsed top-level statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    CreateTable(CreateTable),
    Insert(Insert),
    Select(Select),
    DropTable(DropTable),
    /// `SHOW TABLES` — list the names of every table.
    ShowTables,
}

/// `CREATE TABLE name (col TYPE, ...)`.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateTable {
    pub table: String,
    pub columns: Vec<ColumnDef>,
}

/// `DROP TABLE name`.
#[derive(Debug, Clone, PartialEq)]
pub struct DropTable {
    pub table: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ColumnDef {
    pub name: String,
    pub ty: DataType,
    /// Whether the column accepts NULL. Follows SQL convention: a column is
    /// nullable unless declared `NOT NULL`. An explicit `NULL` is also honored.
    pub nullable: bool,
}

/// The column types the parser recognizes. Mirrors the engine's logical types
/// without depending on the catalog: a later binder maps these onto the real
/// `catalog::schema::ColumnType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum DataType {
    Int,
    Text,
}

/// `INSERT INTO name VALUES (v, ...)` — a single row.
#[derive(Debug, Clone, PartialEq)]
pub struct Insert {
    pub table: String,
    pub values: Vec<Literal>,
}

/// A literal value appearing in an INSERT row or a WHERE comparison.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Literal {
    Int(i64),
    Text(String),
    Null,
}

/// `SELECT projections FROM from [WHERE filter] [ORDER BY ...] [LIMIT n]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Select {
    pub projections: Projection,
    pub from: String,
    pub filter: Option<Expr>,
    pub order_by: Option<OrderBy>,
    pub limit: Option<u64>,
}

/// The column list following `SELECT`.
#[derive(Debug, Clone, PartialEq)]
pub enum Projection {
    /// `SELECT *`
    All,
    /// `SELECT a, b, c`
    Columns(Vec<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderBy {
    pub column: String,
    /// `true` for `DESC`, `false` for `ASC` or an unspecified direction.
    pub descending: bool,
}

/// A boolean/scalar expression, as used in `WHERE`.
///
/// The shape is deliberately permissive: operands are arbitrary `Expr`s, so the
/// type can represent trees that are not semantically meaningful (e.g. a
/// comparison whose operand is itself a logical combination). Checking that an
/// expression is well-typed and boolean-valued is a binder/validation concern,
/// not the parser's — see `docs/sql-grammar.md`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Expr {
    /// A column reference, e.g. `age`.
    Column(String),
    /// A literal value, e.g. `18` or `'Alice'` or `NULL`.
    Literal(Literal),
    /// A comparison between two operands, e.g. `age > 18`.
    Compare {
        left: Box<Expr>,
        op: CompareOp,
        right: Box<Expr>,
    },
    /// A boolean combination, e.g. `a AND b`.
    Logical {
        left: Box<Expr>,
        op: LogicalOp,
        right: Box<Expr>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum CompareOp {
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum LogicalOp {
    And,
    Or,
}
