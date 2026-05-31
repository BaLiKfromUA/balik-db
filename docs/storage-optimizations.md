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
| [Dictionary encoding for TEXT](#dictionary-encoding-for-text) | implemented | scan + equality filter |

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

Replace each row's TEXT value with a small integer code (`u32`) that
indexes into a per-`.col`-file dictionary holding the distinct values.
The dictionary itself is laid out as a mini raw-TEXT column (`u32`
end-offsets + concatenated UTF-8 blob). The encoding is selected per file
at write time — see [Per-file selector](#per-file-selector) below.

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

### On-disk layout

When `physical_encoding = 1`, after the optional presence bitmap the data
area is:

```text
dict_count : u32 LE
dict_ends  : u32 LE × dict_count        // end-offsets into the dict blob
dict_blob  : UTF-8 bytes                 // distinct values, first-seen order
codes      : u32 LE × row_count          // index into the dict; NULL rows store 0,
                                         //   masked by the presence bitmap
```

The dictionary section is exactly the raw-TEXT layout already implemented,
just over distinct values. Decode of row `i`: read `codes[i]`, follow it
into the dictionary.

### Per-file selector

Every TEXT write computes the exact encoded size for both candidate
layouts, then writes whichever is smaller. An exact tie keeps raw — no
decode indirection for no size win.

```text
raw_size  = row_count * 4 + sum(len(s)) over non-NULL rows
dict_size = 4 + dict_count * 4 + sum(len(s)) over distinct rows + row_count * 4
```

The single encode pass builds the dictionary anyway (to know
`dict_count` and the distinct blob length), so the estimate is exact and
nearly free — no double encode, no heuristic threshold.

Concrete examples:

| Values | raw | dict | Chosen |
|---|---|---|---|
| `[]`, `[NULL, NULL]` | 0, 8 | 4, 12 | raw (degenerate cases) |
| `["a", "b", "c", "d"]` (all distinct) | 20 | 40 | raw |
| `["alice", "alice"]` (2 repeats) | 18 | 21 | raw — break-even is ~3 |
| `["alice", "alice", "alice"]` | 27 | 25 | dict |
| `["shipped"×3, "pending"×2]` | 49 | 39 | dict |

So short low-volume columns naturally stay raw; the dict kicks in exactly
when there's enough repetition to pay for the `codes` column.

### Maintenance model

The dictionary is rebuilt from current values on every whole-file rewrite.
This piggybacks on the existing rewrite-on-append model — no incremental
dictionary state to maintain, no separate compaction step. The selector
is re-run on every write too, so a column can flip raw↔dict as its
cardinality changes across the column-file's lifetime.

### Trade-offs

- **Whole-file dict rebuild per insert.** Same write-amplification curve
  as the rest of the column-store layer; bounded by `row_group_size`.
- **No cross-row-group sharing.** Each `.col` file carries its own
  dictionary. A column with the same low-cardinality vocabulary across
  many row groups pays for the dictionary N times instead of once. Cross-
  group sharing needs schema-level state and isn't planned.
- **Selector is per-file, not per-column.** A column that's
  high-cardinality in one row group and low in another picks the right
  encoding for each — but loses the ability for downstream code to assume
  "this column is always dict-coded." Filter pushdown that wants integer
  comparison has to handle both branches.
- **TEXT only.** Dictionary encoding is the **sole** compression scheme in
  the engine, and it applies **only to TEXT** columns. INT columns are always
  stored as raw little-endian `i64`s (`physical_encoding = 0`) — they carry
  [min/max stats](#int-minmax-header-stats) for future skip-pruning, but their
  values are never compressed. Numeric compression (delta, frame-of-reference,
  bit-packing) is a separate, unplanned encoding; the `physical_encoding` byte
  has room for it without a `format_version` bump when it lands.
