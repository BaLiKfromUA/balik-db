//! On-disk format for column data files (`.col`).
//!
//! Each `.col` file holds the values of one column for one row group: a
//! fixed 56-byte header followed by the data area. The whole file is
//! rewritten on every change, so the header counts always match the data.
//!
//! Byte layout (all multi-byte integers little-endian):
//!
//! ```text
//! offset  size  field              notes
//! ------  ----  -----              -----
//! 0       8     magic              ASCII "BALIKCOL"
//! 8       4     format_version     u32 = 1
//! 12      1     logical_type       0=INT, 1=TEXT
//! 13      1     physical_encoding  0=raw, 1=dictionary (TEXT only)
//! 14      1     flags              bit 0 = has_nulls; bits 1-7 reserved
//! 15      1     reserved
//! 16      4     row_count          u32 — rows in this file, including NULLs
//! 20      4     null_count         u32 — number of NULL rows
//! 24      16    min                INT: i64 LE in the first 8 bytes of the
//!                                  slot, remaining 8 bytes zeroed; TEXT: all
//!                                  16 bytes zeroed
//! 40      16    max                INT: i64 LE in the first 8 bytes of the
//!                                  slot, remaining 8 bytes zeroed; TEXT: all
//!                                  16 bytes zeroed
//! 56      ...   data area          presence bitmap (if any) + encoded values
//! ```
//!
//! INT `min`/`max` are computed over **live** values only (non-NULL and not
//! marked deleted in the row group's bitmap), so the range tightens when a
//! row is tombstoned. An empty / all-NULL / all-deleted INT column stores
//! the sentinel pair `min = i64::MAX, max = i64::MIN`, which `int_min` /
//! `int_max` translate to `None`. The 16-byte slot leaves 8 bytes of room
//! for future growth (e.g. fixed-precision decimal) without a format bump.
//!
//! The 56-byte header is 8-byte aligned so the data area can be aligned for
//! SIMD or memcpy paths.
//!
//! ## Data area
//!
//! When `flags.has_nulls = 1`, the data area begins with a presence bitmap
//! sized `ceil(row_count / 8)` bytes (bit i: 1 = present, 0 = NULL), followed
//! by the encoded values. When `flags.has_nulls = 0`, no bitmap is emitted —
//! saving a decode pass when nothing is NULL. `null_count` always reflects
//! truth regardless of the flag.
//!
//! Raw encodings (`physical_encoding = 0`):
//!
//! - **INT**: `row_count` little-endian `i64`s; NULL rows store a `0`
//!   placeholder masked by the presence bitmap, keeping `offset = row * 8`.
//! - **TEXT**: `row_count` little-endian `u32` end-offsets, then the
//!   concatenated UTF-8 blob. Value `i` is `blob[end[i-1]..end[i]]` with
//!   `end[-1] = 0`; NULL rows are zero-length.
//!
//! Dictionary encoding (`physical_encoding = 1`, TEXT only). TEXT columns
//! pick raw vs dictionary per file at encode time — whichever produces
//! fewer bytes wins; an exact tie keeps raw to avoid the decode
//! indirection for no size win. The dictionary is rebuilt from current
//! values on every whole-file rewrite — no incremental dict state. Data
//! area after the optional presence bitmap:
//!
//! - `dict_count : u32 LE` — number of distinct non-NULL values.
//! - `dict_ends  : u32 LE × dict_count` — end-offsets into `dict_blob`.
//! - `dict_blob  : UTF-8 bytes` — distinct values concatenated, in
//!   first-seen order; same shape as the raw-TEXT data area but over
//!   distinct values.
//! - `codes      : u32 LE × row_count` — index into `dict_ends`; NULL rows
//!   store `0` (masked by the presence bitmap, so the value is never
//!   consulted).
//!
//! See `docs/storage-optimizations.md` for the selector rationale and
//! trade-offs.

use std::fs;
use std::path::Path;

use crate::catalog::schema::ColumnType;
use crate::error::Error;
use crate::fs_atomic;
use crate::storage::{Value, delete_bitmap};

const HEADER_SIZE: usize = 56;
const MAGIC: &[u8; 8] = b"BALIKCOL";
const FORMAT_VERSION: u32 = 1;

const FLAG_HAS_NULLS: u8 = 0b0000_0001;

const LOGICAL_INT: u8 = 0;
const LOGICAL_TEXT: u8 = 1;
const PHYSICAL_RAW: u8 = 0;
const PHYSICAL_DICT: u8 = 1;

/// Parsed view of a `.col` header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub format_version: u32,
    pub logical_type: ColumnType,
    pub physical_encoding: u8,
    pub flags: u8,
    pub row_count: u32,
    pub null_count: u32,
    pub min: [u8; 16],
    pub max: [u8; 16],
}

