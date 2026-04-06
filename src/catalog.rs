use std::fmt;
use std::fs;
use std::path::Path;

const METADATA_FILE: &str = "balik.meta";

pub fn is_initialized(path: &Path) -> bool {
    path.join(METADATA_FILE).exists()
}

pub fn initialize(path: &Path) -> Result<(), Error> {
    if path.exists() {
        if is_initialized(path) {
            return Err(Error(format!(
                "directory '{}' is already an initialized balik database",
                path.display()
            )));
        }
        return Err(Error(format!(
            "path '{}' already exists and is not a balik database — refusing to overwrite",
            path.display()
        )));
    }

    fs::create_dir_all(path).map_err(|e| Error(format!("failed to create directory: {e}")))?;

    let meta = format!(
        "version = \"{}\"\ncreated = \"{}\"\n",
        env!("CARGO_PKG_VERSION"),
        timestamp(),
    );
    fs::write(path.join(METADATA_FILE), meta)
        .map_err(|e| Error(format!("failed to write metadata: {e}")))?;

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

#[derive(Debug)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::METADATA_FILE;
    use super::initialize;

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
    fn init_meta_contains_version() {
        // Given
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("testdb");

        // When
        let result = initialize(&db_path);

        // Then
        assert!(result.is_ok());

        let meta = fs::read_to_string(db_path.join(METADATA_FILE)).unwrap();
        assert!(meta.contains(&format!("version = \"{}\"", env!("CARGO_PKG_VERSION"))));
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
