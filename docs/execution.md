# Execution

Execution turns a **physical plan** into results: it walks a tree of physical
operators against the storage engine and produces either rows (for `SELECT`) or
an effect (for `CREATE TABLE` / `INSERT`). It is the last stage of the pipeline:

```
SQL text ─parser─▶ AST ─binder─▶ LogicalPlan ─optimizer─▶ LogicalPlan ─lower─▶ PhysicalPlan ─execute─▶ Result
```

The logical plan says *what* a query computes; the physical plan says *how* —
which concrete operators run, in what order, reading which columns. Lowering
turns one into the other (and pushes pruning hints and projections down into the
scan); the executor runs it.

## Where it lives

The physical layer is part of the `execution` module, beside the logical layer:

- `src/execution/physical/plan.rs` — the `PhysicalPlan` operator tree and its
  `Display` (the `…Exec` indented tree shown by `explain`).
- `src/execution/physical/executor.rs` — the executor: `execute` (the root
  entry) and `execute_stream` (the per-operator batch pipeline).
- `src/execution/physical/expr.rs` — vectorized predicate evaluation for filters.
- `src/execution/physical/prune.rs` — extracts row-group pruning hints from a
  WHERE predicate.
- `src/execution/physical/result.rs` — the `QueryResult` and its rendering.

## Execution model: a vectorized Volcano

Execution combines two classic ideas:

- **Volcano / iterator model.** Each operator is a node in a tree that *pulls*
  from its child: the root asks for the next chunk of output, which asks its
  child, and so on down to the scan. Control flow is lazy and pull-based — an
  operator only does work when something upstream demands output. Concretely
  every relational operator is a `BatchStream`
  (`Box<dyn Iterator<Item = Result<ColumnBatch, Error>>>`).

- **Vectorized execution.** The unit pulled between operators is **not a single
  row** (as in textbook Volcano) but a **`ColumnBatch`** — a column-major chunk
  of many rows, one row group's worth at a time. Working a batch at a time
  amortizes per-call overhead and lets operators run *column-at-a-time* kernels
  (e.g. the filter builds a boolean mask over a whole column) and skip whole row
  groups at the scan.

So the shape is a Volcano pipeline whose values are vectors. Most operators
**stream** (they transform one batch and pass it on), which keeps the pipeline
lazy — `LimitExec` can stop early, and `TableScanExec` can skip row groups —
without materializing the whole table. Two operators **block**: `SortExec` and
`TopKExec` must see all of their input before they can emit anything.

`CreateTableExec` and `InsertExec` are not part of the batch pipeline: they run
for their effect against storage and report a `QueryResult::Affected` line.

## Operators

| Operator          | Kind      | What it does                                            |
|-------------------|-----------|---------------------------------------------------------|
| `CreateTableExec` | root      | create a table in the catalog                           |
| `InsertExec`      | root      | append one row                                          |
| `TableScanExec`   | streaming | read column batches from storage, skipping row groups   |
| `FilterExec`      | streaming | keep rows matching a WHERE predicate                    |
| `ProjectionExec`  | streaming | select / reorder columns                                |
| `SortExec`        | blocking  | order all rows by one column                            |
| `LimitExec`       | streaming | stop after N rows                                       |
| `TopKExec`        | blocking  | the fused ORDER BY + LIMIT — keep the N best rows        |

## Row-group skipping at the scan

`TableScanExec` carries a set of pruning hints — `column <op> integer`
comparisons extracted from the WHERE clause (`prune.rs`). For each row group,
the storage engine consults that column's INT `min`/`max` zone map and skips the
entire group when no row in it can satisfy a hint. This is a pure optimization:

- Only **mandatory** comparisons are used. Pruning descends through `AND`
  (every conjunct must hold) but stops at `OR` (a row could match either side).
- A scan **skips groups, it does not filter rows.** A group that is kept may
  still contain rows that fail the predicate; removing those is `FilterExec`'s
  job. Pruning only ever avoids reading groups that can match *nothing*.

## Vectorized filtering

`FilterExec` evaluates its predicate **column-at-a-time** (`expr.rs`): a
comparison resolves each operand to a column vector or a broadcast literal and
compares them element-wise into a boolean **selection mask**; logical nodes
combine two masks with `AND`/`OR`. The operator then compacts the batch to the
rows whose mask entry is `true`.

## Results

A statement produces a `QueryResult`:

- `Rows { names, rows }` for `SELECT`, rendered as a simple aligned text table.
- `Affected(message)` for `CREATE TABLE` / `INSERT`.

The output column names are derived from the plan shape (a projection fixes them;
row-shaping operators above it pass them through), so an empty result still has a
correct header.

## Semantics and current limitations

- **ORDER BY must reference a selected column.** The plan nests the sort *above*
  the projection, so `SortExec` only sees the projected columns. `SELECT id FROM
  t ORDER BY name` therefore errors — sort by a column you also select. See the
  follow-up issue to lift this.
- **ASC / DESC**, single sort column, are supported. NULLs sort **first**
  ascending (last descending).
- **NULL comparisons are never true.** Evaluation is the minimal two-valued
  form: any comparison touching NULL is `false`, and `AND`/`OR` treat that as a
  plain `false`. A comparison between mismatched types (which the binder does not
  currently reject) is likewise `false`.
- **`LIMIT 0`** is valid and yields an empty result.
