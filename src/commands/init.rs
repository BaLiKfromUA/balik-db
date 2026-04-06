use std::fmt;
use std::fs;
use std::path::Path;

pub fn run(path: &Path) -> Result<(), Error> {
    if (path.exists()) {
        return Err(Error(format!(
            "directory '{}' is already an initialized balik database",
            path.display()
        )));
    }

    fs::create_dir_all(path).map_err(|e| Error(format!("failed to create directory: {e}")))?;
    println!("Initialized empty balik database at '{}'", path.display());
    Ok(())
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
    use super::run;
    use tempfile::TempDir;

    #[test]
    fn init_creates_directory_structure() {
        // Given
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("testdb");

        // When
        let result = run(&db_path);

        // Then
        assert!(result.is_ok());
        assert!(db_path.exists());
    }

    #[test]
    fn init_refuses_existing_database() {
        // Given
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("testdb");

        run(&db_path).unwrap();

        // When
        let result = run(&db_path);

        // Then
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("already an initialized")
        );
    }
}
