//! Single-file integrity wrapper for our TOML metadata files.
//!
//! `wrap` prepends a `# crc32 = 0xXXXXXXXX\n` line to the body. The result
//! is still valid TOML (the leading line is a comment), and a verified file
//! looks like:
//!
//! ```toml
//! # crc32 = 0xdeadbeef
//! format_version = 1
//! ...
//! ```
//!
//! `verify` strips and validates the leading line, returning the body for
//! the caller to parse. Storing the algorithm name (`crc32`) inside the
//! line means a future migration to a different hash can be detected per
//! file without bumping the file's `format_version`.
//!
//! ## Why CRC32-IEEE
//!
//! Same polynomial used by ZIP, ext4, BTRFS, and most DB on-disk formats.
//! It catches all 1- and 2-bit errors and all burst errors up to 32 bits;
//! random corruption escapes with probability ~2^-32. It is **not** a
//! cryptographic hash — adversarial collisions are trivial to construct.
//! We use it for bit-rot and accidental-tamper detection, not security.
//!
//! The bitwise implementation keeps the dep tree empty. For files at our
//! scale (KB at most) the table-driven speedup isn't worth the code.

use crate::error::Error;

const POLYNOMIAL: u32 = 0xEDB8_8320;
const PREFIX: &str = "# crc32 = 0x";

/// CRC32-IEEE checksum of the input bytes.
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ POLYNOMIAL
            } else {
                crc >> 1
            };
        }
    }
    crc ^ 0xFFFF_FFFF
}

/// Wrap `body` with a leading `# crc32 = 0xXXXXXXXX\n` line. The returned
/// bytes are what gets written to disk.
pub fn wrap(body: &[u8]) -> Vec<u8> {
    let crc = crc32(body);
    let header = format!("{PREFIX}{crc:08x}\n");
    let mut out = Vec::with_capacity(header.len() + body.len());
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(body);
    out
}

/// Validate the leading checksum line and return a slice over the body.
/// Errors if the line is missing, malformed, or the computed CRC doesn't
/// match the stored value.
pub fn verify(bytes: &[u8]) -> Result<&[u8], Error> {
    let nl = bytes
        .iter()
        .position(|&b| b == b'\n')
        .ok_or_else(|| Error("checksum line missing (no newline found)".to_string()))?;
    let line = std::str::from_utf8(&bytes[..nl])
        .map_err(|_| Error("checksum line is not valid UTF-8".to_string()))?;
    let hex = line
        .strip_prefix(PREFIX)
        .ok_or_else(|| Error(format!("checksum line missing or malformed: '{line}'")))?;
    let stored = u32::from_str_radix(hex, 16)
        .map_err(|_| Error(format!("checksum value '{hex}' is not hex")))?;
    let body = &bytes[nl + 1..];
    let computed = crc32(body);
    if stored != computed {
        return Err(Error(format!(
            "checksum mismatch: stored 0x{stored:08x}, computed 0x{computed:08x}"
        )));
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_known_vectors() {
        // Standard CRC32-IEEE test vector.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        // Empty input: initial 0xFFFFFFFF XOR final 0xFFFFFFFF = 0.
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn round_trip_returns_body() {
        let body = b"format_version = 1\nname = \"users\"\n";
        let wrapped = wrap(body);
        let body_out = verify(&wrapped).unwrap();
        assert_eq!(body_out, body);
    }

    #[test]
    fn wrap_starts_with_comment_line() {
        let wrapped = wrap(b"x = 1\n");
        assert!(wrapped.starts_with(b"# crc32 = 0x"));
        assert!(wrapped.contains(&b'\n'));
    }

    #[test]
    fn detects_body_bit_flip() {
        let body = b"format_version = 1\n";
        let mut wrapped = wrap(body);
        let target = wrapped.len() - 4;
        wrapped[target] ^= 0x01;
        let err = verify(&wrapped).unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"));
    }

    #[test]
    fn detects_checksum_value_tamper() {
        let mut wrapped = wrap(b"a = 1\n");
        let pos = wrapped.iter().position(|&b| b == b'x').unwrap() + 1;
        wrapped[pos] = if wrapped[pos] == b'0' { b'1' } else { b'0' };
        let err = verify(&wrapped).unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"));
    }

    #[test]
    fn rejects_missing_header() {
        let err = verify(b"format_version = 1\n").unwrap_err();
        assert!(err.to_string().contains("checksum line"), "got: {err}");
    }

    #[test]
    fn rejects_malformed_hex() {
        let err = verify(b"# crc32 = 0xZZZZZZZZ\nfoo\n").unwrap_err();
        assert!(err.to_string().contains("not hex"), "got: {err}");
    }

    #[test]
    fn rejects_no_newline_at_all() {
        let err = verify(b"# crc32 = 0xdeadbeef").unwrap_err();
        assert!(err.to_string().contains("no newline"), "got: {err}");
    }
}
