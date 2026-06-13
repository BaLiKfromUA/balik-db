# Logical optimization

The optimizer rewrites a logical plan into an **equivalent** one that is cheaper
to execute — same rows out, less work to get them. It sits between planning and
execution (see [execution.md](execution.md)):

```
SQL text ──parser──▶ AST ──planner──▶ LogicalPlan ──optimizer──▶ LogicalPlan ──execute──▶ rows
```

Optimization is a pure `LogicalPlan → LogicalPlan` transformation. It reads no
row data and no catalog — every rule is derivable from the plan tree itself.
Building a plan never optimizes it; the rewrite runs only when asked for (the
`explain --optimize` flag), while `query` always optimizes before executing.

## Where it lives

The optimizer sits inside the `execution` module's `logical` layer, beside the
binder:

- `src/execution/logical/optimizer/mod.rs` — public entry point
  `optimize(plan) -> LogicalPlan`, re-exported as `execution::optimize`. It runs
  the rules in a fixed order: top-K first, then column pushdown — so pushdown
  sees the fused `TopK` and still collects its ordering column.
- `src/execution/logical/optimizer/top_k.rs` — the top-K fusion rule.
- `src/execution/logical/optimizer/column_pushdown.rs` — the column-pushdown
  rule.

## Rules

### Top-K

`ORDER BY ... LIMIT n` does not need a full sort followed by a separate
truncation — it only needs the `n` smallest or largest rows. This rule fuses a
`Limit` sitting directly over a `Sort` into a single `TopK` node, so execution
can keep just `n` rows as it scans rather than ordering the whole input.

```
SELECT id FROM users ORDER BY name LIMIT 10
```

```
        before                          after

Projection [id]                  Projection [id]
  Limit 10                         TopK [name] 10
    Sort [name]                      Scan users
      Scan users
```

It only fires on that exact adjacency: a `Sort` with no `Limit`, or a `Limit`
with no `Sort`, is left as-is. `TopK` is never produced by the binder — it exists
only as an optimizer output.

### Column pushdown

The binder emits a `Scan` with no column list, meaning "produce every column".
But a query only ever needs the columns it projects, filters on, or sorts by.
Column pushdown collects that set and records it on the `Scan`, so a column store
can read just those `.col` files instead of the whole row.

The needed set is the **projected** columns first (preserving their order), then
any columns referenced only by `Filter` or `Sort`, de-duplicated.

```
SELECT id, name FROM users WHERE age > 18
```

```
        before                          after

Projection [id, name]            Projection [id, name]
  Filter [age > 18]                Filter [age > 18]
    Scan users                       Scan users [id, name, age]
```

`age` appears in the scan list — the filter needs it — even though it is not
projected. `SELECT *` lists every column, since the binder already expanded it
under the `Projection`. Plans with no `Scan` (`CreateTable`, `Insert`) pass
through unchanged.

## CLI

```
balik-cli explain --db ./balik_db --optimize \
  --sql "SELECT id, name FROM users WHERE age > 18"
```

Without `--optimize` the plans print as the binder built them; with it, the
rules run first. `explain` prints both the logical and physical plans.