impl Header {
    /// True when the data area carries a presence bitmap (some row is NULL).
    pub fn has_nulls(&self) -> bool {
        self.flags & FLAG_HAS_NULLS != 0
    }

    /// Smallest live value in an INT column, or `None` when the column has
    /// no live values (empty / all-NULL / all-deleted) or isn't INT-typed.
    /// The sentinel pair `min > max` encodes the empty case.
    //
    // Exposed for the future skip-pruning consumer; covered by in-module
    // tests today.
    #[allow(dead_code)]
    pub fn int_min(&self) -> Option<i64> {
        self.int_range().map(|(min, _)| min)
    }

    /// Largest live value in an INT column, see [`Header::int_min`].
    #[allow(dead_code)]
    pub fn int_max(&self) -> Option<i64> {
        self.int_range().map(|(_, max)| max)
    }

    fn int_range(&self) -> Option<(i64, i64)> {
        if self.logical_type != ColumnType::Int {
            return None;
        }
        let min = i64::from_le_bytes(self.min[0..8].try_into().unwrap());
        let max = i64::from_le_bytes(self.max[0..8].try_into().unwrap());
        if min > max { None } else { Some((min, max)) }
    }
}

fn logical_type_byte(ty: ColumnType) -> u8 {
    match ty {
        ColumnType::Int => LOGICAL_INT,
        ColumnType::Text => LOGICAL_TEXT,
    }
}

fn parse_logical_type(b: u8) -> Result<ColumnType, Error> {
    match b {
        LOGICAL_INT => Ok(ColumnType::Int),
        LOGICAL_TEXT => Ok(ColumnType::Text),
        other => Err(Error::corrupt(format!("unknown logical type tag {other}"))),
    }
}

