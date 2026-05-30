# balik-db Design — Storage Optimizations

This document collects optimizations layered on top of the core storage
design described in [`storage-design.md`](storage-design.md). Each section
describes one optimization: its purpose, the on-disk shape, when it's
maintained, what it costs, and its current status.

The base format reserves bytes for these features (`physical_encoding` tag,
`min`/`max` stat slots) so most of them can be added without bumping
`format_version`.

| Optimization | Status | Reader/consumer |
|---|---|---|
| [INT min/max stats](#int-minmax-header-stats) | implemented | future skip-pruning |
| [Dictionary encoding for TEXT](#dictionary-encoding-for-text) | planned | scan + equality filter |

---

## INT min/max header stats

The 16-byte `min` and `max` slots in each `.col` header (offsets `24` and
`40` — see `.col` layout in [`storage-design.md`](storage-design.md)) carry
the smallest and largest **live** values for INT columns. They exist as a
prerequisite for skip-pruning: a query like `WHERE id BETWEEN 100 AND 200`
can skip an entire row group whose stored `[min, max]` doesn't overlap the
filter.

### Slot layout

```text
offset  size  contents
------  ----  --------
24      8     min  : i64 LE
32      8     reserved (zeroed)
40      8     max  : i64 LE
48      8     reserved (zeroed)
```

The trailing 8 bytes of each slot are kept zero so the field can later grow
to a wider type (e.g. a fixed-precision decimal) without bumping
`format_version`. Readers parsing the current i64 form simply ignore those
bytes.

TEXT columns leave all 32 bytes zeroed — `min`/`max` over arbitrary strings
needs a separate design pass (prefix bounds, length ranges) and isn't worth
the byte budget today.

### Empty / all-NULL / all-deleted: sentinel pair

When a column has no live value to summarize (empty file, every row NULL,
every row tombstoned), the slots store `min = i64::MAX, max = i64::MIN`.
That pair satisfies `min > max`, which is impossible for any real range, so
it unambiguously means "no live values" without a dedicated flag byte.

The typed accessors `Header::int_min` / `Header::int_max` decode the
sentinel as `None`, so the rest of the system never sees the raw `i64::MAX`
value leak out.

### Live, not physical

Min/max is computed over **live** values only — NULLs and tombstoned
offsets are skipped. The alternative ("physical" min/max, taken over every
value in the data area regardless of bitmap state) would be cheaper to
maintain but would leave the stored range loose after deletes. For the toy
DB the tighter live range is worth the cost.

Concretely, the cases:

| When | Source | Cost |
|---|---|---|
| `insert` | Inline in the existing per-column rewrite — bitmap loaded once per row group, passed to `write_column`. | No extra IO; one min/max pass per column already-touched value. |
| `delete` | After the bitmap bit flip, every INT column in the affected row group is rewritten with the same data area and refreshed stats. | One extra atomic rewrite per INT column per delete (`O(row_group_size × INT_cols)`). |
| `update` | Falls out of `delete + insert`. | Sum of both. |

**Why we accept the delete cost.** A previous design kept delete as a
single bitmap-bit flip (`O(1)`), at the cost of a loose envelope after
tombstones. The empty-row-group case (delete the only row → range still
points at the deleted value) was the deciding factor: a query against the
"empty" group would still have to scan it. Rewriting the INT files keeps
the model simple — stats always reflect live state, no "stats are loose
after delete" caveat to remember.

### Consumer

No reader currently filters on `int_min` / `int_max`; they're stamped today
so future skip-pruning has stats reaching back to the first write of every
row group. Adding stats later would force a backfill rewrite of every
existing column file.

---

## Dictionary encoding for TEXT

**Status:** planned (S6 in the stage-2 implementation plan). The format
hook is already in place — `physical_encoding = 1` is reserved for it in
the `.col` header — but the encode/decode path is not implemented yet.

### What it is

Replace each row's TEXT value with a small integer code (`u32`) that
indexes into a per-`.col`-file dictionary holding the distinct values. The
dictionary itself is laid out as a mini raw-TEXT column (`u32` end-offsets
+ concatenated UTF-8 blob).

### Why it fits a column store

- **Repeated low-cardinality text is common.** Status codes, country
  names, enum-shaped strings, currency tickers — a column store sees those
  values once per row but stores them N times.
- **Per-column choice.** Each `.col` file picks its own
  `physical_encoding`, so a low-cardinality column dictionarizes while a
  free-form-text neighbor stays raw. No cross-column coupling.
- **Scan-friendly.** Decoding is a code → dictionary indexed lookup, the
  same shape as an INT column read.
- **Equality filters compare integers.** A predicate like
  `status = 'SHIPPED'` can be rewritten to a code comparison once per
  query — the per-row work is integer equality, not string equality.

### On-disk layout sketch

When `physical_encoding = 1`, after the optional presence bitmap the data
area is:

```text
dict_count : u32 LE
dict_ends  : u32 LE × dict_count        // end-offsets into the dict blob
dict_blob  : UTF-8 bytes                 // distinct values, concatenated
codes      : u32 LE × row_count          // index into the dict; NULL rows store 0,
                                         //   masked by the presence bitmap
```

The dictionary section is exactly the raw-TEXT layout already implemented,
just over distinct values. Decode of row `i`: read `codes[i]`, follow it
into the dictionary.

### Encoding selector rule

**TEXT → dictionary, INT → raw.** Documented and enforced at the single
encode switch point in `column_file.rs`. Raw-TEXT decode is kept reachable
so the `physical_encoding` tag stays meaningful and the format is
reversible.

### Maintenance model

The dictionary is rebuilt from current values on every whole-file rewrite.
This piggybacks on the existing rewrite-on-append model — no incremental
dictionary state to maintain, no separate compaction step.

### Trade-offs

- **High-cardinality TEXT loses.** When most values are distinct, the
  dictionary grows to ~row_count entries and the codes column adds 4 bytes
  per row on top — net size goes up vs raw. A future selector pass could
  measure cardinality and stay on raw, but the v1 rule is "TEXT always
  dictionary" for simplicity.
- **Whole-file dict rebuild per insert.** Same write-amplification curve
  as the rest of the column-store layer; bounded by `row_group_size`.
- **No cross-row-group sharing.** Each `.col` file carries its own
  dictionary. A column with the same low-cardinality vocabulary across
  many row groups pays for the dictionary N times instead of once. Cross-
  group sharing needs schema-level state and isn't planned.
