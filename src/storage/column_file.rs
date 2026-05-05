//! On-disk format for column data files (`.col`).
//!
//! Each `.col` file holds the values of one column for one row group.
//! Stage 1 only materializes the header; the data area lands in Stage 2.
//!
//! Byte layout (all multi-byte integers little-endian):
//!
//! ```text
//! offset  size  field              notes
//! ------  ----  -----              -----
//! 0       8     magic              ASCII "BALIKCOL"
//! 8       4     format_version     u32 = 1
//! 12      1     logical_type       0=INT, 1=TEXT
//! 13      1     physical_encoding  0=raw (only encoding defined for now)
//! 14      1     flags              bit 0 = has_nulls; bits 1-7 reserved
//! 15      1     reserved
//! 16      4     row_count          u32 — rows in this file, including NULLs
//! 20      4     null_count         u32 — number of NULL rows
//! 24      16    min                zeroed until row group seals
//! 40      16    max                zeroed until row group seals
//! 56      ...   data area          Stage 2+
//! ```
//!
//! The 56-byte header is 8-byte aligned so the future data area can be
//! aligned for SIMD or memcpy paths.
//!
//! ## NULL handling (planned for Stage 2)
//!
//! When `flags.has_nulls = 1`, the data area begins with a NULL bitmap
//! sized `ceil(row_count / 8)` bytes (1 = present, 0 = NULL), followed by
//! the per-encoding values. When `flags.has_nulls = 0`, no bitmap is
//! emitted even on nullable columns — saves a decode pass when no NULLs
//! have been written. `null_count` always reflects truth and can be used
//! for fast pruning regardless of the flag.

use std::fs;
use std::path::Path;

use crate::catalog::schema::ColumnType;
use crate::error::Error;

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
// Stage 2: read_header returns this; Stage 1 only constructs it inside tests
// so dead-code analysis doesn't see a production caller.
#[allow(dead_code)]
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
    // Stage 2: the scan path branches on this. Stage 1 only checks it in
    // tests pinning the `flags` byte layout.
    #[allow(dead_code)]
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

// Stage 2: called from read_header on the scan path; Stage 1 has no caller.
#[allow(dead_code)]
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

/// Parse the header of an existing `.col` file. Used by tests today; will
/// be the entry point for scan/skip-pruning in Stage 2.
#[allow(dead_code)]
pub fn read_header(path: &Path) -> Result<Header, Error> {
    let bytes = fs::read(path).map_err(|e| Error::io("read column file", e))?;
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
}
