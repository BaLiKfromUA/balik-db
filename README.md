# balik-db (WIP!)

<img src="docs/images/logo.png" align="right" height="200" alt="balik-db logo" />

Toy column-storage database written in Rust.

Goals:
- Learn internals of databases + practice them
- Get first experience with Rust
- Experiment with LLMs

## Compile & Run tests

```bash
cargo build

cargo test
```

## Manual usage

1. Build cli binary

```bash
cargo build --release

cp target/release/balik-cli . 
```

2. Initialize database

```bash
./balik-cli init

Initialized empty balik database at './balik_db'
```

3. Create your first table

```bash
./balik-cli table-create --table orders --columns "id:INT,total:INT"

Created table 'orders' (id=1) in './balik_db'
```

4. List tables

```bash
./balik-cli table-list

orders
```

5. Describe table

```bash
./balik-cli table-describe --table orders

Table:          orders
ID:             1
Storage:        column-store
Row group size: 8192
Columns:
  id                       INT    NOT NULL
  total                    INT    NOT NULL
```

6. Insert rows and read them back

```bash
./balik-cli row-insert --table orders --values "1,100"

Inserted into 'orders' as rid 0

./balik-cli row-insert --table orders --values "2,250"

Inserted into 'orders' as rid 1

./balik-cli row-get --table orders --rid 1

rid 1: id=2, total=250
```

`row-insert --values` is comma-delimited and positional (no quoting / no SQL
parser yet). The literal `NULL` (case-insensitive) maps to SQL NULL on
nullable columns. TEXT values therefore cannot contain a comma through this
interface — to be removed once the SQL parser lands.

7. Read all records from table

```bash
./balik-cli table-scan --table orders

rid 0: id=1, total=100
rid 1: id=2, total=250
```

8. Update a row

Updates are modeled as `delete + insert`, so the row gets a new rid — the
old one becomes a tombstone and the new value is appended at the tail.

```bash
./balik-cli row-update --table orders --rid 0 --values "1,150"

rid 0: updated as rid 2

./balik-cli row-get --table orders --rid 0

rid 0: not found

./balik-cli table-scan --table orders

rid 1: id=2, total=250
rid 2: id=1, total=150
```

9. Delete a row

```bash
./balik-cli row-delete --table orders --rid 1

rid 1: deleted

./balik-cli table-scan --table orders

rid 2: id=1, total=150
```

Deleting an unknown or already-deleted rid fails cleanly with `no such record`.

10. Drop a table

```bash
./balik-cli table-drop --table orders

Dropped table 'orders' from './balik_db'
```

11. Run basic validation

```bash
./balik-cli doctor

balik-db doctor
===============
[ok] balik-db version: 0.1.0
[ok] supported format version: 1
[ok] OS: linux (x86_64)
[ok] database found at './balik_db'
     format version: 1, created: unix:1777213236
[ok] catalog: 1 tables
[ok] orders: 2 columns (id INT, total INT)

```

## SQL parsing

The `parse` command turns a SQL string into an AST and prints it. It is a
front-end only — it does not touch storage, the catalog, or run the query.

```bash
./balik-cli parse --query "SELECT id, name FROM users WHERE age > 18 ORDER BY name LIMIT 10"

Select(
    Select {
        projections: Columns(
            [
                "id",
                "name",
            ],
        ),
        from: "users",
        filter: Some(
            Compare {
                left: Column(
                    "age",
                ),
                op: Gt,
                right: Literal(
                    Int(
                        18,
                    ),
                ),
            },
        ),
        order_by: Some(
            OrderBy {
                column: "name",
                descending: false,
            },
        ),
        limit: Some(
            10,
        ),
    },
)

```

It prints the AST to stdout on success; on a malformed query it writes an error
(with an approximate line/column) to stderr and exits non-zero. The supported
SQL subset, AST shape, and what the parser deliberately does *not* validate are
documented in [docs/sql-grammar.md](docs/sql-grammar.md).

## Logical planning

The `explain-logical` command takes the AST one step further: it builds a
**logical plan** (a tree of logical operators) and validates it against the
catalog — table and column references must exist, INSERT arity and types must
match. It does not execute the query.

