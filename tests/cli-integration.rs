use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

fn balik_cli() -> Command {
    Command::cargo_bin("balik-cli").unwrap()
}

fn init_db() -> (TempDir, std::path::PathBuf) {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("db");
    balik_cli()
        .args(["init", "--db", db.to_str().unwrap()])
        .assert()
        .success();
    (tmp, db)
}

#[test]
fn help_succeeds() {
    balik_cli()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Usage:"));
}

#[test]
fn unexpected_command_fails() {
    balik_cli()
        .arg("test")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand"));
}

#[test]
fn verbose_flag_emits_tracing_logs() {
    // GIVEN
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("testdb");

    // WHEN / THEN: `-v` raises the log level to INFO, so init emits tracing events
    balik_cli()
        .args(["-vv", "init", "--db", db_path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Initialized empty balik database"))
        .stdout(predicate::str::contains("INFO initializing database"))
        .stdout(predicate::str::contains(
            "DEBUG creating database directory",
        ))
        .stdout(predicate::str::contains("DEBUG writing metadata file path"))
        .stdout(predicate::str::contains("INFO database initialized"));
}

#[test]
fn doctor_fails_when_database_missing() {
    // GIVEN
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("nonexistent");

    // WHEN / THEN
    balik_cli()
        .args(["doctor", "--db", db_path.to_str().unwrap()])
        .assert()
        .failure()
        .stdout(predicate::str::contains("[!!] database not found"))
        .stderr(predicate::str::contains(
            "one or more diagnostic checks failed",
        ));
}

#[test]
fn if_init_done_then_doctor_succeeds() {
    // GIVEN
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("testdb");

    // WHEN
    balik_cli()
        .args(["init", "--db", db_path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Initialized empty balik database"));

    // THEN
    balik_cli()
        .args(["doctor", "--db", db_path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("[ok] database found at"));
}

#[test]
fn table_create_then_list_shows_both() {
    let (_tmp, db) = init_db();

    balik_cli()
        .args([
            "table-create",
            "--db",
            db.to_str().unwrap(),
            "--table",
            "users",
            "--columns",
            "id:INT,name:TEXT",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Created table 'users'"));

    balik_cli()
        .args([
            "table-create",
            "--db",
            db.to_str().unwrap(),
            "--table",
            "orders",
            "--columns",
            "id:INT,total:INT",
        ])
        .assert()
        .success();

    balik_cli()
        .args(["table-list", "--db", db.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("orders"))
        .stdout(predicate::str::contains("users"));
}

#[test]
fn table_describe_shows_schema() {
    let (_tmp, db) = init_db();
    balik_cli()
        .args([
            "table-create",
            "--db",
            db.to_str().unwrap(),
            "--table",
            "users",
            "--columns",
            "id:INT,name:TEXT,age:INT",
            "--row-group-size",
            "4096",
        ])
        .assert()
        .success();

    balik_cli()
        .args([
            "table-describe",
            "--db",
            db.to_str().unwrap(),
            "--table",
            "users",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Table:          users"))
        .stdout(predicate::str::contains("Storage:        column-store"))
        .stdout(predicate::str::contains("Row group size: 4096"))
        .stdout(predicate::str::contains("id"))
        .stdout(predicate::str::contains("INT"))
        .stdout(predicate::str::contains("name"))
        .stdout(predicate::str::contains("TEXT"));
}

#[test]
fn table_create_duplicate_name_fails() {
    let (_tmp, db) = init_db();
    balik_cli()
        .args([
            "table-create",
            "--db",
            db.to_str().unwrap(),
            "--table",
            "users",
            "--columns",
            "id:INT",
        ])
        .assert()
        .success();

    balik_cli()
        .args([
            "table-create",
            "--db",
            db.to_str().unwrap(),
            "--table",
            "users",
            "--columns",
            "id:INT",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already exists"));
}

#[test]
fn table_create_with_invalid_schema_fails() {
    let (_tmp, db) = init_db();

    // unknown type
    balik_cli()
        .args([
            "table-create",
            "--db",
            db.to_str().unwrap(),
            "--table",
            "users",
            "--columns",
            "id:BLOB",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unsupported column type"));

    // duplicate column
    balik_cli()
        .args([
            "table-create",
            "--db",
            db.to_str().unwrap(),
            "--table",
            "users",
            "--columns",
            "id:INT,id:TEXT",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("duplicate column name"));

    // invalid table name
    balik_cli()
        .args([
            "table-create",
            "--db",
            db.to_str().unwrap(),
            "--table",
            "1users",
            "--columns",
            "id:INT",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("must start with a letter"));
}

#[test]
fn tables_persist_across_restart() {
    let (_tmp, db) = init_db();

    // First invocation: create tables.
    balik_cli()
        .args([
            "table-create",
            "--db",
            db.to_str().unwrap(),
            "--table",
            "users",
            "--columns",
            "id:INT,name:TEXT",
        ])
        .assert()
        .success();
    balik_cli()
        .args([
            "table-create",
            "--db",
            db.to_str().unwrap(),
            "--table",
            "orders",
            "--columns",
            "id:INT,total:INT",
        ])
        .assert()
        .success();

    // Second invocation: list + describe must read back what the first wrote.
    balik_cli()
        .args(["table-list", "--db", db.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("orders"))
        .stdout(predicate::str::contains("users"));

    balik_cli()
        .args([
            "table-describe",
            "--db",
            db.to_str().unwrap(),
            "--table",
            "users",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Table:          users"))
        .stdout(predicate::str::contains("name"));
}

#[test]
fn table_drop_removes_table() {
    let (_tmp, db) = init_db();
    balik_cli()
        .args([
            "table-create",
            "--db",
            db.to_str().unwrap(),
            "--table",
            "users",
            "--columns",
            "id:INT",
        ])
        .assert()
        .success();

    balik_cli()
        .args([
            "table-drop",
            "--db",
            db.to_str().unwrap(),
            "--table",
            "users",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Dropped table 'users'"));

    balik_cli()
        .args(["table-list", "--db", db.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("(no tables)"));

    balik_cli()
        .args([
            "table-describe",
            "--db",
            db.to_str().unwrap(),
            "--table",
            "users",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no such table"));
}

#[test]
fn table_create_writes_expected_layout() {
    let (_tmp, db) = init_db();
    balik_cli()
        .args([
            "table-create",
            "--db",
            db.to_str().unwrap(),
            "--table",
            "users",
            "--columns",
            "id:INT,name:TEXT",
        ])
        .assert()
        .success();

    let table_dir = db.join("tables").join("00000001");
    assert!(table_dir.is_dir(), "table dir should exist");
    assert!(
        table_dir.join("manifest.toml").is_file(),
        "manifest.toml should exist"
    );
    assert!(
        table_dir.join("row_groups").is_dir(),
        "row_groups/ should exist"
    );
    assert!(
        db.join("catalog.toml").is_file(),
        "catalog.toml should exist"
    );

    let catalog = std::fs::read_to_string(db.join("catalog.toml")).unwrap();
    assert!(catalog.contains("storage_track = \"column-store\""));

    // Row group 000000 is materialized at create time with one empty .col
    // file per column. Each file is exactly the 56-byte header.
    let rg0 = table_dir.join("row_groups").join("000000");
    assert!(rg0.is_dir(), "row_groups/000000/ should exist");
    for col in ["id.col", "name.col"] {
        let p = rg0.join(col);
        assert!(p.is_file(), "{col} should exist in row group 0");
        let bytes = std::fs::read(&p).unwrap();
        assert_eq!(
            bytes.len(),
            56,
            "{col} should be exactly 56 bytes (header only)"
        );
        assert_eq!(&bytes[0..8], b"BALIKCOL", "{col} should start with magic");
    }

    // The row group's delete bitmap is also materialized at create time:
    // 24-byte header + ceil(row_group_size / 8) bytes of zeroed bitmap.
    // Default row_group_size = 8192, so 24 + 1024 = 1048 bytes.
    let bm = rg0.join("deletes.bm");
    assert!(bm.is_file(), "deletes.bm should exist in row group 0");
    let bm_bytes = std::fs::read(&bm).unwrap();
    assert_eq!(
        bm_bytes.len(),
        1048,
        "deletes.bm should be 24-byte header + 1024 bytes of bitmap"
    );
    assert_eq!(
        &bm_bytes[0..8],
        b"BALIKDEL",
        "deletes.bm should start with magic"
    );
    assert!(
        bm_bytes[24..].iter().all(|&b| b == 0),
        "fresh deletes.bm should have every bit clear"
    );
}

#[test]
fn table_create_without_init_fails() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("not-a-db");

    balik_cli()
        .args([
            "table-create",
            "--db",
            db.to_str().unwrap(),
            "--table",
            "users",
            "--columns",
            "id:INT",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "not an initialized balik database",
        ));
}

#[test]
fn row_insert_get_persist_across_restart() {
    let (_tmp, db) = init_db();
    let db = db.to_str().unwrap();

    balik_cli()
        .args([
            "table-create",
            "--db",
            db,
            "--table",
            "users",
            "--columns",
            "id:INT,name:TEXT?",
        ])
        .assert()
        .success();

    balik_cli()
        .args([
            "row-insert",
            "--db",
            db,
            "--table",
            "users",
            "--values",
            "1,Alice",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("rid 0"));
    balik_cli()
        .args([
            "row-insert",
            "--db",
            db,
            "--table",
            "users",
            "--values",
            "2,NULL",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("rid 1"));

    // Each invocation is a fresh process, so reading back proves durability.
    balik_cli()
        .args(["row-get", "--db", db, "--table", "users", "--rid", "0"])
        .assert()
        .success()
        .stdout(predicate::str::contains("id=1, name=Alice"));
    balik_cli()
        .args(["row-get", "--db", db, "--table", "users", "--rid", "1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("id=2, name=NULL"));

    // An id past the end reads back as not found, not an error.
    balik_cli()
        .args(["row-get", "--db", db, "--table", "users", "--rid", "99"])
        .assert()
        .success()
        .stdout(predicate::str::contains("not found"));
}

#[test]
fn row_insert_null_into_not_null_column_fails() {
    let (_tmp, db) = init_db();
    let db = db.to_str().unwrap();
    balik_cli()
        .args([
            "table-create",
            "--db",
            db,
            "--table",
            "users",
            "--columns",
            "id:INT,name:TEXT",
        ])
        .assert()
        .success();
    balik_cli()
        .args([
            "row-insert",
            "--db",
            db,
            "--table",
            "users",
            "--values",
            "NULL,bob",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("NOT NULL"));
}

#[test]
fn table_scan_lists_rows_after_restart() {
    let (_tmp, db) = init_db();
    let db = db.to_str().unwrap();

    balik_cli()
        .args([
            "table-create",
            "--db",
            db,
            "--table",
            "users",
            "--columns",
            "id:INT,name:TEXT?",
        ])
        .assert()
        .success();

    for (id, name) in [("1", "alice"), ("2", "NULL"), ("3", "carol")] {
        balik_cli()
            .args([
                "row-insert",
                "--db",
                db,
                "--table",
                "users",
                "--values",
                &format!("{id},{name}"),
            ])
            .assert()
            .success();
    }

    // Fresh process → proves the scan reads from disk, not a cached state.
    balik_cli()
        .args(["table-scan", "--db", db, "--table", "users"])
        .assert()
        .success()
        .stdout(predicate::str::contains("rid 0: id=1, name=alice"))
        .stdout(predicate::str::contains("rid 1: id=2, name=NULL"))
        .stdout(predicate::str::contains("rid 2: id=3, name=carol"));
}

#[test]
fn table_scan_on_empty_table_reports_no_rows() {
    let (_tmp, db) = init_db();
    let db = db.to_str().unwrap();
    balik_cli()
        .args([
            "table-create",
            "--db",
            db,
            "--table",
            "t",
            "--columns",
            "id:INT",
        ])
        .assert()
        .success();
    balik_cli()
        .args(["table-scan", "--db", db, "--table", "t"])
        .assert()
        .success()
        .stdout(predicate::str::contains("(no rows)"));
}

#[test]
fn row_delete_hides_row_from_get_and_scan_across_restart() {
    let (_tmp, db) = init_db();
    let db = db.to_str().unwrap();

    balik_cli()
        .args([
            "table-create",
            "--db",
            db,
            "--table",
            "users",
            "--columns",
            "id:INT,name:TEXT",
        ])
        .assert()
        .success();
    for (id, name) in [("1", "alice"), ("2", "bob"), ("3", "carol")] {
        balik_cli()
            .args([
                "row-insert",
                "--db",
                db,
                "--table",
                "users",
                "--values",
                &format!("{id},{name}"),
            ])
            .assert()
            .success();
    }

    balik_cli()
        .args(["row-delete", "--db", db, "--table", "users", "--rid", "1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("rid 1: deleted"));

    // Fresh process → tombstone read back from disk, not memory.
    balik_cli()
        .args(["row-get", "--db", db, "--table", "users", "--rid", "1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("rid 1: not found"));

    balik_cli()
        .args(["table-scan", "--db", db, "--table", "users"])
        .assert()
        .success()
        .stdout(predicate::str::contains("rid 0: id=1, name=alice"))
        .stdout(predicate::str::contains("rid 2: id=3, name=carol"))
        .stdout(predicate::str::contains("rid 1").not());
}

#[test]
fn row_delete_unknown_rid_fails() {
    let (_tmp, db) = init_db();
    let db = db.to_str().unwrap();
    balik_cli()
        .args([
            "table-create",
            "--db",
            db,
            "--table",
            "users",
            "--columns",
            "id:INT",
        ])
        .assert()
        .success();
    balik_cli()
        .args(["row-delete", "--db", db, "--table", "users", "--rid", "0"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no such record"));
}
