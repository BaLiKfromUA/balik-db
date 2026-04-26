use std::io::{self, Write};
use std::path::Path;

use crate::catalog::metadata::{self, Status};
use crate::error::Error;
use crate::storage::Storage;
use crate::storage::column_store::ColumnStore;

pub fn run(path: &Path) -> Result<(), Error> {
    let mut out = std::io::stdout();
    let all_ok = diagnose(&mut out, path)?;
    if !all_ok {
        return Err(Error("one or more diagnostic checks failed".to_string()));
    }
    Ok(())
}

fn diagnose(out: &mut impl Write, path: &Path) -> io::Result<bool> {
    writeln!(out, "balik-db doctor")?;
    writeln!(out, "===============")?;

    print_version(out)?;
    print_os_info(out)?;
    let db_ok = check_database(out, path)?;
    if !db_ok {
        return Ok(false);
    }
    let catalog_ok = check_catalog(out, path)?;
    Ok(db_ok && catalog_ok)
}

fn print_version(out: &mut impl Write) -> io::Result<()> {
    writeln!(out, "[ok] balik-db version: {}", env!("CARGO_PKG_VERSION"))?;
    writeln!(
        out,
        "[ok] supported format version: {}",
        metadata::FORMAT_VERSION
    )
}

fn print_os_info(out: &mut impl Write) -> io::Result<()> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    writeln!(out, "[ok] OS: {os} ({arch})")
}

fn check_database(out: &mut impl Write, path: &Path) -> io::Result<bool> {
    if !path.exists() {
        writeln!(out, "[!!] database not found at '{}'", path.display())?;
        writeln!(
            out,
            "     run `balik-cli init --db {}` to create one",
            path.display()
        )?;
        return Ok(false);
    }

    match metadata::status(path) {
        Status::Missing => {
            writeln!(
                out,
                "[!!] '{}' exists but has no balik.meta — not an initialized balik database",
                path.display()
            )?;
            Ok(false)
        }
        Status::Unreadable => {
            writeln!(
                out,
                "[!!] balik.meta at '{}' is corrupt or unreadable",
                path.display()
            )?;
            Ok(false)
        }
        Status::WrongMagic => {
            writeln!(out, "[!!] '{}' is not a balik database", path.display())?;
            Ok(false)
        }
        Status::TooNew { found, supported } => {
            writeln!(
                out,
                "[!!] database at '{}' uses format version {found}, this binary supports {supported}",
                path.display()
            )?;
            Ok(false)
        }
        Status::Ok {
            format_version,
            created,
        } => {
            writeln!(out, "[ok] database found at '{}'", path.display())?;
            writeln!(
                out,
                "     format version: {format_version}, created: {created}"
            )?;
            Ok(true)
        }
    }
}

