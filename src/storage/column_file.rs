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
//! 24      16    min                reserved, zeroed
//! 40      16    max                reserved, zeroed
//! 56      ...   data area          presence bitmap (if any) + encoded values
//! ```
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

use std::fs;
use std::path::Path;

use crate::catalog::schema::ColumnType;
use crate::error::Error;
use crate::fs_atomic;
use crate::storage::Value;

const HEADER_SIZE: usize = 56;
const MAGIC: &[u8; 8] = b"BALIKCOL";
const FORMAT_VERSION: u32 = 1;

// Stage 2: read by the scan path; Stage 1 just zeros the flags byte.
#[allow(dead_code)]
const FLAG_HAS_NULLS: u8 = 0b0000_0001;

const LOGICAL_INT: u8 = 0;
const LOGICAL_TEXT: u8 = 1;
const PHYSICAL_RAW: u8 = 0;

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
        other => Err(Error(format!("unknown logical type tag {other}"))),
    }
}

/// Build a 56-byte empty header for a new `.col` file of the given type.
fn empty_header(ty: ColumnType) -> [u8; HEADER_SIZE] {
    let mut buf = [0u8; HEADER_SIZE];
    buf[0..8].copy_from_slice(MAGIC);
    buf[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    buf[12] = logical_type_byte(ty);
    buf[13] = PHYSICAL_RAW;
    // [14]      flags          = 0 (no nulls yet)
    // [15]      reserved       = 0
    // [16..20]  row_count      = 0
    // [20..24]  null_count     = 0
    // [24..40]  min            = 0
    // [40..56]  max            = 0
    buf
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
        return Err(Error(format!(
            "column file '{}' shorter than {HEADER_SIZE}-byte header",
            path.display()
        )));
    }
    if &bytes[0..8] != MAGIC {
        return Err(Error(format!(
            "column file '{}' has bad magic",
            path.display()
        )));
    }
    let format_version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    if format_version > FORMAT_VERSION {
        return Err(Error(format!(
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

/// Read and parse just the header of an existing `.col` file.
#[allow(dead_code)]
pub fn read_header(path: &Path) -> Result<Header, Error> {
    let bytes = fs::read(path).map_err(|e| Error::io("read column file", e))?;
    parse_header(&bytes, path)
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
            Value::Text(_) => return Err(Error("INT column received a TEXT value".to_string())),
        };
        out.extend_from_slice(&n.to_le_bytes());
    }
    Ok(out)
}

fn encode_text_raw(values: &[Value]) -> Result<Vec<u8>, Error> {
    let mut offsets = Vec::with_capacity(values.len() * 4);
    let mut blob = Vec::new();
    for v in values {
        match v {
            Value::Text(s) => blob.extend_from_slice(s.as_bytes()),
            Value::Null => {}
            Value::Int(_) => return Err(Error("TEXT column received an INT value".to_string())),
        }
        let end = u32::try_from(blob.len())
            .map_err(|_| Error("TEXT column exceeds the 4 GiB limit".to_string()))?;
        offsets.extend_from_slice(&end.to_le_bytes());
    }
    offsets.extend_from_slice(&blob);
    Ok(offsets)
}

/// Build the whole byte image (header + data area) for a column of `values`.
fn encode_column(ty: ColumnType, values: &[Value]) -> Result<Vec<u8>, Error> {
    let row_count = u32::try_from(values.len())
        .map_err(|_| Error("row group exceeds the u32 row-count limit".to_string()))?;
    let null_count = values.iter().filter(|v| matches!(v, Value::Null)).count() as u32;
    let has_nulls = null_count > 0;

    let mut buf = empty_header(ty).to_vec();
    if has_nulls {
        buf[14] = FLAG_HAS_NULLS;
    }
    buf[16..20].copy_from_slice(&row_count.to_le_bytes());
    buf[20..24].copy_from_slice(&null_count.to_le_bytes());

    if has_nulls {
        buf.extend_from_slice(&build_present_bitmap(values));
    }
    let data = match ty {
        ColumnType::Int => encode_int_raw(values)?,
        ColumnType::Text => encode_text_raw(values)?,
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
        return Err(Error(format!(
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
        return Err(Error(format!(
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
            return Err(Error(format!(
                "column file '{}' has an out-of-range TEXT offset",
                path.display()
            )));
        }
        if is_present(present, i) {
            let s = std::str::from_utf8(&blob[start..end])
                .map_err(|_| Error(format!("column file '{}' has invalid UTF-8", path.display())))?
                .to_string();
            out.push(Value::Text(s));
        } else {
            out.push(Value::Null);
        }
        start = end;
    }
    Ok(out)
}

/// Encode `values` for a column of type `ty` and replace the `.col` file at
/// `path` atomically: write a sibling temp file, fsync it, then rename it
/// into place so a crash leaves either the old image or the new one.
pub fn write_column(path: &Path, ty: ColumnType, values: &[Value]) -> Result<(), Error> {
    tracing::debug!(path = %path.display(), rows = values.len(), "writing column file");
    let bytes = encode_column(ty, values)?;
    fs_atomic::write(path, &bytes, "column file")
}

/// Read and decode every value in a `.col` file, in row order. NULL rows
/// decode to `Value::Null`. Corrupt or truncated files return `Err` so the
/// scan path never yields garbage.
pub fn read_column(path: &Path) -> Result<Vec<Value>, Error> {
    let bytes = fs::read(path).map_err(|e| Error::io("read column file", e))?;
    let header = parse_header(&bytes, path)?;
    let row_count = header.row_count as usize;
    let data = &bytes[HEADER_SIZE..];

    let (present, data) = if header.has_nulls() {
        let bm_len = row_count.div_ceil(8);
        if data.len() < bm_len {
            return Err(Error(format!(
                "column file '{}' presence bitmap is truncated",
                path.display()
            )));
        }
        (Some(&data[..bm_len]), &data[bm_len..])
    } else {
        (None, data)
    };

    if header.physical_encoding != PHYSICAL_RAW {
        return Err(Error(format!(
            "column file '{}' uses unsupported physical encoding {}",
            path.display(),
            header.physical_encoding
        )));
    }
    match header.logical_type {
        ColumnType::Int => decode_int_raw(data, row_count, present, path),
        ColumnType::Text => decode_text_raw(data, row_count, present, path),
    }
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
    fn empty_header_zeroes_counts_and_stats() {
        let buf = empty_header(ColumnType::Int);
        let row_count = u32::from_le_bytes(buf[16..20].try_into().unwrap());
        let null_count = u32::from_le_bytes(buf[20..24].try_into().unwrap());
        assert_eq!(row_count, 0);
        assert_eq!(null_count, 0);
        assert!(buf[24..40].iter().all(|&b| b == 0), "min should be zeroed");
        assert!(buf[40..56].iter().all(|&b| b == 0), "max should be zeroed");
    }

    #[test]
    fn write_then_read_round_trips_int() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("id.col");
        write_empty(&path, ColumnType::Int).unwrap();
        let h = read_header(&path).unwrap();
        assert_eq!(h.format_version, FORMAT_VERSION);
        assert_eq!(h.logical_type, ColumnType::Int);
        assert_eq!(h.physical_encoding, PHYSICAL_RAW);
        assert_eq!(h.flags, 0);
        assert!(!h.has_nulls());
        assert_eq!(h.row_count, 0);
        assert_eq!(h.null_count, 0);
        assert_eq!(h.min, [0u8; 16]);
        assert_eq!(h.max, [0u8; 16]);
    }

    #[test]
    fn write_then_read_round_trips_text() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("name.col");
        write_empty(&path, ColumnType::Text).unwrap();
        let h = read_header(&path).unwrap();
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
    fn read_header_rejects_short_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("bad.col");
        fs::write(&path, b"too short").unwrap();
        let err = read_header(&path).unwrap_err();
        assert!(err.to_string().contains("shorter than"));
    }

    #[test]
    fn read_header_rejects_bad_magic() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("bad.col");
        let mut bytes = empty_header(ColumnType::Int);
        bytes[0..8].copy_from_slice(b"NOPENOPE");
        fs::write(&path, bytes).unwrap();
        let err = read_header(&path).unwrap_err();
        assert!(err.to_string().contains("bad magic"));
    }

    #[test]
    fn read_header_rejects_too_new_format() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("bad.col");
        let mut bytes = empty_header(ColumnType::Int);
        let future = FORMAT_VERSION + 1;
        bytes[8..12].copy_from_slice(&future.to_le_bytes());
        fs::write(&path, bytes).unwrap();
        let err = read_header(&path).unwrap_err();
        assert!(err.to_string().contains("format_version"));
    }

    #[test]
    fn read_header_rejects_unknown_logical_type() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("bad.col");
        let mut bytes = empty_header(ColumnType::Int);
        bytes[12] = 99;
        fs::write(&path, bytes).unwrap();
        let err = read_header(&path).unwrap_err();
        assert!(err.to_string().contains("unknown logical type"));
    }

    fn roundtrip(ty: ColumnType, values: &[Value]) -> Vec<Value> {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("data.col");
        write_column(&path, ty, values).unwrap();
        read_column(&path).unwrap()
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
        write_column(&path, ColumnType::Int, &values).unwrap();
        let h = read_header(&path).unwrap();
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
        write_column(&path, ColumnType::Int, &[Value::Int(1), Value::Int(2), Value::Int(3)])
            .unwrap();
        assert_eq!(fs::read(&path).unwrap().len(), HEADER_SIZE + 24);
    }

    #[test]
    fn write_column_rejects_value_of_wrong_type() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("data.col");
        let err = write_column(&path, ColumnType::Int, &[Value::Text("x".to_string())]).unwrap_err();
        assert!(err.to_string().contains("INT column received a TEXT"));
    }

    #[test]
    fn read_column_rejects_truncated_data() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("data.col");
        write_column(&path, ColumnType::Int, &[Value::Int(1), Value::Int(2)]).unwrap();
        // Lop off the last value's bytes; the header still claims 2 rows.
        let mut bytes = fs::read(&path).unwrap();
        bytes.truncate(bytes.len() - 8);
        fs::write(&path, bytes).unwrap();
        assert!(read_column(&path).unwrap_err().to_string().contains("truncated"));
    }
}