/// Build a 56-byte empty header for a new `.col` file of the given type.
/// INT columns get the sentinel pair (`min = i64::MAX`, `max = i64::MIN`)
/// so an empty file reads back as "no live range." TEXT columns leave the
/// slots zeroed — no stats are recorded for TEXT yet. The
/// `physical_encoding` byte is raw by default; the TEXT writer in
/// `encode_column` may overwrite it to dictionary once values are present.
fn empty_header(ty: ColumnType) -> [u8; HEADER_SIZE] {
    let mut buf = [0u8; HEADER_SIZE];
    buf[0..8].copy_from_slice(MAGIC);
    buf[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    buf[12] = logical_type_byte(ty);
    buf[13] = PHYSICAL_RAW;
    if matches!(ty, ColumnType::Int) {
        write_int_stats(&mut buf, i64::MAX, i64::MIN);
    }
    buf
}

/// Stamp the INT `(min, max)` pair into the header. The i64 LE value goes
/// into the first 8 bytes of each 16-byte slot; the remaining 8 bytes per
/// slot stay zero as room for a wider type later without a format bump.
fn write_int_stats(header: &mut [u8], min: i64, max: i64) {
    header[24..32].copy_from_slice(&min.to_le_bytes());
    header[40..48].copy_from_slice(&max.to_le_bytes());
}

/// Compute the (min, max) range for an INT column over live values only —
/// skips NULLs and offsets flagged in `deleted`. Returns the empty sentinel
/// pair when no live value remains.
fn compute_int_range(values: &[Value], deleted: &[u8]) -> (i64, i64) {
    let mut min = i64::MAX;
    let mut max = i64::MIN;
    for (i, v) in values.iter().enumerate() {
        if let Value::Int(n) = v
            && !delete_bitmap::is_deleted(deleted, i)
        {
            if *n < min {
                min = *n;
            }
            if *n > max {
                max = *n;
            }
        }
    }
    (min, max)
}

/// Create a new `.col` file at `path` with an empty header.
pub fn write_empty(path: &Path, ty: ColumnType) -> Result<(), Error> {
    tracing::debug!(path = %path.display(), ty = ty.as_str(), "writing empty column file");
    fs::write(path, empty_header(ty)).map_err(|e| Error::io("write column file", e))
}

/// Parse a `.col` header from the leading bytes of a file image, validating
/// magic and format version.
fn parse_header(bytes: &[u8], path: &Path) -> Result<Header, Error> {
    if bytes.len() < HEADER_SIZE {
        return Err(Error::corrupt(format!(
            "column file '{}' shorter than {HEADER_SIZE}-byte header",
            path.display()
        )));
    }
    if &bytes[0..8] != MAGIC {
        return Err(Error::corrupt(format!(
            "column file '{}' has bad magic",
            path.display()
        )));
    }
    let format_version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    if format_version > FORMAT_VERSION {
        return Err(Error::corrupt(format!(
            "column file '{}' uses format_version {format_version}, this binary supports {FORMAT_VERSION}",
            path.display()
        )));
    }
    let logical_type = parse_logical_type(bytes[12])?;
    let physical_encoding = bytes[13];
    let flags = bytes[14];
    let row_count = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
    let null_count = u32::from_le_bytes(bytes[20..24].try_into().unwrap());
    let mut min = [0u8; 16];
    min.copy_from_slice(&bytes[24..40]);
    let mut max = [0u8; 16];
    max.copy_from_slice(&bytes[40..56]);
    Ok(Header {
        format_version,
        logical_type,
        physical_encoding,
        flags,
        row_count,
        null_count,
        min,
        max,
    })
}

// ---- data-area codec ----

/// Presence bitmap for `values`: bit i set (LSB-first within each byte) when
/// row i is non-NULL. Written ahead of the data when a column has any NULL.
fn build_present_bitmap(values: &[Value]) -> Vec<u8> {
    let mut bm = vec![0u8; values.len().div_ceil(8)];
    for (i, v) in values.iter().enumerate() {
        if !matches!(v, Value::Null) {
            bm[i / 8] |= 1 << (i % 8);
        }
    }
    bm
}

/// Whether row i is present (non-NULL). `None` bitmap means no NULLs at all.
fn is_present(present: Option<&[u8]>, i: usize) -> bool {
    match present {
        None => true,
        Some(bm) => bm[i / 8] & (1 << (i % 8)) != 0,
    }
}

fn encode_int_raw(values: &[Value]) -> Result<Vec<u8>, Error> {
    let mut out = Vec::with_capacity(values.len() * 8);
    for v in values {
        let n = match v {
            Value::Int(n) => *n,
            Value::Null => 0,
            Value::Text(_) => return Err(Error::invalid_value("INT column received a TEXT value")),
        };
        out.extend_from_slice(&n.to_le_bytes());
    }
    Ok(out)
}

/// Encode TEXT values, picking the smaller of raw and dictionary per file.
/// Returns the chosen `physical_encoding` tag together with the encoded
/// bytes (no header, no presence bitmap — caller assembles those).
///
/// A single pass over `values` builds the dictionary and tallies the
/// non-NULL byte length, which is enough to compute the exact byte size of
/// both candidate layouts:
///
/// - raw  = `row_count * 4 + sum(len(s))` over non-NULL rows
/// - dict = `4 + dict_count * 4 + sum(len(s))` over **distinct** non-NULL
///   rows `+ row_count * 4`
///
/// Dict wins for repeated values; raw wins when distinct count is close to
/// row count (the codes column adds 4 bytes/row that dict never recovers).
/// Ties keep raw.
fn encode_text(values: &[Value]) -> Result<(u8, Vec<u8>), Error> {
    use std::collections::HashMap;

    let mut codes: Vec<u32> = Vec::with_capacity(values.len());
    let mut index: HashMap<&str, u32> = HashMap::new();
    let mut dict_strs: Vec<&str> = Vec::new();
    let mut raw_blob_size: usize = 0;
    for v in values {
        match v {
            Value::Text(s) => {
                raw_blob_size += s.len();
                let code = if let Some(&c) = index.get(s.as_str()) {
                    c
                } else {
                    let c = u32::try_from(dict_strs.len()).map_err(|_| {
                        Error::invalid_value("TEXT dictionary exceeds the u32 entry limit")
                    })?;
                    index.insert(s.as_str(), c);
                    dict_strs.push(s.as_str());
                    c
                };
                codes.push(code);
            }
            Value::Null => codes.push(0),
            Value::Int(_) => return Err(Error::invalid_value("TEXT column received an INT value")),
        }
    }

    let row_count = values.len();
    let raw_size = row_count * 4 + raw_blob_size;
    let dict_blob_size: usize = dict_strs.iter().map(|s| s.len()).sum();
    let dict_size = 4 + dict_strs.len() * 4 + dict_blob_size + row_count * 4;

    if dict_size < raw_size {
        let dict_count = u32::try_from(dict_strs.len())
            .map_err(|_| Error::invalid_value("TEXT dictionary exceeds the u32 entry limit"))?;
        let mut out = Vec::with_capacity(dict_size);
        out.extend_from_slice(&dict_count.to_le_bytes());
        let mut blob = Vec::with_capacity(dict_blob_size);
        for s in &dict_strs {
            blob.extend_from_slice(s.as_bytes());
            let end = u32::try_from(blob.len()).map_err(|_| {
                Error::invalid_value("TEXT dictionary blob exceeds the 4 GiB limit")
            })?;
            out.extend_from_slice(&end.to_le_bytes());
        }
        out.extend_from_slice(&blob);
        for code in &codes {
            out.extend_from_slice(&code.to_le_bytes());
        }
        Ok((PHYSICAL_DICT, out))
    } else {
        let mut out = Vec::with_capacity(raw_size);
        let mut blob = Vec::with_capacity(raw_blob_size);
        for v in values {
            if let Value::Text(s) = v {
                blob.extend_from_slice(s.as_bytes());
            }
            let end = u32::try_from(blob.len())
                .map_err(|_| Error::invalid_value("TEXT column exceeds the 4 GiB limit"))?;
            out.extend_from_slice(&end.to_le_bytes());
        }
        out.extend_from_slice(&blob);
        Ok((PHYSICAL_RAW, out))
    }
}

/// Build the whole byte image (header + data area) for a column of `values`.
/// `deleted` is the row group's delete bitmap (or `&[]` when no rows are
/// tombstoned) — it's only consulted to exclude tombstoned offsets from the
/// INT min/max stats; the data area still encodes every value so positional
/// rids stay stable.
fn encode_column(ty: ColumnType, values: &[Value], deleted: &[u8]) -> Result<Vec<u8>, Error> {
    let row_count = u32::try_from(values.len())
        .map_err(|_| Error::invalid_value("row group exceeds the u32 row-count limit"))?;
    let null_count = values.iter().filter(|v| matches!(v, Value::Null)).count() as u32;
    let has_nulls = null_count > 0;

    let mut buf = empty_header(ty).to_vec();
    if has_nulls {
        buf[14] = FLAG_HAS_NULLS;
    }
    buf[16..20].copy_from_slice(&row_count.to_le_bytes());
    buf[20..24].copy_from_slice(&null_count.to_le_bytes());
    if matches!(ty, ColumnType::Int) {
        let (min, max) = compute_int_range(values, deleted);
        write_int_stats(&mut buf, min, max);
    }

    if has_nulls {
        buf.extend_from_slice(&build_present_bitmap(values));
    }
    let data = match ty {
        ColumnType::Int => encode_int_raw(values)?,
        ColumnType::Text => {
            let (encoding, bytes) = encode_text(values)?;
            buf[13] = encoding;
            bytes
        }
    };
    buf.extend_from_slice(&data);
    Ok(buf)
}

fn decode_int_raw(
    data: &[u8],
    row_count: usize,
    present: Option<&[u8]>,
    path: &Path,
) -> Result<Vec<Value>, Error> {
    if data.len() < row_count * 8 {
        return Err(Error::corrupt(format!(
            "column file '{}' INT data is truncated",
            path.display()
        )));
    }
    let mut out = Vec::with_capacity(row_count);
    for i in 0..row_count {
        if is_present(present, i) {
            let bytes = data[i * 8..i * 8 + 8].try_into().unwrap();
            out.push(Value::Int(i64::from_le_bytes(bytes)));
        } else {
            out.push(Value::Null);
        }
    }
    Ok(out)
}

fn decode_text_raw(
    data: &[u8],
    row_count: usize,
    present: Option<&[u8]>,
    path: &Path,
) -> Result<Vec<Value>, Error> {
    let offs_len = row_count * 4;
    if data.len() < offs_len {
        return Err(Error::corrupt(format!(
            "column file '{}' TEXT offsets are truncated",
            path.display()
        )));
    }
    let blob = &data[offs_len..];
    let mut out = Vec::with_capacity(row_count);
    let mut start = 0usize;
    for i in 0..row_count {
        let end = u32::from_le_bytes(data[i * 4..i * 4 + 4].try_into().unwrap()) as usize;
        if end < start || end > blob.len() {
            return Err(Error::corrupt(format!(
                "column file '{}' has an out-of-range TEXT offset",
                path.display()
            )));
        }
        if is_present(present, i) {
            let s = std::str::from_utf8(&blob[start..end])
                .map_err(|_| {
                    Error::corrupt(format!(
                        "column file '{}' has invalid UTF-8",
                        path.display()
                    ))
                })?
                .to_string();
            out.push(Value::Text(s));
        } else {
            out.push(Value::Null);
        }
        start = end;
    }
    Ok(out)
}

fn decode_text_dict(
    data: &[u8],
    row_count: usize,
    present: Option<&[u8]>,
    path: &Path,
) -> Result<Vec<Value>, Error> {
    if row_count == 0 {
        return Ok(Vec::new());
    }
    if data.len() < 4 {
        return Err(Error::corrupt(format!(
            "column file '{}' dictionary header is truncated",
            path.display()
        )));
    }
    let dict_count = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
    let ends_start = 4;
    let ends_end = ends_start + dict_count * 4;
    if data.len() < ends_end {
        return Err(Error::corrupt(format!(
            "column file '{}' dictionary offsets are truncated",
            path.display()
        )));
    }
    let mut ends: Vec<usize> = Vec::with_capacity(dict_count);
    for i in 0..dict_count {
        let off = ends_start + i * 4;
        ends.push(u32::from_le_bytes(data[off..off + 4].try_into().unwrap()) as usize);
    }
    let blob_len = ends.last().copied().unwrap_or(0);
    let blob_start = ends_end;
    let blob_end = blob_start + blob_len;
    if data.len() < blob_end {
        return Err(Error::corrupt(format!(
            "column file '{}' dictionary blob is truncated",
            path.display()
        )));
    }
    let blob = &data[blob_start..blob_end];
    let mut dict: Vec<&str> = Vec::with_capacity(dict_count);
    let mut start = 0usize;
    for end in &ends {
        if *end < start || *end > blob.len() {
            return Err(Error::corrupt(format!(
                "column file '{}' has an out-of-range dictionary offset",
                path.display()
            )));
        }
        let s = std::str::from_utf8(&blob[start..*end]).map_err(|_| {
            Error::corrupt(format!(
                "column file '{}' has invalid UTF-8 in the dictionary",
                path.display()
            ))
        })?;
        dict.push(s);
        start = *end;
    }
    let codes_start = blob_end;
    let codes_end = codes_start + row_count * 4;
    if data.len() < codes_end {
        return Err(Error::corrupt(format!(
            "column file '{}' dictionary codes are truncated",
            path.display()
        )));
    }
    let mut out = Vec::with_capacity(row_count);
    for i in 0..row_count {
        if is_present(present, i) {
            let off = codes_start + i * 4;
            let code = u32::from_le_bytes(data[off..off + 4].try_into().unwrap()) as usize;
            if code >= dict_count {
                return Err(Error::corrupt(format!(
                    "column file '{}' has dictionary code {} out of range (dict has {} entries)",
                    path.display(),
                    code,
                    dict_count
                )));
            }
            out.push(Value::Text(dict[code].to_string()));
        } else {
            out.push(Value::Null);
        }
    }
    Ok(out)
}

/// Encode `values` for a column of type `ty` and replace the `.col` file at
/// `path` atomically: write a sibling temp file, fsync it, then rename it
/// into place so a crash leaves either the old image or the new one.
///
/// `deleted` is the row group's current delete bitmap (or `&[]` when no
/// rows in the group are tombstoned). It is only used to exclude tombstoned
/// offsets from the INT min/max stats — the data area itself always carries
/// every value so positional rids remain stable.
pub fn write_column(
    path: &Path,
    ty: ColumnType,
    values: &[Value],
    deleted: &[u8],
) -> Result<(), Error> {
    tracing::debug!(path = %path.display(), rows = values.len(), "writing column file");
    let bytes = encode_column(ty, values, deleted)?;
    fs_atomic::write(path, &bytes, "column file")
}

/// Read and parse only the 56-byte header of a `.col` file, without decoding
/// the data area. Crash recovery uses this to check a column's row count
/// cheaply before deciding whether the file needs truncation.
pub fn read_header(path: &Path) -> Result<Header, Error> {
    use std::io::Read;
    let mut file = fs::File::open(path).map_err(|e| Error::io("open column file", e))?;
    let mut buf = [0u8; HEADER_SIZE];
    file.read_exact(&mut buf)
        .map_err(|e| Error::io("read column header", e))?;
    parse_header(&buf, path)
}

/// Drop the trailing rows of a `.col` file, keeping its first `new_len` rows,
/// and rewrite the whole file atomically. `deleted` is the row group's delete
/// bitmap so the rewritten INT min/max stats reflect live values. Errors if
/// the file holds fewer than `new_len` rows — that is corruption, not a tail
/// to trim.
pub fn truncate_column(
    path: &Path,
    ty: ColumnType,
    new_len: usize,
    deleted: &[u8],
) -> Result<(), Error> {
    let (_, mut values) = read_column(path)?;
    if values.len() < new_len {
        return Err(Error::corrupt(format!(
            "column file '{}' holds {} rows, cannot truncate to {new_len}",
            path.display(),
            values.len()
        )));
    }
    values.truncate(new_len);
    write_column(path, ty, &values, deleted)
}

/// Read and decode a `.col` file, returning its parsed header and every
/// value in row order. NULL rows decode to `Value::Null`. Corrupt or
/// truncated files return `Err` so the scan path never yields garbage.
pub fn read_column(path: &Path) -> Result<(Header, Vec<Value>), Error> {
    let bytes = fs::read(path).map_err(|e| Error::io("read column file", e))?;
    let header = parse_header(&bytes, path)?;
    let row_count = header.row_count as usize;
    let data = &bytes[HEADER_SIZE..];

    let (present, data) = if header.has_nulls() {
        let bm_len = row_count.div_ceil(8);
        if data.len() < bm_len {
            return Err(Error::corrupt(format!(
                "column file '{}' presence bitmap is truncated",
                path.display()
            )));
        }
        (Some(&data[..bm_len]), &data[bm_len..])
    } else {
        (None, data)
    };

    let values = match (header.logical_type, header.physical_encoding) {
        (ColumnType::Int, PHYSICAL_RAW) => decode_int_raw(data, row_count, present, path)?,
        (ColumnType::Text, PHYSICAL_RAW) => decode_text_raw(data, row_count, present, path)?,
        (ColumnType::Text, PHYSICAL_DICT) => decode_text_dict(data, row_count, present, path)?,
        (ty, enc) => {
            return Err(Error::corrupt(format!(
                "column file '{}' has {} column with unsupported physical encoding {enc}",
                path.display(),
                ty.as_str()
            )));
        }
    };
    Ok((header, values))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn empty_header_is_56_bytes() {
        assert_eq!(empty_header(ColumnType::Int).len(), HEADER_SIZE);
    }

    #[test]
    fn empty_header_has_magic_and_version() {
        let buf = empty_header(ColumnType::Int);
        assert_eq!(&buf[0..8], MAGIC);
        let ver = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        assert_eq!(ver, FORMAT_VERSION);
    }

    #[test]
    fn empty_header_encodes_int_type() {
        let buf = empty_header(ColumnType::Int);
        assert_eq!(buf[12], LOGICAL_INT);
        assert_eq!(buf[13], PHYSICAL_RAW);
        assert_eq!(buf[14], 0); // flags
    }

    #[test]
    fn empty_header_encodes_text_type() {
        let buf = empty_header(ColumnType::Text);
        assert_eq!(buf[12], LOGICAL_TEXT);
    }

    #[test]
    fn empty_header_zeroes_counts_and_stamps_int_sentinel() {
        let buf = empty_header(ColumnType::Int);
        let row_count = u32::from_le_bytes(buf[16..20].try_into().unwrap());
        let null_count = u32::from_le_bytes(buf[20..24].try_into().unwrap());
        assert_eq!(row_count, 0);
        assert_eq!(null_count, 0);
        // Sentinel pair for INT: min = i64::MAX, max = i64::MIN. The trailing
        // 8 bytes of each 16-byte slot stay zero.
        assert_eq!(
            i64::from_le_bytes(buf[24..32].try_into().unwrap()),
            i64::MAX
        );
        assert_eq!(
            i64::from_le_bytes(buf[40..48].try_into().unwrap()),
            i64::MIN
        );
        assert!(buf[32..40].iter().all(|&b| b == 0));
        assert!(buf[48..56].iter().all(|&b| b == 0));
    }

    #[test]
    fn empty_header_zeroes_text_stats() {
        let buf = empty_header(ColumnType::Text);
        assert!(buf[24..40].iter().all(|&b| b == 0), "TEXT min stays zero");
        assert!(buf[40..56].iter().all(|&b| b == 0), "TEXT max stays zero");
    }

    #[test]
    fn write_then_read_round_trips_int() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("id.col");
        write_empty(&path, ColumnType::Int).unwrap();
        let (h, _) = read_column(&path).unwrap();
        assert_eq!(h.format_version, FORMAT_VERSION);
        assert_eq!(h.logical_type, ColumnType::Int);
        assert_eq!(h.physical_encoding, PHYSICAL_RAW);
        assert_eq!(h.flags, 0);
        assert!(!h.has_nulls());
        assert_eq!(h.row_count, 0);
        assert_eq!(h.null_count, 0);
        assert_eq!(h.int_min(), None);
        assert_eq!(h.int_max(), None);
    }

    #[test]
    fn write_then_read_round_trips_text() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("name.col");
        write_empty(&path, ColumnType::Text).unwrap();
        let (h, _) = read_column(&path).unwrap();
        assert_eq!(h.logical_type, ColumnType::Text);
    }

    #[test]
    fn has_nulls_reflects_flag_bit() {
        let mut h = Header {
            format_version: FORMAT_VERSION,
            logical_type: ColumnType::Int,
            physical_encoding: 0,
            flags: 0,
            row_count: 0,
            null_count: 0,
            min: [0u8; 16],
            max: [0u8; 16],
        };
        assert!(!h.has_nulls());
        h.flags |= FLAG_HAS_NULLS;
        assert!(h.has_nulls());
    }

    #[test]
    fn read_column_rejects_short_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("bad.col");
        fs::write(&path, b"too short").unwrap();
        let err = read_column(&path).unwrap_err();
        assert!(err.to_string().contains("shorter than"));
    }

    #[test]
    fn read_column_rejects_bad_magic() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("bad.col");
        let mut bytes = empty_header(ColumnType::Int);
        bytes[0..8].copy_from_slice(b"NOPENOPE");
        fs::write(&path, bytes).unwrap();
        let err = read_column(&path).unwrap_err();
        assert!(err.to_string().contains("bad magic"));
    }

    #[test]
    fn read_column_rejects_too_new_format() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("bad.col");
        let mut bytes = empty_header(ColumnType::Int);
        let future = FORMAT_VERSION + 1;
        bytes[8..12].copy_from_slice(&future.to_le_bytes());
        fs::write(&path, bytes).unwrap();
        let err = read_column(&path).unwrap_err();
        assert!(err.to_string().contains("format_version"));
    }

    #[test]
    fn read_column_rejects_unknown_logical_type() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("bad.col");
        let mut bytes = empty_header(ColumnType::Int);
        bytes[12] = 99;
        fs::write(&path, bytes).unwrap();
        let err = read_column(&path).unwrap_err();
        assert!(err.to_string().contains("unknown logical type"));
    }

    fn roundtrip(ty: ColumnType, values: &[Value]) -> Vec<Value> {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("data.col");
        write_column(&path, ty, values, &[]).unwrap();
        read_column(&path).unwrap().1
    }

    #[test]
    fn int_round_trips_without_nulls() {
        let values = vec![Value::Int(1), Value::Int(-42), Value::Int(i64::MAX)];
        assert_eq!(roundtrip(ColumnType::Int, &values), values);
    }

    #[test]
    fn int_round_trips_with_nulls() {
        let values = vec![Value::Int(7), Value::Null, Value::Int(9), Value::Null];
        assert_eq!(roundtrip(ColumnType::Int, &values), values);

        // The presence bitmap is materialized: flag set, null_count correct.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("data.col");
        write_column(&path, ColumnType::Int, &values, &[]).unwrap();
        let (h, _) = read_column(&path).unwrap();
        assert!(h.has_nulls());
        assert_eq!(h.row_count, 4);
        assert_eq!(h.null_count, 2);
    }

    #[test]
    fn text_round_trips_with_nulls_and_duplicates() {
        let values = vec![
            Value::Text("alice".to_string()),
            Value::Null,
            Value::Text(String::new()), // empty string is distinct from NULL
            Value::Text("alice".to_string()),
        ];
        assert_eq!(roundtrip(ColumnType::Text, &values), values);
    }

    #[test]
    fn empty_and_all_null_columns_round_trip() {
        assert_eq!(roundtrip(ColumnType::Int, &[]), Vec::<Value>::new());
        let all_null = vec![Value::Null, Value::Null];
        assert_eq!(roundtrip(ColumnType::Text, &all_null), all_null);
    }

    #[test]
    fn no_nulls_means_no_presence_bitmap() {
        // 3 INT rows, no NULLs: 56-byte header + 3 * 8 bytes, no bitmap.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("data.col");
        write_column(
            &path,
            ColumnType::Int,
            &[Value::Int(1), Value::Int(2), Value::Int(3)],
            &[],
        )
        .unwrap();
        assert_eq!(fs::read(&path).unwrap().len(), HEADER_SIZE + 24);
    }

    #[test]
    fn write_column_rejects_value_of_wrong_type() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("data.col");
        let err =
            write_column(&path, ColumnType::Int, &[Value::Text("x".to_string())], &[]).unwrap_err();
        assert!(err.to_string().contains("INT column received a TEXT"));
    }

    fn read_int_stats(values: &[Value], deleted: &[u8]) -> (Option<i64>, Option<i64>) {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("stats.col");
        write_column(&path, ColumnType::Int, values, deleted).unwrap();
        let (h, _) = read_column(&path).unwrap();
        (h.int_min(), h.int_max())
    }

    #[test]
    fn int_min_max_covers_live_values_only() {
        let values = vec![
            Value::Int(3),
            Value::Null,
            Value::Int(-7),
            Value::Int(42),
            Value::Int(i64::MIN),
            Value::Int(i64::MAX),
        ];
        assert_eq!(
            read_int_stats(&values, &[]),
            (Some(i64::MIN), Some(i64::MAX))
        );
    }

    #[test]
    fn int_min_max_is_sentinel_for_empty_and_all_null() {
        assert_eq!(read_int_stats(&[], &[]), (None, None));
        assert_eq!(
            read_int_stats(&[Value::Null, Value::Null], &[]),
            (None, None)
        );
    }

    #[test]
    fn int_min_max_equals_single_value() {
        assert_eq!(read_int_stats(&[Value::Int(99)], &[]), (Some(99), Some(99)));
    }

    #[test]
    fn int_min_max_excludes_deleted_offsets() {
        // Live values [1, 5, 9]; offset 1 (value 5) is tombstoned.
        let values = vec![Value::Int(1), Value::Int(5), Value::Int(9)];
        let deleted = vec![0b0000_0010u8]; // bit 1 set
        assert_eq!(read_int_stats(&values, &deleted), (Some(1), Some(9)));

        // Deleting the extremes leaves only the middle value.
        let deleted = vec![0b0000_0101u8]; // bits 0 and 2
        assert_eq!(read_int_stats(&values, &deleted), (Some(5), Some(5)));

        // Deleting every offset returns to the sentinel.
        let deleted = vec![0b0000_0111u8];
        assert_eq!(read_int_stats(&values, &deleted), (None, None));
    }

    #[test]
    fn text_columns_never_report_int_stats() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("name.col");
        write_column(
            &path,
            ColumnType::Text,
            &[Value::Text("alice".to_string())],
            &[],
        )
        .unwrap();
        let (h, _) = read_column(&path).unwrap();
        assert_eq!(h.int_min(), None);
        assert_eq!(h.int_max(), None);
        // Slots stay zeroed for TEXT.
        assert!(h.min.iter().all(|&b| b == 0));
        assert!(h.max.iter().all(|&b| b == 0));
    }

    fn write_text_and_read_back(values: &[Value]) -> (Header, Vec<Value>) {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("text.col");
        write_column(&path, ColumnType::Text, values, &[]).unwrap();
        let (h, decoded) = read_column(&path).unwrap();
        (h, decoded)
    }

    #[test]
    fn text_low_cardinality_picks_dict() {
        let values = vec![
            Value::Text("shipped".to_string()),
            Value::Text("pending".to_string()),
            Value::Text("shipped".to_string()),
            Value::Text("pending".to_string()),
            Value::Text("shipped".to_string()),
        ];
        let (h, decoded) = write_text_and_read_back(&values);
        assert_eq!(h.physical_encoding, PHYSICAL_DICT);
        assert_eq!(decoded, values);
    }

    #[test]
    fn text_high_cardinality_picks_raw() {
        let values = vec![
            Value::Text("a".to_string()),
            Value::Text("b".to_string()),
            Value::Text("c".to_string()),
            Value::Text("d".to_string()),
        ];
        let (h, decoded) = write_text_and_read_back(&values);
        assert_eq!(h.physical_encoding, PHYSICAL_RAW);
        assert_eq!(decoded, values);
    }

    #[test]
    fn text_dict_round_trips_with_nulls() {
        // Many repeats so dict wins; NULLs are masked by the presence bitmap
        // and decode correctly regardless of the code stored for them.
        let values = vec![
            Value::Text("shipped".to_string()),
            Value::Null,
            Value::Text("shipped".to_string()),
            Value::Text("pending".to_string()),
            Value::Null,
            Value::Text("shipped".to_string()),
        ];
        let (h, decoded) = write_text_and_read_back(&values);
        assert_eq!(h.physical_encoding, PHYSICAL_DICT);
        assert!(h.has_nulls());
        assert_eq!(decoded, values);
    }

    #[test]
    fn text_single_distinct_value_repeated_picks_dict() {
        let values = vec![
            Value::Text("alice".to_string()),
            Value::Text("alice".to_string()),
            Value::Text("alice".to_string()),
        ];
        let (h, decoded) = write_text_and_read_back(&values);
        assert_eq!(h.physical_encoding, PHYSICAL_DICT);
        assert_eq!(decoded, values);
    }

    #[test]
    fn text_empty_and_all_null_pick_raw() {
        // Empty: raw payload is 0 bytes vs dict's 4-byte dict_count header.
        let (h, decoded) = write_text_and_read_back(&[]);
        assert_eq!(h.physical_encoding, PHYSICAL_RAW);
        assert!(decoded.is_empty());

        // All-NULL: same reason — raw skips the dict_count header.
        let (h, decoded) = write_text_and_read_back(&[Value::Null, Value::Null]);
        assert_eq!(h.physical_encoding, PHYSICAL_RAW);
        assert_eq!(decoded, vec![Value::Null, Value::Null]);
    }

    #[test]
    fn read_column_rejects_truncated_data() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("data.col");
        write_column(&path, ColumnType::Int, &[Value::Int(1), Value::Int(2)], &[]).unwrap();
        // Lop off the last value's bytes; the header still claims 2 rows.
        let mut bytes = fs::read(&path).unwrap();
        bytes.truncate(bytes.len() - 8);
        fs::write(&path, bytes).unwrap();
        assert!(
            read_column(&path)
                .unwrap_err()
                .to_string()
                .contains("truncated")
        );
    }
}
