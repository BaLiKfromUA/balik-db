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
├── balik.meta                  # database bootstrap
├── balik.lock                  # whole-database advisory lock (see Concurrency)
├── catalog.toml                # index of tables — atomically rewritten on each create/drop
└── tables/
    └── 00000001/               # zero-padded table_id
        ├── manifest.toml       # this table's schema, row-group size, RID counter
        └── row_groups/
            └── 000000/         # row group id (zero-padded)
                ├── id.col      # column "id" — 56-byte header + encoded values
                ├── name.col    # column "name"
                └── deletes.bm  # per-row-group delete bitmap
```

Three persistence files (`balik.meta`, `catalog.toml`, `manifest.toml`) plus
N `.col` files and one `deletes.bm` per row group per table.


## Files and their formats

### `balik.meta` — database bootstrap

TOML, written once by `init`, wrapped with a leading
[checksum line](#toml-file-integrity). Source of truth for "is this a balik
database?".

```toml
# crc32 = 0xdeadbeef
magic = "balik-db"
format_version = 1
created = "unix:1772345678"
```

Read by `metadata::status` which is the gate function used by
`ColumnStore::open` to refuse to operate on uninitialized directories. A
checksum mismatch (or a missing wrapper) surfaces as `Status::Unreadable`.


### TOML file integrity

All three TOML files (`balik.meta`, `catalog.toml`, `manifest.toml`) are
wrapped with a leading checksum line:

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

### `catalog.toml` — table index

TOML, **rewritten atomically** on every `create_table` / `drop_table`,
with a leading [checksum line](#toml-file-integrity).
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

**Atomic write protocol** (`src/fs_atomic.rs`, shared across `balik.meta`,
`catalog.toml`, `manifest.toml`, and every `.col` file):

1. Serialize new state to `<file>.tmp`.
2. `fsync` the temp file.
3. `rename(<file>.tmp, <file>)` — POSIX-atomic.

A crash at any point leaves either the old or new state, never a partial
file. We do **not** currently fsync the parent directory after rename, which
means a power cut can lose the rename even though the file content is
durable. Acceptable for a toy DB; documented as a known gap.

A crash mid-insert can also leave a row group partially written across the
N column files of a single row — each column file is rewritten atomically
in isolation, but there is no cross-file transaction, so a crash partway
through the per-column loop can leave a *prefix* of columns one row longer
than the rest. The `next_rid` allocator is bumped only after every column
file has landed, so the torn row is never committed — but the disagreeing
column lengths would otherwise corrupt positional reads. See
[Crash recovery on open](#crash-recovery-on-open) for how this is repaired.

### `manifest.toml` — table schema

TOML, with a leading [checksum line](#toml-file-integrity). Schema fields
(`columns`, `row_group_size`, …) are written once at `create_table` and
never modified. `next_rid` is **rewritten atomically on every insert** —
same tmp+fsync+rename protocol as `catalog.toml` — and is the authoritative
RID allocator (always points at the next unused row).

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


### `.col` files — column data files

Binary, one file per (column × row group). Every `.col` is a **56-byte
header** followed by an encoded data area, and is rewritten **whole** on
every insert (atomic tmp+fsync+rename), so the header counts always match
the data.

```text
offset  size  field              value / notes
------  ----  -----              -------------
0       8     magic              ASCII "BALIKCOL"
8       4     format_version     u32 LE = 1
12      1     logical_type       0 = INT, 1 = TEXT
13      1     physical_encoding  0 = raw, 1 = dictionary (TEXT only; chosen
                                  per file by the encode-time size selector
                                  — see storage-optimizations.md)
14      1     flags              bit 0 = has_nulls; bits 1-7 reserved
15      1     reserved
16      4     row_count          u32 LE — rows in this file (incl. NULLs)
20      4     null_count         u32 LE — number of NULL rows
24      16    min                INT: i64 LE live min in first 8 bytes; TEXT: zero
                                  (see storage-optimizations.md)
40      16    max                INT: i64 LE live max in first 8 bytes; TEXT: zero
                                  (see storage-optimizations.md)
56      ...   data area          presence bitmap (if any) + encoded values
```

Aligned at 56 bytes (multiple of 8) so the data area is 8-byte aligned.
Reserved zones exist so we can grow the format with new fields (e.g.,
compression algorithm, sortedness flag) without bumping `format_version` or
breaking older readers — readers that don't know about a field see a zero,
which by convention means "default / absent".

#### Data area

When `flags.has_nulls = 1`, the data area starts with a **presence bitmap**
of `ceil(row_count / 8)` bytes (LSB-first within each byte; bit `i` = `1`
means row `i` is present, `0` means NULL). When `flags.has_nulls = 0` no
bitmap is emitted, saving a decode pass when nothing is NULL. `null_count`
is authoritative regardless of the flag.

Raw encodings (`physical_encoding = 0`):

- **INT (raw):** `row_count` little-endian `i64`s back-to-back. NULL rows
  store a `0` placeholder masked by the presence bitmap, keeping
  `offset = row * 8` arithmetic.
- **TEXT (raw):** `row_count` little-endian `u32` end-offsets, then the
  concatenated UTF-8 blob. Value `i` is `blob[end[i-1]..end[i]]` with
  `end[-1] = 0`. NULL rows are zero-length (offset unchanged from the
  previous row).

##### Worked example — TEXT raw with a NULL

Encoding the four values `["alice", NULL, "", "bob"]` for a nullable TEXT
column. `null_count = 1`, so `flags.has_nulls = 1`.

```text
header (56 B)
  ...
  row_count       = 4              (LE u32)
  null_count      = 1              (LE u32)
  flags           = 0b0000_0001    (has_nulls bit set)

