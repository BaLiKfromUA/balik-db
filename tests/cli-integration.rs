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
