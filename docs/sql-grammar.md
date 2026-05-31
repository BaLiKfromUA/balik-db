# SQL parser & AST

The parser turns a SQL string into an internal AST. It is a self-contained
subsystem: it does **not** read or write storage, consult the catalog, plan, or
execute anything. Its only job is text → AST, plus clear error reporting.

## Approach: library-based, with an isolated dependency

Parsing is delegated to the [`sqlparser`](https://crates.io/crates/sqlparser)
crate (tokenizer + grammar), and its tree is then **lowered** into our own AST.
The third-party types never escape the parser module:

- `src/parser/mod.rs` — public entry point `parse(sql) -> Result<Statement, ParseError>`
  and the only other place (besides `lower`) that names `sqlparser` (it calls
  the tokenizer).
- `src/parser/ast.rs` — the internal AST. No `sqlparser` types, no storage/catalog
  dependency.
- `src/parser/lower.rs` — converts `sqlparser::ast` → our `ast`, doing structural
  validation and rejecting anything outside the supported subset.
- `src/parser/error.rs` — `ParseError`, independent of the storage `Error`.

The rest of the system works only with `parser::ast` types. Any construct that
parses as valid SQL but is not in the supported subset is **rejected** during
lowering (with an `unsupported: …` message) rather than silently dropped — so a
returned AST always means "fully understood".

## Supported subset

```
statement := create_table | insert | select

create_table := CREATE TABLE name '(' column_def (',' column_def)* ')'
column_def   := name type [NULL | NOT NULL]    -- columns are nullable unless
                                               -- declared NOT NULL
type         := INT | TEXT                     -- INT also accepts INTEGER;
                                               -- TEXT also accepts VARCHAR/CHAR/STRING

insert       := INSERT INTO name VALUES '(' literal (',' literal)* ')'   -- single row
literal      := integer | "'" string "'" | NULL

select       := SELECT projection FROM name [WHERE expr] [ORDER BY name [ASC|DESC]] [LIMIT integer]
projection   := '*' | name (',' name)*
expr         := expr (AND | OR) expr
              | expr ('=' | '!=' | '<' | '<=' | '>' | '>=') expr
              | '(' expr ')'
              | name              -- column reference
              | literal
```

Notes:
- WHERE supports the six comparison operators and `AND`/`OR`, over column
  references and literals, with optional parentheses (precedence handled by the
  underlying grammar: `OR` binds looser than `AND`, which binds looser than
  comparisons).
- `ORDER BY` takes a single column with an optional `ASC`/`DESC`.

## AST shape

`Statement` is one of `CreateTable`, `Insert`, `Select` (see `src/parser/ast.rs`):

- `CreateTable { table, columns: Vec<ColumnDef { name, ty: DataType, nullable: bool }> }`,
  `DataType` ∈ `{ Int, Text }`. `nullable` defaults to `true` and is set to
  `false` by `NOT NULL`.
- `Insert { table, values: Vec<Literal> }`, `Literal` ∈ `{ Int(i64), Text(String), Null }`.
- `Select { projections, from, filter, order_by, limit }` where `projections`
  is `All` or `Columns(Vec<String>)`, `filter` is an optional `Expr`, `order_by`
  is an optional `OrderBy { column, descending }`, and `limit` an optional `u64`.
- `Expr` ∈ `{ Column, Literal, Compare { left, op, right }, Logical { left, op, right } }`.

The CLI renders the AST with the pretty `Debug` formatter (`{:#?}`), giving an
indented, easy-to-scan tree.

## What the parser deliberately does NOT check

These are left to a later binder / validation / execution layer:

- whether a table or column actually exists;
- whether value count matches column count;
- whether value types are compatible with column types;
- nullability and constraint enforcement;
- whether a `WHERE` expression is well-typed / boolean-valued — the `Expr` type
  is intentionally permissive about operand shape.

It also does not support (and rejects as `unsupported`): JOIN, GROUP BY, HAVING,
DISTINCT, subqueries, `WITH`, `UNION`/set operations, multiple FROM tables,
qualified names (`schema.table`, `table.column`), functions, multi-row or
column-list INSERT, `OFFSET`, column options other than `NULL`/`NOT NULL`
(DEFAULT, PRIMARY KEY, ...), table constraints, and other complex SQL.

## Running it

```bash
balik-cli parse --query "SELECT id, name FROM users WHERE age > 18 ORDER BY name LIMIT 10"
```

Prints the AST to stdout. On a parse error the message goes to **stderr** and
the process exits non-zero. Syntax errors carry an approximate `Line/Column`;
structural errors raised during lowering append the nearest identifier as
`(near \`name\`)`. The parser never panics on malformed input.