fn check_catalog(out: &mut impl Write, db: &Path) -> io::Result<bool> {
    let store = match ColumnStore::open(db) {
        Ok(s) => s,
        Err(e) => {
            writeln!(out, "[!!] cannot open catalog: {e}")?;
            return Ok(false);
        }
    };
    let names = match store.list_tables() {
        Ok(n) => n,
        Err(e) => {
            writeln!(out, "[!!] cannot list tables: {e}")?;
            return Ok(false);
        }
    };
    writeln!(out, "[ok] catalog: {} tables", names.len())?;

    let mut all_ok = true;
    for name in &names {
        match store.describe_table(name) {
            Ok(desc) => {
                let cols = desc
                    .schema
                    .columns
                    .iter()
                    .map(|c| format!("{} {}", c.name, c.ty.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ");
                writeln!(
                    out,
                    "[ok] {name}: {} columns ({cols})",
                    desc.schema.columns.len()
                )?;
            }
            Err(e) => {
                writeln!(out, "[!!] table '{name}': {e}")?;
                all_ok = false;
            }
        }
    }
    Ok(all_ok)
}

#[cfg(test)]
mod tests {
    use super::diagnose;
    use tempfile::TempDir;

    fn run_doctor(db_path: &std::path::Path) -> (String, bool) {
        let mut buf = Vec::new();
        let ok = diagnose(&mut buf, db_path).unwrap();
        (String::from_utf8(buf).unwrap(), ok)
    }

    #[test]
    fn doctor_reports_missing_database() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("nonexistent");

        let (output, ok) = run_doctor(&db_path);

        assert!(!ok);
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

        crate::catalog::metadata::initialize(&db_path).unwrap();

        let (output, ok) = run_doctor(&db_path);

        assert!(ok);
        assert!(output.contains("[ok] database found at"));
        assert!(!output.contains("[!!]"));
    }

    #[test]
    fn doctor_reports_uninitialized_directory() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("testdb");
        std::fs::create_dir(&db_path).unwrap();

        let (output, ok) = run_doctor(&db_path);

        assert!(!ok);
        assert!(output.contains("[!!]"));
        assert!(output.contains("has no balik.meta"));
    }

    #[test]
    fn doctor_reports_too_new_database() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("testdb");
        std::fs::create_dir(&db_path).unwrap();
        let future = crate::catalog::metadata::FORMAT_VERSION + 1;
        std::fs::write(
            db_path.join("balik.meta"),
            format!("magic = \"balik-db\"\nformat_version = {future}\ncreated = \"unix:0\"\n"),
        )
        .unwrap();

        let (output, ok) = run_doctor(&db_path);

        assert!(!ok);
        assert!(output.contains(&format!("uses format version {future}")));
        assert!(output.contains(&format!(
            "this binary supports {}",
            crate::catalog::metadata::FORMAT_VERSION
        )));
    }

    #[test]
    fn doctor_shows_format_version_line() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("testdb");
        crate::catalog::metadata::initialize(&db_path).unwrap();

        let (output, ok) = run_doctor(&db_path);

        assert!(ok);
        assert!(output.contains(&format!(
            "supported format version: {}",
            crate::catalog::metadata::FORMAT_VERSION
        )));
        assert!(output.contains(&format!(
            "format version: {}",
            crate::catalog::metadata::FORMAT_VERSION
        )));
    }

    /// Initialize a balik db at `tmp/testdb` and create a `users` table with
    /// schema (id INT, name TEXT). Returns the db path.
    fn setup_db_with_users(tmp: &TempDir) -> std::path::PathBuf {
        use crate::catalog::schema::{Column, ColumnType, Schema};
        use crate::catalog::tables::TableOptions;
        use crate::storage::Storage;
        use crate::storage::column_store::ColumnStore;

        let db = tmp.path().join("testdb");
        crate::catalog::metadata::initialize(&db).unwrap();
        let mut store = ColumnStore::open(&db).unwrap();
        store
            .create_table(
                "users",
                Schema {
                    columns: vec![
                        Column {
                            name: "id".to_string(),
                            ty: ColumnType::Int,
                            nullable: false,
                        },
                        Column {
                            name: "name".to_string(),
                            ty: ColumnType::Text,
                            nullable: false,
                        },
                    ],
                },
                TableOptions::default(),
            )
            .unwrap();
        db
    }

    #[test]
    fn doctor_reports_empty_catalog() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("testdb");
        crate::catalog::metadata::initialize(&db_path).unwrap();

        let (output, ok) = run_doctor(&db_path);

        assert!(ok);
        assert!(output.contains("[ok] catalog: 0 tables"));
        assert!(!output.contains("[!!]"));
    }

    #[test]
    fn doctor_reports_healthy_table() {
        let tmp = TempDir::new().unwrap();
        let db = setup_db_with_users(&tmp);

        let (output, ok) = run_doctor(&db);

        assert!(ok, "expected ok, got output:\n{output}");
        assert!(output.contains("[ok] catalog: 1 tables"));
        assert!(output.contains("[ok] users: 2 columns"));
        assert!(output.contains("id INT"));
        assert!(output.contains("name TEXT"));
        assert!(!output.contains("[!!]"));
    }

    #[test]
    fn doctor_detects_corrupt_catalog() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("testdb");
        crate::catalog::metadata::initialize(&db_path).unwrap();
        // Write garbage where catalog.toml lives.
        std::fs::write(db_path.join("catalog.toml"), "not toml at all =====")
            .unwrap();

        let (output, ok) = run_doctor(&db_path);

        assert!(!ok);
        assert!(output.contains("[!!] cannot open catalog"));
    }

    #[test]
    fn doctor_detects_corrupt_manifest() {
        let tmp = TempDir::new().unwrap();
        let db = setup_db_with_users(&tmp);
        // Stomp on the table's manifest.
        std::fs::write(
            db.join("tables").join("00000001").join("manifest.toml"),
            "this is not toml",
        )
        .unwrap();

        let (output, ok) = run_doctor(&db);

        assert!(!ok);
        assert!(output.contains("[!!] table 'users':"));
    }
}
