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
fn parse_select_prints_ast() {
    balik_cli()
        .args([
            "parse",
            "--query",
            "SELECT id, name FROM users WHERE age > 18 ORDER BY name LIMIT 10",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Select"))
        .stdout(predicate::str::contains("Columns"))
        .stdout(predicate::str::contains("Compare"))
        .stdout(predicate::str::contains("OrderBy"));
}

#[test]
fn parse_create_table_prints_ast() {
    balik_cli()
        .args(["parse", "--query", "CREATE TABLE users (id INT, name TEXT)"])
        .assert()
        .success()
        .stdout(predicate::str::contains("CreateTable"))
        .stdout(predicate::str::contains("Int"))
        .stdout(predicate::str::contains("Text"));
}

#[test]
fn parse_invalid_query_fails_with_stderr_message() {
    balik_cli()
        .args(["parse", "--query", "SELEC id FROM users"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error:"));
}

#[test]
fn parse_unsupported_query_reports_unsupported() {
    balik_cli()
        .args(["parse", "--query", "SELECT a FROM x JOIN y ON x.id = y.id"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unsupported"));
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
fn row_update_reassigns_rid_and_persists_across_restart() {
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
    for values in ["1,alice", "2,bob"] {
        balik_cli()
            .args([
                "row-insert",
                "--db",
                db,
                "--table",
                "users",
                "--values",
                values,
            ])
            .assert()
            .success();
    }

    // Update rid 0 — should reassign to rid 2 (next_rid at update time).
    balik_cli()
        .args([
            "row-update",
            "--db",
            db,
            "--table",
            "users",
            "--rid",
            "0",
            "--values",
            "1,alicia",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("rid 0: updated as rid 2"));

    // Fresh process → updated row + tombstone read from disk.
    balik_cli()
        .args(["row-get", "--db", db, "--table", "users", "--rid", "0"])
        .assert()
        .success()
        .stdout(predicate::str::contains("rid 0: not found"));
    balik_cli()
        .args(["table-scan", "--db", db, "--table", "users"])
        .assert()
        .success()
        .stdout(predicate::str::contains("rid 1: id=2, name=bob"))
        .stdout(predicate::str::contains("rid 2: id=1, name=alicia"))
        .stdout(predicate::str::contains("rid 0").not());
}

#[test]
fn row_update_unknown_rid_fails() {
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
        .args([
            "row-update",
            "--db",
            db,
            "--table",
            "users",
            "--rid",
            "0",
            "--values",
            "1",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no such record"));
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

/// Initialize a db with a `users(id INT, name TEXT, age INT)` table.
fn init_db_with_users() -> (TempDir, std::path::PathBuf) {
    let (tmp, db) = init_db();
    balik_cli()
        .args([
            "table-create",
            "--db",
            db.to_str().unwrap(),
            "--table",
            "users",
            "--columns",
            "id:INT,name:TEXT?,age:INT?",
        ])
        .assert()
        .success();
    (tmp, db)
}

#[test]
fn explain_logical_prints_select_tree() {
    let (_tmp, db) = init_db_with_users();
    balik_cli()
        .args([
            "explain-logical",
            "--db",
            db.to_str().unwrap(),
            "--query",
            "SELECT id, name FROM users WHERE age > 18 LIMIT 10",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Limit 10"))
        .stdout(predicate::str::contains("Projection [id, name]"))
        .stdout(predicate::str::contains("Filter [age > 18]"))
        .stdout(predicate::str::contains("Scan users"));
}

#[test]
fn explain_logical_optimize_pushes_columns_onto_scan() {
    let (_tmp, db) = init_db_with_users();
    // Without --optimize the scan reads every column (no column list).
    balik_cli()
        .args([
            "explain-logical",
            "--db",
            db.to_str().unwrap(),
            "--query",
            "SELECT id, name FROM users WHERE age > 18",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Scan users\n"));

    // With --optimize the scan lists exactly the columns the query needs:
    // the projected ones plus the column used only in the filter.
    balik_cli()
        .args([
            "explain-logical",
            "--db",
            db.to_str().unwrap(),
            "--query",
            "SELECT id, name FROM users WHERE age > 18",
            "--optimize",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Scan users [id, name, age]"));
}

#[test]
fn explain_logical_optimize_fuses_sort_and_limit_into_topk() {
    let (_tmp, db) = init_db_with_users();
    // Without --optimize the ORDER BY and LIMIT are separate operators.
    balik_cli()
        .args([
            "explain-logical",
            "--db",
            db.to_str().unwrap(),
            "--query",
            "SELECT id FROM users ORDER BY name LIMIT 10",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Limit 10"))
        .stdout(predicate::str::contains("Sort [name]"));

    // With --optimize they collapse into a single TopK node.
    balik_cli()
        .args([
            "explain-logical",
            "--db",
            db.to_str().unwrap(),
            "--query",
            "SELECT id FROM users ORDER BY name LIMIT 10",
            "--optimize",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("TopK [name] 10"))
        .stdout(predicate::str::contains("Limit").not())
        .stdout(predicate::str::contains("Sort").not());
}

#[test]
fn explain_logical_json_format_is_parseable() {
    let (_tmp, db) = init_db_with_users();
    let output = balik_cli()
        .args([
            "explain-logical",
            "--db",
            db.to_str().unwrap(),
            "--query",
            "SELECT * FROM users",
            "--format",
            "json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: serde_json::Value = serde_json::from_slice(&output).expect("output is valid JSON");
    // `SELECT *` expands to every column under a Projection over a Scan.
    assert!(
        json["Projection"]["columns"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == "age")
    );
}

#[test]
fn explain_logical_unknown_column_fails() {
    let (_tmp, db) = init_db_with_users();
    balik_cli()
        .args([
            "explain-logical",
            "--db",
            db.to_str().unwrap(),
            "--query",
            "SELECT nope FROM users",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown column 'nope'"));
}

#[test]
fn explain_logical_unknown_table_fails() {
    let (_tmp, db) = init_db();
    balik_cli()
        .args([
            "explain-logical",
            "--db",
            db.to_str().unwrap(),
            "--query",
            "SELECT * FROM ghosts",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no such table"));
}

#[test]
fn explain_logical_invalid_query_fails() {
    let (_tmp, db) = init_db();
    balik_cli()
        .args([
            "explain-logical",
            "--db",
            db.to_str().unwrap(),
            "--query",
            "SELEC id FROM users",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error:"));
}

fn run_query(db: &std::path::Path, sql: &str) -> assert_cmd::assert::Assert {
    balik_cli()
        .args(["query", "--db", db.to_str().unwrap(), "--sql", sql])
        .assert()
}

#[test]
fn query_runs_create_insert_select_pipeline() {
    let (_tmp, db) = init_db();
    run_query(&db, "CREATE TABLE users (id INT, name TEXT, age INT)").success();
    run_query(&db, "INSERT INTO users VALUES (1, 'Alice', 20)").success();
    run_query(&db, "INSERT INTO users VALUES (2, 'Bob', 15)").success();
    run_query(&db, "INSERT INTO users VALUES (3, 'Carol', 30)").success();

    // A fresh process per invocation, so this select also proves the inserts
    // persisted across a reopen.
    let assert = run_query(
        &db,
        "SELECT id, name FROM users WHERE age >= 18 ORDER BY name LIMIT 10",
    )
    .success()
    .stdout(predicate::str::contains("Alice"))
    .stdout(predicate::str::contains("Carol"))
    .stdout(predicate::str::contains("Bob").not());

    // WHERE dropped Bob (age 15); ORDER BY put Alice before Carol.
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.find("Alice") < stdout.find("Carol"),
        "rows not in name order:\n{stdout}"
    );
}

#[test]
fn query_select_star_returns_all_columns() {
    let (_tmp, db) = init_db();
    run_query(&db, "CREATE TABLE t (id INT, name TEXT, age INT)").success();
    run_query(&db, "INSERT INTO t VALUES (7, 'Zed', 40)").success();
    run_query(&db, "SELECT * FROM t")
        .success()
        .stdout(predicate::str::contains("id"))
        .stdout(predicate::str::contains("name"))
        .stdout(predicate::str::contains("age"))
        .stdout(predicate::str::contains("Zed"))
        .stdout(predicate::str::contains("40"));
}

#[test]
fn query_limit_zero_returns_no_rows() {
    let (_tmp, db) = init_db();
    run_query(&db, "CREATE TABLE t (id INT, name TEXT)").success();
    run_query(&db, "INSERT INTO t VALUES (1, 'Alice')").success();
    run_query(&db, "SELECT * FROM t LIMIT 0")
        .success()
        .stdout(predicate::str::contains("Alice").not());
}

#[test]
fn query_unknown_table_fails() {
    let (_tmp, db) = init_db();
    run_query(&db, "SELECT * FROM ghosts")
        .failure()
        .stderr(predicate::str::contains("no such table"));
}

#[test]
fn explain_shows_logical_and_physical_plans() {
    let (_tmp, db) = init_db();
    run_query(&db, "CREATE TABLE users (id INT, name TEXT, age INT)").success();
    balik_cli()
        .args([
            "explain",
            "--db",
            db.to_str().unwrap(),
            "--optimize",
            "--sql",
            "SELECT id, name FROM users WHERE age > 18 ORDER BY name LIMIT 10",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Logical Plan:"))
        .stdout(predicate::str::contains("Physical Plan:"))
        .stdout(predicate::str::contains("TopKExec"))
        .stdout(predicate::str::contains("TableScanExec users"))
        .stdout(predicate::str::contains("prune=[age > 18]"));
}

#[test]
fn explain_without_optimize_shows_sort_and_limit() {
    let (_tmp, db) = init_db();
    run_query(&db, "CREATE TABLE users (id INT, name TEXT, age INT)").success();
    balik_cli()
        .args([
            "explain",
            "--db",
            db.to_str().unwrap(),
            "--sql",
            "SELECT id FROM users ORDER BY id LIMIT 5",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("SortExec"))
        .stdout(predicate::str::contains("LimitExec"));
}
