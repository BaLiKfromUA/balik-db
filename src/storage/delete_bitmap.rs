//! On-disk format for per-row-group delete bitmaps (`deletes.bm`).
//!
//! One bitmap file lives in each row-group directory next to the `.col`
//! files. Bit `i` of the bitmap corresponds to the row at offset `i` within
//! the row group: `1` means deleted, `0` means live. The file is
//! materialized with all bits zero at `create_table` time, pre-sized to the
//! table's `row_group_size`, and never resized — `delete` / `update` flip
//! bits in place.
//!
//! Why a separate file (not a section of each `.col`):
//!
//! - Deletes are per-row across all columns simultaneously, so a single
//!   shared bitmap is the natural place to put the bits.
//! - `.col` headers are write-once at seal time; the delete bitmap is
//!   mutable. Mixing them would force header rewrites on every delete.
//!
//! Byte layout (all multi-byte integers little-endian):
//!
//! ```text
//! offset  size  field             notes
//! ------  ----  -----             -----
//! 0       8     magic             ASCII "BALIKDEL"
//! 8       4     format_version    u32 = 1
//! 12      4     deleted_count     u32 — number of set bits (live readers can
//!                                       skip the bitmap when this is 0)
//! 16      8     reserved          zeroed (forward compat)
//! 24      ...   bitmap data       ceil(row_group_size / 8) bytes; bit i =
//!                                 row at offset i (1 = deleted, 0 = live)
//! ```
//!
//! The 24-byte header is 8-byte aligned. `deleted_count` is denormalized
//! (derivable by counting bits) for the same reason `null_count` is in the
//! `.col` header — it lets readers skip both the bitmap and the row group
//! without scanning when nothing is deleted (or when everything is).

use std::fs;
use std::path::Path;

use crate::error::Error;

const HEADER_SIZE: usize = 24;
const MAGIC: &[u8; 8] = b"BALIKDEL";
const FORMAT_VERSION: u32 = 1;

/// File name within a row-group directory.
pub const FILE_NAME: &str = "deletes.bm";

/// Parsed view of a `deletes.bm` header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub format_version: u32,
    pub deleted_count: u32,
}

/// Bytes of bitmap data needed to track `row_group_size` rows.
fn bitmap_bytes(row_group_size: u32) -> usize {
    row_group_size.div_ceil(8) as usize
}

