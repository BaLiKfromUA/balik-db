# balik-db Design — Storage

This document describes the **current** state of balik-db storage strategy and implementation.

## Storage track

We commit to **column store** track.

**Why:** the project's stated goal is OLAP-leaning / scan-heavy workloads, and
a column store gives us the right primitives for that down the road
(min/max-based skip pruning, per-column encoding choices, vectorized scans).
Per-column files also let us evolve encoding decisions independently across
columns, which keeps the format easy to change.

**Trade-offs we knowingly accept:**

- Point lookups (`get(rid)`) require touching one file per column; row stores
  would touch a single file. Acceptable for a learning DB; future work can
  add an in-memory column-handle cache.
- Updates and deletes are awkward in append-only column layouts. We've
  designed for them by leaving room (`flags.has_nulls`, `null_count`,
  positional RIDs that survive deletes via a separate delete bitmap)

## On-disk layout

```
demo-db/
├── balik.meta                  # database bootstrap (Stage 0)
├── catalog.toml                # index of tables — atomically rewritten on each create/drop
└── tables/
    └── 00000001/               # zero-padded table_id
        ├── manifest.toml       # this table's schema, row-group size, RID counter
        └── row_groups/
            └── 000000/         # row group id (zero-padded)
                ├── id.col      # column "id" — 56-byte header + future data
                ├── name.col    # column "name"
                └── deletes.bm  # per-row-group delete bitmap
```

Three persistence files (`balik.meta`, `catalog.toml`, `manifest.toml`) plus
N `.col` files and one `deletes.bm` per row group per table.


## Files and their formats

### `balik.meta` — database bootstrap

TOML, written once by `init`. Source of truth for "is this a balik database?".

```toml
magic = "balik-db"
format_version = 1
created = "unix:1772345678"
```

Read by `metadata::status` which is the gate function used by
`ColumnStore::open` to refuse to operate on uninitialized directories.


### TOML file integrity (`catalog.toml`, `manifest.toml`)

Both `catalog.toml` and `manifest.toml` are wrapped with a leading
checksum line:

```toml
# crc32 = 0xdeadbeef
format_version = 1
...
```

The first line is a TOML comment (parsers ignore it) carrying a CRC32-IEEE
of every byte that follows. On read the wrapper is verified before the body
is parsed; mismatch surfaces as `"catalog.toml: checksum mismatch: ..."` and
the load fails. On write, the body is serialized to TOML and the wrapper is
prepended before bytes hit disk.

The algorithm name lives **inside** the comment, not in `format_version`,
so a future migration to BLAKE3 / SHA-256 can be detected per-file without
bumping the file's format version.

CRC32-IEEE is the same polynomial used by ZIP, ext4, and most DB on-disk
formats. It catches bit rot and accidental tampering; it is **not** a
cryptographic hash. The bitwise implementation in `src/checksum.rs` keeps
the dep tree empty and is fast enough for files at our scale (KB at most).

`balik.meta` is not currently wrapped — it's tiny, written once at `init`,
and validated by structure already (magic + version). Adding checksum
coverage there is a future cleanup if real corruption shows up.

### `catalog.toml` — table index

