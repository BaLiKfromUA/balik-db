# Logical optimization

The optimizer rewrites a logical plan into an **equivalent** one that is cheaper
to execute — same rows out, less work to get them. It sits between planning and
a future execution stage:

```
SQL text ──parser──▶ AST ──planner──▶ LogicalPlan ──optimizer──▶ LogicalPlan ──(later)──▶ rows
```

Optimization is a pure `LogicalPlan → LogicalPlan` transformation. It reads no
row data and no catalog — every rule is derivable from the plan tree itself.
Building a plan never optimizes it; the rewrite runs only when asked for (the
`explain-logical --optimize` flag).

## Where it lives

The optimizer is part of the `execution` module, beside the planner:

- `src/execution/optimizer/mod.rs` — public entry point
  `optimize(plan) -> LogicalPlan`, re-exported as `execution::optimize`. It runs
  the rules in a fixed order.
- `src/execution/optimizer/column_pushdown.rs` — the column-pushdown rule.

## Rules

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
balik-cli explain-logical --db ./balik_db \
  --query "SELECT id, name FROM users WHERE age > 18" --optimize
```

Without `--optimize` the plan prints as the binder built it; with it, the rules
run first. `--format tree` (default) and `--format json` both honor the flag.
