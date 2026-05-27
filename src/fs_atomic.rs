//! Atomic whole-file replacement shared by the on-disk formats.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::Error;

/// Atomically replace `path` with `bytes`: write a sibling `<path>.tmp`, fsync
/// it, then rename it over `path`. A crash leaves either the complete old file
/// or the complete new one — never a half-written file.
///
/// `context` names the file for error messages (e.g. `"catalog"`,
/// `"column file"`), so a failure reads `rename catalog: <io error>`.
///
/// On success the rename consumes the temp file. On any failure the temp file
/// is removed (best effort) so a stale `<path>.tmp` is not left behind.
///
/// The parent directory is not fsynced, so a power loss can still lose the
/// rename even though the file's contents are durable; that is an accepted gap
/// for a toy database.
pub fn write(path: &Path, bytes: &[u8], context: &str) -> Result<(), Error> {
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);

    let result = (|| {
        fs::write(&tmp, bytes).map_err(|e| Error::io(&format!("write {context} temp file"), e))?;
        fs::File::open(&tmp)
            .and_then(|f| f.sync_all())
            .map_err(|e| Error::io(&format!("fsync {context} temp file"), e))?;
        fs::rename(&tmp, path).map_err(|e| Error::io(&format!("rename {context}"), e))
    })();

    if result.is_err() {
        // The rename never happened (or failed), so the temp file may linger.
        // Removing it is best effort: it may not exist, and a failure here
        // must not mask the original error.
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn write_replaces_contents_and_leaves_no_tmp() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("f.bin");
        write(&path, b"hello", "test file").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"hello");
        // A second write overwrites the first.
        write(&path, b"world!!", "test file").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"world!!");
        assert!(!tmp.path().join("f.bin.tmp").exists());
    }

    #[test]
    fn failed_write_removes_temp_file() {
        let tmp = TempDir::new().unwrap();
        // Make the target a directory so the final rename(file -> dir) fails
        // after the temp file has already been created.
        let path = tmp.path().join("target");
        fs::create_dir(&path).unwrap();

        let err = write(&path, b"data", "test file").unwrap_err();
        assert!(err.to_string().contains("rename"), "got: {err}");
        assert!(
            !tmp.path().join("target.tmp").exists(),
            "temp file should have been cleaned up after the failed rename"
        );
    }
}
