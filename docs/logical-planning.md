# Logical planning

The planner turns a parsed AST into a **logical plan**: a tree of logical
operators describing *what* a query does, independent of how it is executed. It
sits between the parser (text → AST) and a future execution stage (plan → rows):

```
SQL text  ──parser──▶  AST  ──planner──▶  LogicalPlan  ──(later)──▶  rows
```

Planning **does not execute** anything and reads no row data. It reads the
**catalog** (and only the catalog) to validate that referenced tables and
columns exist, and to expand `SELECT *`.

## Where it lives

The planner is part of the `execution` module — the query-engine layer above the
`Storage` trait — so that execution can join it there later:

- `src/execution/planner/plan.rs` — the `LogicalPlan` structures, their tree
  `Display`, and `serde::Serialize` for JSON output.
- `src/execution/planner/binder.rs` — binding and validation: AST + catalog →
  `LogicalPlan`. The only place that reads the catalog.
- `src/execution/planner/mod.rs` — public entry point
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
| `Scan`        | read a table — always the leaf of a SELECT tree    |
| `Filter`      | keep rows matching a WHERE predicate               |
| `Projection`  | choose / order output columns                      |
| `Sort`        | order rows by a column (ORDER BY)                  |
| `Limit`       | cap the number of rows                             |

For a SELECT, the relational operators nest innermost → outermost as
`Scan → Filter → Projection → Sort → Limit`, so the printed tree reads
outermost-first:

```
SELECT id, name FROM users WHERE age > 18 ORDER BY name LIMIT 10
```

```
Limit 10
  Sort [name]
    Projection [id, name]
      Filter [age > 18]
        Scan users
```

`SELECT *` expands to the table's columns under the `Projection`. `Filter`
stores the WHERE expression verbatim; it is not evaluated here.

## Validation

- **CREATE TABLE** — table name is valid, column list is non-empty, no duplicate
  column names, types are supported (reuses `Schema::validate`).
- **INSERT** — the table exists, the value count matches the column count, each
  value's type matches its column, and `NULL` lands only on a nullable column.
- **SELECT** — the table exists; every column named in the projection, `WHERE`,
  and `ORDER BY` exists in the table.

## CLI

```
balik-cli explain-logical --db ./balik_db --query "SELECT id FROM users WHERE age > 18"
```

`--format tree` (default) prints the operator tree; `--format json` prints the
same plan as JSON. Parse or planning errors are written to stderr and the
process exits non-zero.