data area
  presence bitmap (ceil(4/8) = 1 byte, LSB-first)
    bit 0 = 1  (alice present)
    bit 1 = 0  (NULL)
    bit 2 = 1  ("" present — empty string is NOT null)
    bit 3 = 1  (bob present)
    byte    = 0b0000_1101 = 0x0D

  end-offsets (4 × u32 LE = 16 bytes)
    end[0]  = 5    after "alice"
    end[1]  = 5    NULL row → zero-length, offset unchanged
    end[2]  = 5    "" → zero-length, offset unchanged
    end[3]  = 8    after "bob"
    bytes   = 05 00 00 00  05 00 00 00  05 00 00 00  08 00 00 00

  blob (8 bytes of UTF-8, no separators)
    bytes   = 61 6C 69 63 65   62 6F 62        // "alice" + "bob"
```

Decode of row `i` reads `start = end[i-1]` (or `0` when `i == 0`) and
`end = end[i]`. If the presence bit is `0` the row is NULL regardless of
the slice (which for NULL and `""` is empty anyway). If the bit is `1`,
the value is `blob[start..end]` parsed as UTF-8 — so row 0 yields
`"alice"`, row 2 yields the empty string, row 3 yields `"bob"`.

Total data area for this column: `1 + 16 + 8 = 25` bytes after the 56-byte
header (`81` bytes on disk).

TEXT columns can also write the **dictionary** encoding
(`physical_encoding = 1`): per-file `dict_count` + `dict_ends` + `dict_blob`
+ `codes`, chosen automatically when it's smaller than raw. The selector,
layout, and trade-offs live in
[`storage-optimizations.md`](storage-optimizations.md#dictionary-encoding-for-text).

### `deletes.bm` — per-row-group delete bitmap

Binary, one file per row group, materialized at `create_table` time and
sized for the table's `row_group_size`. Bit `i` of the bitmap corresponds to the row at offset `i` within the row group:
`1` = deleted, `0` = live.

```text
offset  size  field             notes
------  ----  -----             -----
0       8     magic             ASCII "BALIKDEL"
8       4     format_version    u32 LE = 1
12      4     deleted_count     u32 LE — number of set bits, 0 at create time
16      8     reserved          zeroed (forward compat)
24      ...   bitmap data       ceil(row_group_size / 8) bytes
                                (1 bit per slot, 1 = deleted, 0 = live)
