use std::io::Write;
use std::{fmt, path::Path};

use crate::catalog;

pub fn run(path: &Path) -> Result<(), Error> {
    let mut out = std::io::stdout();
    diagnose(&mut out, path)
}

fn diagnose(out: &mut impl Write, path: &Path) -> Result<(), Error> {
    writeln!(out, "balik-db doctor").unwrap();
    writeln!(out, "===============").unwrap();

    print_version(out);
    print_os_info(out);
    check_database(out, path);

    Ok(())
}

fn print_version(out: &mut impl Write) {
    writeln!(out, "[ok] balik-db version: {}", env!("CARGO_PKG_VERSION")).unwrap();
}

fn print_os_info(out: &mut impl Write) {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    writeln!(out, "[ok] OS: {os} ({arch})").unwrap();
}

fn check_database(out: &mut impl Write, path: &Path) {
    if !path.exists() {
        writeln!(out, "[!!] database not found at '{}'", path.display()).unwrap();
        writeln!(out, "     run `balik-cli init --db {}` to create one", path.display()).unwrap();
        return;
    }

    if !catalog::is_initialized(path) {
        writeln!(
            out,
            "[!!] '{}' exists but is not an initialized balik database",
            path.display()
        )
        .unwrap();
        return;
    }

    writeln!(out, "[ok] database found at '{}'", path.display()).unwrap();
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
    use super::diagnose;
    use tempfile::TempDir;

    fn run_doctor(db_path: &std::path::Path) -> String {
        let mut buf = Vec::new();
        diagnose(&mut buf, db_path).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn doctor_reports_missing_database() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("nonexistent");

        let output = run_doctor(&db_path);

        assert!(output.contains("balik-db doctor"));
        assert!(output.contains("[ok] balik-db version:"));
        assert!(output.contains("[ok] OS:"));
        assert!(output.contains("[!!] database not found"));
        assert!(output.contains("run `balik-cli init --db"));
    }

    #[test]
    fn doctor_reports_initialized_database() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("testdb");

        crate::catalog::initialize(&db_path).unwrap();

        let output = run_doctor(&db_path);

        assert!(output.contains("[ok] database found at"));
        assert!(!output.contains("[!!]"));
    }

    #[test]
    fn doctor_reports_uninitialized_directory() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("testdb");
        std::fs::create_dir(&db_path).unwrap();

        let output = run_doctor(&db_path);

        assert!(output.contains("[!!]"));
        assert!(output.contains("exists but is not an initialized balik database"));
    }
}