```bash
./balik-cli explain-logical --query "SELECT id, name FROM users WHERE age > 18 LIMIT 10"

Limit 10
  Projection [id, name]
    Filter [age > 18]
      Scan users
```

Use `--format json` for machine-readable output. Parse or planning errors go to
stderr with a non-zero exit code. See
[docs/logical-planning.md](docs/logical-planning.md) for the operator set and
validation rules.

## Logging

The CLI uses [`tracing`](https://docs.rs/tracing) for diagnostics, wired through [`clap-verbosity-flag`](https://docs.rs/clap-verbosity-flag). Logs are silent by default and go to stdout when enabled — raise the level with `-v` (repeatable):

| Flag | Level | What you see |
|---|---|---|
| _(none)_ | warn | only warnings and errors |
| `-v` | info | high-level events (e.g. "initializing database") |
| `-vv` | debug | per-step events (directory creation, file writes, dispatched command) |
| `-vvv` | trace | everything |
| `-q` | off | suppress even warnings |

Example:

```bash
./balik-cli -vv init --db ./demo-db

DEBUG dispatching command args.command=Init { path: "./demo-db" }
 INFO initializing database path=./demo-db
DEBUG creating database directory path=./demo-db
DEBUG writing metadata file path=./demo-db/balik.meta
 INFO database initialized path=./demo-db
Initialized empty balik database at './demo-db'
```

## Project structure

```
src/
  main.rs              // entry point: parse args, dispatch, exit code
  error.rs             // shared Error enum used across modules
  checksum.rs          // CRC32 wrapper for balik.meta / catalog.toml / manifest.toml
  fs_atomic.rs         // shared tmp+fsync+rename atomic-write helper
  catalog/             // on-disk metadata and table schemas
    mod.rs
    metadata.rs        // bootstrap metadata file (magic, version, ...)
    schema.rs          // logical column types, schema validation, --columns DSL
    tables.rs          // persistent catalog: catalog.toml + manifest.toml + next_rid
  cli/                 // command-line frontend
    mod.rs             // Args, Command, parse()
    values.rs          // CLI --values parser / record renderer (retired with SQL parser)
    commands/
      mod.rs
      parse.rs         // parse a SQL query and print its AST
      explain_logical.rs // parse + build a logical plan and print it (tree / JSON)
      doctor.rs        // diagnostic command
      init.rs          // initialize a new database directory
      table_create.rs  // create a table from a schema DSL
      table_list.rs    // list table names
      table_describe.rs// print a table's schema and storage info
      table_drop.rs    // remove a table
      table_scan.rs    // print every live row in a table
      row_insert.rs    // insert one row, prints assigned rid
      row_get.rs       // fetch one row by rid
      row_update.rs    // update one row, prints the new rid (update = delete + insert)
      row_delete.rs    // tombstone one row by rid
  storage/             // storage trait + column-store implementation
    mod.rs             // Storage trait, Rid, TableHandle, Record, Value
    column_store.rs    // column-store implementation: insert / get / scan / update / delete
    column_file.rs     // .col header + INT raw / TEXT raw-or-dict data encoding
    delete_bitmap.rs   // per-row-group deletes.bm format
  parser/              // SQL front end: query string -> internal AST
    mod.rs             // public parse() entry point + supported-subset docs
    ast.rs             // internal AST (no third-party parser types leak out)
    lower.rs           // sqlparser tree -> internal AST + structural validation
    error.rs           // ParseError (independent of the storage Error)
  execution/           // query-engine layer above the Storage trait
    mod.rs             // re-exports the planner; execution lands here later
    planner/           // AST -> LogicalPlan
      mod.rs           // public plan() entry point
      plan.rs          // LogicalPlan structures + tree Display + JSON
      binder.rs        // binding + catalog validation
tests/
  cli-integration.rs   // end-to-end tests driving the `balik-cli` binary
```

Unit tests live next to the code they cover (inside `#[cfg(test)] mod tests` blocks); integration tests under `tests/` exercise the compiled binary via `assert_cmd`.

## AI Usage

Logo was generated by ChatGPT.

For additional code review and validating Rust usage, I have been using Claude Code (Opus model) in conversational mode.

For some parts of code, I have been using Claude Code (Opus model) in planning mode for incremental code generation. Such commits/PRs are marked with ✨