```

For the default `row_group_size = 8192` the file is exactly `24 + 1024 =
1048` bytes at create time. `delete` flips a bit and bumps
`deleted_count`; `update` runs delete + insert (see
[Mutation model](#mutation-model)).

### NULL handling

The schema's `nullable: bool` declares whether NULLs are *permitted* at
insert time (enforced by `validate_record` in `src/storage/column_store.rs`).
The header's `flags.has_nulls` reports whether NULLs are *present* in the
encoded data area (see [Data area](#data-area) for the bitmap layout).

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

**Stable across deletes.** A per-row-group delete bitmap tracks holes;
RIDs never shift or get reused — deleted rows leave gaps and the next
insert gets `next_rid`, not the freed slot. See [Mutation
model](#mutation-model) below.

## Mutation model

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
  IO stays safe (no torn writes mid-file), header stats are recomputed
  cleanly on each rewrite, and variable-length `TEXT` doesn't need to shift
  every later row when one value's length changes.
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
- The delete bitmap is hot — every insert/scan reads it, every delete
  rewrites it. Keeping it in its own small file means a delete only touches
  a few bytes of disk (24-byte header + bitmap), not the whole column.

The bitmap file uses the same atomic-write protocol (tmp + fsync + rename)
as every other file in the database.

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

### Write amplification & known costs

`insert` rewrites **every** `.col` file in the current row group, not just
the appended tail. For one inserted row across `N` columns with `R` rows
already in the open row group, the work is:

| Step | Cost |
|---|---|
| Read existing column image | `R` decoded values per column × `N` columns |
| Re-encode with one extra value | `R + 1` values per column × `N` columns |
| Atomic write (tmp + fsync + rename) | one `write` + one `fsync` + one `rename` per column |
| `next_rid` bump | one atomic rewrite of `manifest.toml` |

Bounded by `row_group_size` (default 8192), so the worst case per insert is
~64 KB rewritten per INT column or ~540 KB per average-TEXT column, plus
`N + 1` fsyncs. Quadratic in row-group fill: filling one row group of size
`R` costs `O(N · R²)` bytes of write traffic.

**Why we accept it:**

- Each column file's whole-file rewrite gives us per-file atomicity for
  free (tmp + fsync + rename) — no in-file allocator, no free-space
  tracking, no torn-write recovery code.
- TEXT's variable-length offsets array would have to shift on any in-place
  edit anyway, so true in-place append isn't simpler than a full rewrite.
- The cost is bounded by `row_group_size`; correctness and durability are
  the explicit design targets, throughput is not.

**TODO — WAL + in-memory row buffer (future stage).** 

The standard answer to per-row insert cost: inserts land in an in-memory row
buffer for the currently-open row group and flush to columnar `.col` files
only when the buffer fills or the group seals. The buffer is what cuts
write amplification from `O(N · R²)` to `O(N · R)`; a **write-ahead log**
sitting next to the buffer is what preserves the current contract that
every insert is durable on return. Both pieces have to land together — a
buffer without a WAL silently weakens durability across restarts, and a
WAL without a buffer just adds an fsync per insert without cutting the
rewrite cost.

Smaller mitigations possible without a WAL: a **batched insert** API
(`&[Record]` → one column rewrite per batch instead of per row) and
**group commit** of fsyncs across concurrent inserts. Neither changes the
durability contract.

## Concurrency

balik-db is **single-writer per database**, enforced by a whole-database
advisory lock. `ColumnStore::open` opens `balik.lock` in the database root
and takes an exclusive lock on it (`std::fs::File::try_lock`, which maps to
`flock(2)` on Unix); the handle is held for the lifetime of the open store
and the lock is released when the store is dropped or the process exits.

```text
process A: open(db) ── holds balik.lock ──────────────► drop ─ releases
process B:        open(db) → "already open in another process" (fails fast)
```

**Why a single exclusive lock.** Every mutating operation is a
read-modify-write of whole files (the `.col` rewrite cycle, the `next_rid`
bump in `manifest.toml`, the `catalog.toml` rewrite). Two processes
interleaving those cycles would lose writes or tear a row group across
columns — exactly the inconsistency the [crash recovery](#crash-recovery-on-open)
pass repairs, but with no `next_rid` boundary to recover against. A single
exclusive lock is the simplest thing that makes the whole-file-rewrite model
correct under concurrent access.

**Properties.**

- The lock is **advisory** — it only blocks other openers that go through
  `ColumnStore::open`. A process writing the files directly bypasses it.
- It is held **per open file description**, so two `ColumnStore`s in the same
  process (or two processes) contend correctly; the second `open` fails fast
  rather than blocking.
- It is released by the OS on process exit, **including a crash**, so a
  killed process never leaves a stale lock that needs manual clearing.

**Trade-offs we knowingly accept.**

- **No concurrent readers.** The lock is exclusive even for read-only
  commands (`table-scan`, `row-get`), so a long scan blocks an unrelated
  read in another process. A future shared/exclusive (reader/writer) split
  could let read-only opens take a shared lock; not worth it for a CLI-driven
  toy DB today.
- **Whole-database granularity.** The lock covers every table, not just the
  one being written. Per-table locking would allow parallel writes to
  independent tables but adds lock-ordering complexity we don't need yet.

## Crash recovery on open

Because a crash mid-insert can leave the open row group's column files with
disagreeing lengths (a prefix of columns one row longer than the rest — see
the [atomic write protocol](#catalogtoml--table-index)), `ColumnStore::open`
runs a **reconciliation pass** once, while the database lock is held, before
any data-plane call.

For each table it locates the open (last) row group and compares every
column's `row_count` against the committed length derived from `next_rid`:

```text
group     = next_rid / row_group_size
expected  = next_rid % row_group_size      (live rows the catalog committed)
```

- **`row_count == expected`** — clean; nothing to do (the common path, and a
  cheap header-only read, no full decode).
- **`row_count > expected`** — an uncommitted torn insert. The column is
  truncated back to `expected` rows and rewritten (atomic tmp+fsync+rename),
  dropping the half-written tail so all columns re-agree.
- **`row_count < expected`** — a column shorter than the catalog committed.
  This can't result from the insert protocol; it is treated as corruption
  and fails the open.

A table whose `manifest.toml` can't be read is **skipped** (logged), not
treated as fatal: it is already broken and can't be reconciled or used, so
failing the whole-database open over one bad table would lose access to every
healthy table. The access path — and `doctor` — report it per-table instead.

Defense in depth: even after recovery, `scan` validates that all columns in a
loaded row group have equal length and surfaces a corruption error rather
than indexing a short column out of bounds.

### Known gaps

- The reconciliation only inspects the **open** row group of each table.
  Sealed row groups are immutable once full, so they can't be torn by an
  insert — but on-disk bit rot in a sealed group is caught lazily by the
  per-file checksums / decode checks at read time, not eagerly on open.
- There is no parent-directory fsync after the recovery rewrites (same
  accepted gap as every other write — see the
  [atomic write protocol](#catalogtoml--table-index)).

