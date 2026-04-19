use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::Error;

const METADATA_FILE: &str = "balik.meta";
const MAGIC: &str = "balik-db";
/// On-disk metadata schema version. Bump when the layout changes in a way
/// that older binaries can't read — independent from the crate version.
pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct Metadata {
    magic: String,
    format_version: u32,
    created: String,
}

/// Outcome of inspecting a candidate database directory.
#[derive(Debug)]
pub enum Status {
    /// Directory has no `balik.meta` file.
    Missing,
    /// `balik.meta` exists but could not be parsed.
    Unreadable,
    /// Parsed successfully but magic marker does not match.
    WrongMagic,
    /// Valid balik metadata, but the format version is newer than this binary supports.
    TooNew { found: u32, supported: u32 },
    /// Valid, readable, compatible database.
    Ok {
        format_version: u32,
        created: String,
    },
}

pub fn status(path: &Path) -> Status {
    let meta_path = path.join(METADATA_FILE);
    let Ok(content) = fs::read_to_string(&meta_path) else {
        return Status::Missing;
    };
    let Ok(meta) = toml::from_str::<Metadata>(&content) else {
        return Status::Unreadable;
    };
    if meta.magic != MAGIC {
        return Status::WrongMagic;
    }
    if meta.format_version > FORMAT_VERSION {
        return Status::TooNew {
            found: meta.format_version,
            supported: FORMAT_VERSION,
        };
    }
    Status::Ok {
        format_version: meta.format_version,
        created: meta.created,
    }
}

pub fn initialize(path: &Path) -> Result<(), Error> {
    tracing::info!(path = %path.display(), "initializing database");

    if path.exists() {
        match status(path) {
            Status::Ok { .. } | Status::TooNew { .. } => {
                return Err(Error(format!(
                    "directory '{}' is already an initialized balik database",
                    path.display()
                )));
            }
            Status::Missing | Status::Unreadable | Status::WrongMagic => {
                return Err(Error(format!(
                    "path '{}' already exists and is not a balik database — refusing to overwrite",
                    path.display()
                )));
            }
        }
    }

    tracing::debug!(path = %path.display(), "creating database directory");
    fs::create_dir_all(path).map_err(|e| Error(format!("failed to create directory: {e}")))?;

    let meta = Metadata {
        magic: MAGIC.to_string(),
        format_version: FORMAT_VERSION,
        created: timestamp(),
    };
    let serialized =
        toml::to_string(&meta).map_err(|e| Error(format!("failed to serialize metadata: {e}")))?;

    let meta_path = path.join(METADATA_FILE);
    tracing::debug!(path = %meta_path.display(), "writing metadata file");
    fs::write(&meta_path, serialized)
        .map_err(|e| Error(format!("failed to write metadata: {e}")))?;

    tracing::info!(path = %path.display(), "database initialized");
    Ok(())
}

/// Cheap UTC timestamp without pulling in chrono.
/// TODO: replace later
fn timestamp() -> String {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("unix:{}", dur.as_secs())
}

#[cfg(test)]
mod tests {
    use super::{METADATA_FILE, Status, initialize, status};

    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn init_creates_directory_structure() {
        // Given
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("testdb");

        // When
        let result = initialize(&db_path);

        // Then
        assert!(result.is_ok());

        assert!(db_path.exists());
        assert!(db_path.join(METADATA_FILE).exists());
    }

    #[test]
    fn init_meta_contains_format_version() {
        // Given
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("testdb");

        // When
        let result = initialize(&db_path);

        // Then
        assert!(result.is_ok());

        let meta = fs::read_to_string(db_path.join(METADATA_FILE)).unwrap();
        assert!(meta.contains(&format!("format_version = {}", super::FORMAT_VERSION)));
    }

    #[test]
    fn future_format_version_is_rejected() {
        // Given: a meta file claiming a format version newer than this binary
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("testdb");
        fs::create_dir(&db_path).unwrap();
        let future = super::FORMAT_VERSION + 1;
        fs::write(
            db_path.join(METADATA_FILE),
            format!("magic = \"balik-db\"\nformat_version = {future}\ncreated = \"unix:0\"\n"),
        )
        .unwrap();

        // Then
        assert!(matches!(
            status(&db_path),
            Status::TooNew { found, supported }
                if found == future && supported == super::FORMAT_VERSION
        ));
    }

    #[test]
    fn init_refuses_existing_database() {
        // Given
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("testdb");

        initialize(&db_path).unwrap();

        // When
        let result = initialize(&db_path);

        // Then
        assert!(result.is_err());

        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("already an initialized")
        );
    }

    #[test]
    fn stray_meta_file_without_magic_is_not_recognized() {
        // Given: a directory with a balik.meta file that lacks required fields
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("testdb");
        fs::create_dir(&db_path).unwrap();
        fs::write(db_path.join(METADATA_FILE), "version = \"0.0.0\"\n").unwrap();

        // Then: cannot be parsed into our Metadata shape → Unreadable
        assert!(matches!(status(&db_path), Status::Unreadable));
    }

    #[test]
    fn meta_file_with_wrong_magic_is_detected() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("testdb");
        fs::create_dir(&db_path).unwrap();
        fs::write(
            db_path.join(METADATA_FILE),
            "magic = \"sqlite\"\nformat_version = 1\ncreated = \"unix:0\"\n",
        )
        .unwrap();

        assert!(matches!(status(&db_path), Status::WrongMagic));
    }

    #[test]
    fn initialized_db_reports_ok_status() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("testdb");
        initialize(&db_path).unwrap();

        assert!(matches!(status(&db_path), Status::Ok { .. }));
    }

    #[test]
    fn init_refuses_existing_non_db_directory() {
        // Given
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("testdb");
        fs::create_dir(&db_path).unwrap();

        // When
        let result = initialize(&db_path);

        // Then
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("refusing to overwrite")
        );
    }
}