/// Build the full byte image of an empty bitmap file: header + zeroed
/// bitmap sized to `row_group_size`.
fn empty_file(row_group_size: u32) -> Vec<u8> {
    let mut buf = vec![0u8; HEADER_SIZE + bitmap_bytes(row_group_size)];
    buf[0..8].copy_from_slice(MAGIC);
    buf[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    // [12..16]  deleted_count = 0
    // [16..24]  reserved      = 0
    // [24..]    bitmap        = 0 (all live)
    buf
}

/// Create a new `deletes.bm` file at `path` with all bits zero.
pub fn write_empty(path: &Path, row_group_size: u32) -> Result<(), Error> {
    tracing::debug!(
        path = %path.display(),
        row_group_size,
        "writing empty delete bitmap"
    );
    fs::write(path, empty_file(row_group_size)).map_err(|e| Error::io("write delete bitmap", e))
}

/// Parse a `deletes.bm` header from the leading bytes of a file image,
/// validating magic and format version.
fn parse_header(bytes: &[u8], path: &Path) -> Result<Header, Error> {
    if bytes.len() < HEADER_SIZE {
        return Err(Error::corrupt(format!(
            "delete bitmap '{}' shorter than {HEADER_SIZE}-byte header",
            path.display()
        )));
    }
    if &bytes[0..8] != MAGIC {
        return Err(Error::corrupt(format!(
            "delete bitmap '{}' has bad magic",
            path.display()
        )));
    }
    let format_version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    if format_version > FORMAT_VERSION {
        return Err(Error::corrupt(format!(
            "delete bitmap '{}' uses format_version {format_version}, this binary supports {FORMAT_VERSION}",
            path.display()
        )));
    }
    let deleted_count = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
    Ok(Header {
        format_version,
        deleted_count,
    })
}

/// Read the whole bitmap file and return `(header, bitmap_bytes)`. The
/// bitmap slice covers the trailing region after the 24-byte header and is
/// sized to the row group's `row_group_size` at create time.
pub fn load(path: &Path) -> Result<(Header, Vec<u8>), Error> {
    let bytes = fs::read(path).map_err(|e| Error::io("read delete bitmap", e))?;
    let header = parse_header(&bytes, path)?;
    Ok((header, bytes[HEADER_SIZE..].to_vec()))
}

/// True if `offset` is marked deleted in `bitmap`. Out-of-range offsets are
/// treated as live, since the bitmap is pre-sized to `row_group_size`.
pub fn is_deleted(bitmap: &[u8], offset: usize) -> bool {
    let byte = offset / 8;
    byte < bitmap.len() && bitmap[byte] & (1 << (offset % 8)) != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn bitmap_bytes_rounds_up() {
        assert_eq!(bitmap_bytes(0), 0);
        assert_eq!(bitmap_bytes(1), 1);
        assert_eq!(bitmap_bytes(8), 1);
        assert_eq!(bitmap_bytes(9), 2);
        assert_eq!(bitmap_bytes(8192), 1024);
    }

    #[test]
    fn empty_file_size_matches_row_group_size() {
        assert_eq!(empty_file(8192).len(), HEADER_SIZE + 1024);
        assert_eq!(empty_file(0).len(), HEADER_SIZE);
        assert_eq!(empty_file(1).len(), HEADER_SIZE + 1);
    }

    #[test]
    fn empty_file_has_magic_and_version() {
        let buf = empty_file(8192);
        assert_eq!(&buf[0..8], MAGIC);
        let ver = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        assert_eq!(ver, FORMAT_VERSION);
    }

    #[test]
    fn empty_file_zeroes_counts_and_bitmap() {
        let buf = empty_file(8192);
        let deleted = u32::from_le_bytes(buf[12..16].try_into().unwrap());
        assert_eq!(deleted, 0);
        assert!(
            buf[16..24].iter().all(|&b| b == 0),
            "reserved region should be zeroed"
        );
        assert!(
            buf[24..].iter().all(|&b| b == 0),
            "bitmap should be all-live"
        );
    }

    #[test]
    fn write_then_read_round_trips() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("deletes.bm");
        write_empty(&path, 8192).unwrap();
        let (h, _) = load(&path).unwrap();
        assert_eq!(h.format_version, FORMAT_VERSION);
        assert_eq!(h.deleted_count, 0);
        let bytes = fs::read(&path).unwrap();
        assert_eq!(bytes.len(), HEADER_SIZE + 1024);
    }

    #[test]
    fn load_rejects_short_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("bad.bm");
        fs::write(&path, b"too short").unwrap();
        let err = load(&path).unwrap_err();
        assert!(err.to_string().contains("shorter than"));
    }

    #[test]
    fn load_rejects_bad_magic() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("bad.bm");
        let mut bytes = empty_file(8192);
        bytes[0..8].copy_from_slice(b"NOPENOPE");
        fs::write(&path, bytes).unwrap();
        let err = load(&path).unwrap_err();
        assert!(err.to_string().contains("bad magic"));
    }

    #[test]
    fn load_rejects_too_new_format() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("bad.bm");
        let mut bytes = empty_file(8192);
        let future = FORMAT_VERSION + 1;
        bytes[8..12].copy_from_slice(&future.to_le_bytes());
        fs::write(&path, bytes).unwrap();
        let err = load(&path).unwrap_err();
        assert!(err.to_string().contains("format_version"));
    }

    #[test]
    fn load_returns_header_and_bitmap_sized_to_row_group() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("deletes.bm");
        write_empty(&path, 17).unwrap(); // 17 rows → 3 bitmap bytes
        let (header, bitmap) = load(&path).unwrap();
        assert_eq!(header.deleted_count, 0);
        assert_eq!(bitmap.len(), 3);
        assert!(bitmap.iter().all(|&b| b == 0));
    }

    #[test]
    fn is_deleted_reads_lsb_first_within_bytes() {
        // bit 0 + bit 9 set; LSB-first within each byte.
        let bitmap = vec![0b0000_0001, 0b0000_0010];
        assert!(is_deleted(&bitmap, 0));
        assert!(!is_deleted(&bitmap, 1));
        assert!(!is_deleted(&bitmap, 8));
        assert!(is_deleted(&bitmap, 9));
    }

    #[test]
    fn is_deleted_treats_out_of_range_as_live() {
        let bitmap = vec![0xFF];
        assert!(is_deleted(&bitmap, 0));
        assert!(!is_deleted(&bitmap, 8)); // past the end → live
        assert!(!is_deleted(&[], 0)); // empty bitmap (rgs=0) → never deleted
    }
}
