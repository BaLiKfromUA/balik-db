# Logical planning

The planner turns a parsed AST into a **logical plan**: a tree of logical
operators describing *what* a query does, independent of how it is executed. It
sits between the parser (text → AST) and execution (plan → rows, see
[execution.md](execution.md)):

```
SQL text  ──parser──▶  AST  ──planner──▶  LogicalPlan  ──execute──▶  rows
```

Planning **does not execute** anything and reads no row data. It reads the
**catalog** (and only the catalog) to validate that referenced tables and
columns exist, and to expand `SELECT *`.

## Where it lives

The planner is the `logical` layer of the `execution` module — the query-engine
layer above the `Storage` trait — alongside the optimizer that rewrites the plan
and the physical layer that executes it:

- `src/execution/logical/plan.rs` — the `LogicalPlan` structures, their tree
  `Display`, and `serde::Serialize` for JSON output.
- `src/execution/logical/binder.rs` — binding and validation: AST + catalog →
  `LogicalPlan`. The only place that reads the catalog.
- `src/execution/logical/mod.rs` — public entry point
  `plan(stmt, storage) -> Result<LogicalPlan, Error>`, re-exported as
  `execution::plan`.

Logical/binding failures reuse the engine `Error` type: missing tables surface
as `TableNotFound`; unknown columns, INSERT arity, and type/nullability
mismatches surface as `InvalidQuery`; an ill-formed CREATE TABLE surfaces as
`InvalidSchema` (via the catalog's existing `Schema::validate`).

## Logical operators

| Node          | Meaning                                            |
|---------------|----------------------------------------------------|
| `CreateTable` | define a table (standalone root)                   |
| `Insert`      | add a row (standalone root)                         |
| `DropTable`   | remove a table (standalone root)                   |
| `ShowTables`  | list table names (standalone root)                 |
| `Scan`        | read a table — always the leaf of a SELECT tree    |
| `Filter`      | keep rows matching a WHERE predicate               |
| `Projection`  | choose / order output columns                      |
| `Sort`        | order rows by a column (ORDER BY)                  |
| `Limit`       | cap the number of rows                             |

For a SELECT, the relational operators nest innermost → outermost as
`Scan → Filter → Sort → Limit → Projection`, so the printed tree reads
outermost-first:

```
SELECT id, name FROM users WHERE age > 18 ORDER BY name LIMIT 10
```

```
Projection [id, name]
  Limit 10
    Sort [name]
      Filter [age > 18]
        Scan users
```

The projection sits *above* the sort and limit so that `ORDER BY` may reference a
column the SELECT list does not — the sort runs while every column is still
present, and the projection narrows to the output columns last.

`SELECT *` expands to the table's columns under the `Projection`. `Filter`
stores the WHERE expression verbatim; it is not evaluated here.

## Reusing AST leaf types

What makes a plan different from an AST is the **shape of the tree** — a SELECT
becomes a nested `Scan → Filter → Sort → Limit → Projection`, not a copy of the
`Select` struct. The leaf payloads, however, are reused from the parser's AST:
`ColumnDef` (in `CreateTable`), `Literal` (in `Insert`), and the `Expr` carried
by `Filter`. This is deliberate. They are stable, semantic value types with no
behavior to re-model, so introducing a parallel set of planner types would add
boilerplate and AST→plan conversions without buying anything. The natural point
to introduce dedicated plan-side expression types is when the planner needs to
attach resolved or type-checked information to expressions — which this stage
does not.

## Validation

- **CREATE TABLE** — table name is valid, column list is non-empty, no duplicate
  column names, types are supported (reuses `Schema::validate`).
- **INSERT** — the table exists, the value count matches the column count, each
  value's type matches its column, and `NULL` lands only on a nullable column.
- **SELECT** — the table exists; every column named in the projection, `WHERE`,
  and `ORDER BY` exists in the table.
- **DROP TABLE** — the table exists.
- **SHOW TABLES** — nothing to validate.
- **DESCRIBE** — the table exists.

## CLI

```
balik-cli explain --db ./balik_db --sql "SELECT id FROM users WHERE age > 18"
```

`explain` prints the logical operator tree followed by the physical plan it
lowers to. Parse or planning errors are written to stderr and the process exits
non-zero.
