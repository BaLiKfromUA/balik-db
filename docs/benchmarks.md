# Benchmarks

## Optimizer impact

[`scripts/bench_optimize.sh`](../scripts/bench_optimize.sh) measures how much the
logical optimizer ([logical-optimization.md](logical-optimization.md)) speeds up
a representative `SELECT`. It generates a dataset once, then times the same query
with and without `--optimize`.

```bash
./scripts/bench_optimize.sh [SIZE] [DB_DIR]

./scripts/bench_optimize.sh            # ~1GB into a temp dir, cleaned up on exit
./scripts/bench_optimize.sh 250MB      # smaller dataset
./scripts/bench_optimize.sh 1GB ./bench-db   # keep the database around
```

The benchmark query has two `WHERE` clauses, an `ORDER BY`, and a `LIMIT`:

```sql
SELECT id, sort_key FROM bench
WHERE filter_a > 500000 AND filter_b < 500000
ORDER BY sort_key LIMIT 20
```

### Why this dataset

`bench-gen` (below) builds a deliberately **wide** table:

| Column | Type | Role |
|---|---|---|
| `id` | INT | sequential row id |
| `sort_key` | INT | `ORDER BY` key, wide range so sorting does real work |
| `filter_a`, `filter_b` | INT | `WHERE` predicates, ~25% combined selectivity |
| `payload1`, `payload2`, `payload3` | TEXT | ~90-char random values the query never selects |

The wide shape makes both optimizer rules pay off at once:

- **Column pushdown** stamps the scan with only `id, sort_key, filter_a,
  filter_b`, so the three large `TEXT` payloads are never decoded.
- **Top-K fusion** turns `ORDER BY ... LIMIT 20` into a bounded `TopK` instead of
  sorting the whole filtered input.

The script prints both the unoptimized and optimized plans before timing, so the
structural difference (full-column `Scan` + `Sort` + `Limit` vs. pushed-down
`Scan` + `TopK`) is visible next to the numbers.

### Timing

If [`hyperfine`](https://github.com/sharkdp/hyperfine) is on `PATH` the script
uses it (warmup + multiple runs); otherwise it falls back to a timed `bash` loop.

CLI timings include process startup and opening the store. That overhead is small
relative to scanning a multi-hundred-MB dataset, but it is not zero — a
noise-free, engine-only measurement would time the `plan → optimize → lower →
execute` pipeline in-process instead.

## Generating data: `bench-gen`

The dataset is produced by a hidden development subcommand (not part of the
user-facing, SQL-first surface):

```bash
./balik-cli bench-gen --db ./bench-db [--table bench] [--size 1GB] [--rows N] [--seed S]
```

- `--size` accepts decimal (`GB`, `MB`) and binary (`GiB`, `MiB`) units; `--rows`
  overrides it with an exact count.
- `--seed` makes the generated data reproducible (default `0`).
- The table is dropped and recreated if it already exists.

### Why a dedicated loader

The normal `insert` path rewrites **every** column file on each inserted row, so
filling a row group of size `R` costs `O(R²)` write traffic — fine for
interactive use, but loading millions of rows that way means terabytes of I/O.
`bench-gen` instead calls `ColumnStore::bulk_load`, which encodes each column
once per row group (`O(N)` total), streaming one row group at a time to keep peak
memory bounded. See [storage-design.md](storage-design.md) for the row-group
layout and the write-amplification trade-off behind `insert`.