TOML, **rewritten atomically** on every `create_table` / `drop_table`,
with a leading [checksum line](#toml-file-integrity-catalogtoml-manifesttoml).
Holds the list of tables and the next-id allocator. Tables are referenced
from this file by their `dir`, decoupling the on-disk path
(`tables/00000001`) from the user-visible name (`users`) so renames are
trivial later.

```toml
# crc32 = 0xdeadbeef
format_version = 1
storage_track = "column-store"
next_table_id = 3

[[tables]]
id = 1
name = "users"
dir = "tables/00000001"

[[tables]]
id = 2
name = "orders"
dir = "tables/00000002"
```

`next_table_id` is monotonic — never reused after a `drop_table`. This keeps
old `.col` files unambiguously identifiable even if a backup of an older
catalog state is restored.

**Atomic write protocol** (`Catalog::save_atomic` in `src/catalog/tables.rs`):

1. Serialize new state to `catalog.toml.tmp`.
2. `fsync` the temp file.
3. `rename(catalog.toml.tmp, catalog.toml)` — POSIX-atomic.

A crash at any point leaves either the old or new state, never a partial
file. We do **not** currently fsync the parent directory after rename, which
means a power cut can lose the rename even though the file content is
durable. Acceptable for a toy DB; documented as a known gap.

### `manifest.toml` — table schema

TOML, written **once at `create_table`**, with a leading
[checksum line](#toml-file-integrity-catalogtoml-manifesttoml). Never
updated in Stage 1 (`next_rid` will be updated in Stage 2 when inserts
arrive).

```toml
# crc32 = 0xdeadbeef
format_version = 1
table_id = 1
name = "users"
storage_track = "column-store"
row_group_size = 8192
next_rid = 0

[[columns]]
name = "id"
type = "INT"
nullable = false
file = "{row_group}/id.col"

[[columns]]
name = "name"
type = "TEXT"
nullable = false
file = "{row_group}/name.col"
```

Notable choices:

- **Logical type only.** `type = "INT"` says nothing about how the bytes are
  laid out — that's the column file's concern. See "Logical vs physical
  types" below.
- **Column order is the on-disk order.** Stored as a TOML array of tables
  (`[[columns]]`), which serde preserves on round-trip.
- **`file` is templated.** `{row_group}` is substituted at insert time.
  Stored explicitly so a future column could be remapped to a different path
  without changing the schema's logical view.
- **Single-shot write.** Manifest is not atomic — written once at create time
  and never modified, so torn-write recovery is trivially "delete the
  half-formed table dir and retry".


### `.col` files — column data files

Binary, one file per (column × row group). Every `.col` has exactly
the **56-byte header**:

```text
offset  size  field              value / notes
------  ----  -----              -------------
0       8     magic              ASCII "BALIKCOL"
8       4     format_version     u32 LE = 1
12      1     logical_type       0 = INT, 1 = TEXT
13      1     physical_encoding  0 = raw (only encoding defined for now)
14      1     flags              bit 0 = has_nulls; bits 1-7 reserved
15      1     reserved
16      4     row_count          u32 LE — rows in this file (incl. NULLs)
20      4     null_count         u32 LE — number of NULL rows
24      16    min                zeroed until row group seals (Stage 2+)
40      16    max                zeroed until row group seals (Stage 2+)
56      ...   data area          empty in Stage 1; populated in Stage 2+
```

Aligned at 56 bytes (multiple of 8) so the future data area is 8-byte
aligned. Reserved zones exist so we can grow the format with new fields
(e.g., compression algorithm, sortedness flag) without bumping
`format_version` or breaking older readers — readers that don't know about a
field see a zero, which by convention means "default / absent".

### `deletes.bm` — per-row-group delete bitmap

Binary, one file per row group, materialized at `create_table` time and
sized for the table's `row_group_size`. Bit `i` of the bitmap corresponds to the row at offset `i` within the row group:
`1` = deleted, `0` = live.

```text
offset  size  field             notes
------  ----  -----             -----
0       8     magic             ASCII "BALIKDEL"
8       4     format_version    u32 LE = 1
12      4     deleted_count     u32 LE — number of set bits, 0 in Stage 1
16      8     reserved          zeroed (forward compat)
24      ...   bitmap data       ceil(row_group_size / 8) bytes
                                (1 bit per slot, 1 = deleted, 0 = live)
```

For the default `row_group_size = 8192` the file is exactly `24 + 1024 =
1048` bytes at create time. In future, we would flip bits during `delete` / `update` and use
`deleted_count` as a fast skip check.

### NULL handling (planned for future)

When `flags.has_nulls = 1`, the data area begins with a NULL bitmap of
`ceil(row_count / 8)` bytes (1 = present, 0 = NULL), followed by per-encoding
values. When `flags.has_nulls = 0`, no bitmap is written even on nullable
columns — saves a decode pass when nothing is null. `null_count` is
authoritative regardless of the flag.

The schema's `nullable: bool` declares whether NULLs are *permitted*; the
header's `flags.has_nulls` reports whether NULLs are *present*.

## RID semantics

RIDs are **positional `u64`s**. The internal mapping is purely arithmetic:

```
group_id  = rid / row_group_size
offset    = rid % row_group_size
first_rid = group_id * row_group_size                  (implicit from group_id)
last_rid  = group_id * row_group_size + row_count - 1  (read row_count from .col header)
total     = next_rid                                   (from manifest.toml)
```

This works because of one invariant we maintain:

> **Row group fill discipline.** Inserts always target the latest row group.
> When it reaches `row_group_size` rows, a new row group is created. Every
> row group except the latest is exactly `row_group_size` rows; the latest
> may be partial. `row_group_size` is fixed at `create_table` time and never
> changes for the lifetime of the table.

Under this discipline we don't store `first_rid` or `last_rid` per row group
— the math from `row_group_size` is sufficient and there's a single source
of truth (no risk of metadata disagreeing with itself).

If we ever need variable-size row groups (e.g., for compaction or schema
migrations), we add a `first_rid: u64` field to row group metadata then. The
format's `format_version` field exists for exactly that kind of migration.

**Stable across deletes.** Once delete/update arrive, a per-row-group delete
bitmap will track holes. RIDs never shift or get reused — deleted rows leave
gaps, the next insert gets `next_rid`, not the freed slot. See [Mutation
model](#mutation-model-planned-for-stage-2) below.

## Mutation model (planned for future)

`UPDATE` and `DELETE` share a single mechanism: a **per-row-group delete
bitmap**.

**`DELETE rid`:** set bit `rid % row_group_size` in the bitmap of row group
`rid / row_group_size`.

**`UPDATE rid → new_record`:**

1. Mark the old RID deleted (as above).
2. Append `new_record` to the latest (open) row group, which assigns it a
   fresh RID.

The new RID is the row's identity going forward. The old `.col` bytes at the
original offset are never overwritten.

### Why update = delete + insert, not in-place

- **Append-only column files.** `.col` files only grow at the tail. Whole-file
  IO stays safe (no torn writes mid-file), seal-time min/max stats stay
  valid, and variable-length `TEXT` doesn't need to shift every later row when
  one value's length changes.
- **One mechanism for both verbs.** Delete and update share the bitmap; we
  don't design and maintain two separate write paths.
- **In-place doesn't fit variable-length anyway.** A new `TEXT` value of
  different length would force shifting every row after it in the file.


### Trade-offs

- **RIDs aren't stable across updates.** A row's positional id changes when
  it's updated. RIDs are an implementation detail — user-level identity is the
  primary key column, so this is invisible to SQL. Future indexes built on
  user keys will need to update their RID mapping on each write.
- **Reads must consult the bitmap.** Every scan and point-read filters out
  tombstoned RIDs. One bit per row — cheap, but not free.
- **Updates create holes.** A heavily-updated row group becomes sparse.
  Compaction (rewrite live rows into a fresh row group, drop the old one) is
  the eventual reclaim mechanism — explicit non-goal for now.

### Where the bitmap lives

One `deletes.bm` file per row group sitting alongside the `.col` files,
**not** in the `.col` headers. The byte format is documented under
[`deletes.bm`](#deletesbm--per-row-group-delete-bitmap) above.

Why a separate file:

- Deletes are **per row** across all columns simultaneously, not per column.
  Storing the bitmap N times — once per `.col` header — would duplicate the
  same bits and create a consistency hazard if the copies ever disagreed.
- The `.col` header is **write-once at seal time**. The delete bitmap is
  **mutable** by definition. Mixing them forces rewriting the header on every
  delete, which defeats append-only.

The bitmap file gets its own atomic-write protocol (tmp + fsync + rename)
when future logic starts mutating it.

## Logical vs physical types

Two distinct concerns, kept in separate layers:

| Layer | Knows | Doesn't know |
|---|---|---|
| `catalog::schema` (`ColumnType`) | The user said `INT` or `TEXT` | How those bytes are laid out |
| `manifest.toml` | The same — only the SQL-level type | Anything physical |
| `storage::column_file` | The byte format, including `physical_encoding` tag | The user-facing type names — it just maps `0 → ColumnType::Int`, `1 → ColumnType::Text` |

Adding a new physical encoding (e.g., varint INT, dictionary-encoded TEXT)
changes only `column_file.rs` and bumps the `physical_encoding` byte. The
catalog, manifest, schema validation, and CLI all stay untouched.

Adding a new logical type (e.g., `BIGINT`, `BOOL`) changes the catalog
surface (`ColumnType` variant, validation, DSL parser) and forces a
physical-encoding decision in storage.

## I/O strategy

balik-db reads and writes whole files via `std::fs::read` / `fs::write`.
There is no fixed-size paging within files, no DB-managed buffer pool, no
`mmap` or `pread`-style partial-file IO. This is a deliberate choice.

### Why whole-file IO is sufficient at our scale

The row group bounds the size of any single read. With the default
`row_group_size = 8192` rows:

| Column type | Bytes/value | Per `.col` file (full row group) |
|---|---|---|
| INT (raw `i64`) | 8 | 64 KB |
| TEXT (avg 64-byte values, with offsets) | ≈ 68 | ≈ 540 KB |
| TEXT (1 KB blobs, worst plausible) | ≈ 1028 | ≈ 8 MB |

A whole-file read at these sizes is one `read(2)` syscall and a single copy
through the OS page cache. Repeated reads on a hot column never hit disk —
the OS handles caching transparently.



