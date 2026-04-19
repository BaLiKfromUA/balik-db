use std::path::Path;

use crate::catalog::metadata;
use crate::error::Error;

pub fn run(path: &Path) -> Result<(), Error> {
    metadata::initialize(path)?;
    println!("Initialized empty balik database at '{}'", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::run;
    use tempfile::TempDir;

    #[test]
    fn init_creates_directory() {
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
