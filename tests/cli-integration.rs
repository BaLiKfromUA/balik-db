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

    run_query(&db, "CREATE TABLE users (id INT NOT NULL, name TEXT NOT NULL)")
        .success()
        .stdout(predicate::str::contains("Created table 'users'"));

    run_query(&db, "CREATE TABLE orders (id INT NOT NULL, total INT NOT NULL)").success();

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
    run_query(
        &db,
        "CREATE TABLE users (id INT NOT NULL, name TEXT NOT NULL, age INT NOT NULL)",
    )
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
        .stdout(predicate::str::contains("Row group size: 8192"))
        .stdout(predicate::str::contains("id"))
        .stdout(predicate::str::contains("INT"))
        .stdout(predicate::str::contains("name"))
        .stdout(predicate::str::contains("TEXT"));
}

#[test]
fn table_create_duplicate_name_fails() {
    let (_tmp, db) = init_db();
    run_query(&db, "CREATE TABLE users (id INT NOT NULL)").success();

    run_query(&db, "CREATE TABLE users (id INT NOT NULL)")
        .failure()
        .stderr(predicate::str::contains("already exists"));
}

#[test]
fn table_create_with_invalid_schema_fails() {
    let (_tmp, db) = init_db();

    // unknown type
    run_query(&db, "CREATE TABLE users (id BLOB)")
        .failure()
        .stderr(predicate::str::contains("column type `BLOB`"));

    // duplicate column
    run_query(&db, "CREATE TABLE users (id INT, id TEXT)")
        .failure()
        .stderr(predicate::str::contains("duplicate column name"));

    // invalid table name
    run_query(&db, "CREATE TABLE 1users (id INT)")
        .failure()
        .stderr(predicate::str::contains("sql parser error"));
}

#[test]
fn tables_persist_across_restart() {
    let (_tmp, db) = init_db();

    // First invocation: create tables.
    run_query(&db, "CREATE TABLE users (id INT NOT NULL, name TEXT NOT NULL)").success();
    run_query(&db, "CREATE TABLE orders (id INT NOT NULL, total INT NOT NULL)").success();

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
    run_query(&db, "CREATE TABLE users (id INT NOT NULL)").success();

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
    run_query(&db, "CREATE TABLE users (id INT NOT NULL, name TEXT NOT NULL)").success();

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

    run_query(&db, "CREATE TABLE users (id INT NOT NULL)")
        .failure()
        .stderr(predicate::str::contains(
            "not an initialized balik database",
        ));
}

#[test]
fn row_insert_get_persist_across_restart() {
    let (_tmp, db) = init_db();
    let db = db.to_str().unwrap();

    run_query(db, "CREATE TABLE users (id INT NOT NULL, name TEXT)").success();

    run_query(db, "INSERT INTO users VALUES (1, 'Alice')")
        .success()
        .stdout(predicate::str::contains("rid 0"));
    run_query(db, "INSERT INTO users VALUES (2, NULL)")
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
    run_query(db, "CREATE TABLE users (id INT NOT NULL, name TEXT NOT NULL)").success();
    run_query(db, "INSERT INTO users VALUES (NULL, 'bob')")
        .failure()
        .stderr(predicate::str::contains("NOT NULL"));
}

#[test]
fn table_scan_lists_rows_after_restart() {
    let (_tmp, db) = init_db();
    let db = db.to_str().unwrap();

    run_query(db, "CREATE TABLE users (id INT NOT NULL, name TEXT)").success();

    for sql in [
        "INSERT INTO users VALUES (1, 'alice')",
        "INSERT INTO users VALUES (2, NULL)",
        "INSERT INTO users VALUES (3, 'carol')",
    ] {
        run_query(db, sql).success();
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
    run_query(db, "CREATE TABLE t (id INT NOT NULL)").success();
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

    run_query(db, "CREATE TABLE users (id INT NOT NULL, name TEXT NOT NULL)").success();
    for sql in [
        "INSERT INTO users VALUES (1, 'alice')",
        "INSERT INTO users VALUES (2, 'bob')",
        "INSERT INTO users VALUES (3, 'carol')",
    ] {
        run_query(db, sql).success();
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

    run_query(db, "CREATE TABLE users (id INT NOT NULL, name TEXT NOT NULL)").success();
    for sql in [
        "INSERT INTO users VALUES (1, 'alice')",
        "INSERT INTO users VALUES (2, 'bob')",
    ] {
        run_query(db, sql).success();
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
    run_query(db, "CREATE TABLE users (id INT NOT NULL)").success();
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
    run_query(db, "CREATE TABLE users (id INT NOT NULL)").success();
    balik_cli()
        .args(["row-delete", "--db", db, "--table", "users", "--rid", "0"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no such record"));
}

/// Initialize a db with a `users(id INT, name TEXT, age INT)` table.
fn init_db_with_users() -> (TempDir, std::path::PathBuf) {
    let (tmp, db) = init_db();
    run_query(&db, "CREATE TABLE users (id INT NOT NULL, name TEXT, age INT)").success();
    (tmp, db)
}

fn run_query(db: impl AsRef<std::path::Path>, sql: &str) -> assert_cmd::assert::Assert {
    balik_cli()
        .args(["query", "--db", db.as_ref().to_str().unwrap(), "--sql", sql])
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
fn query_order_by_column_not_in_select_list() {
    let (_tmp, db) = init_db();
    run_query(&db, "CREATE TABLE users (id INT, name TEXT, age INT)").success();
    // Names sort Alice(2) < Bob(3) < Charlie(1), independently of id.
    run_query(&db, "INSERT INTO users VALUES (1, 'Charlie', 20)").success();
    run_query(&db, "INSERT INTO users VALUES (2, 'Alice', 30)").success();
    run_query(&db, "INSERT INTO users VALUES (3, 'Bob', 25)").success();

    // ORDER BY name though only id is selected: rows come back in name order,
    // and the output carries just the id column.
    run_query(&db, "SELECT id FROM users ORDER BY name")
        .success()
        .stdout("id\n--\n2 \n3 \n1 \n");
    run_query(&db, "SELECT id FROM users ORDER BY name DESC")
        .success()
        .stdout("id\n--\n1 \n3 \n2 \n");
    // The same, fused into a TopK by ORDER BY + LIMIT.
    run_query(&db, "SELECT id FROM users ORDER BY name LIMIT 2")
        .success()
        .stdout("id\n--\n2 \n3 \n");
}

fn explain(db: &std::path::Path, sql: &str, optimize: bool) -> assert_cmd::assert::Assert {
    let mut cmd = balik_cli();
    cmd.args(["explain", "--db", db.to_str().unwrap(), "--sql", sql]);
    if optimize {
        cmd.arg("--optimize");
    }
    cmd.assert()
}

#[test]
fn explain_optimized_shows_logical_and_physical_plans() {
    let (_tmp, db) = init_db_with_users();
    // --optimize fuses ORDER BY + LIMIT into TopK and pushes the needed columns
    // onto the scan; both plans reflect that.
    explain(
        &db,
        "SELECT id, name FROM users WHERE age > 18 ORDER BY name LIMIT 10",
        true,
    )
    .success()
    // Logical plan.
    .stdout(predicate::str::contains("Logical Plan:"))
    .stdout(predicate::str::contains("TopK [name] 10"))
    .stdout(predicate::str::contains("Projection [id, name]"))
    .stdout(predicate::str::contains("Filter [age > 18]"))
    .stdout(predicate::str::contains("Scan users [id, name, age]"))
    // Physical plan.
    .stdout(predicate::str::contains("Physical Plan:"))
    .stdout(predicate::str::contains("TopKExec [name] 10"))
    .stdout(predicate::str::contains(
        "TableScanExec users [id, name, age] prune=[age > 18]",
    ));
}

#[test]
fn explain_without_optimize_keeps_sort_and_limit_separate() {
    let (_tmp, db) = init_db_with_users();
    explain(&db, "SELECT id FROM users ORDER BY id LIMIT 5", false)
        .success()
        .stdout(predicate::str::contains("Sort [id]"))
        .stdout(predicate::str::contains("Limit 5"))
        .stdout(predicate::str::contains("SortExec [id]"))
        .stdout(predicate::str::contains("LimitExec 5"))
        .stdout(predicate::str::contains("TopK").not());
}

#[test]
fn explain_unknown_table_fails() {
    let (_tmp, db) = init_db();
    explain(&db, "SELECT * FROM ghosts", false)
        .failure()
        .stderr(predicate::str::contains("no such table"));
}

#[test]
fn explain_unknown_column_fails() {
    let (_tmp, db) = init_db_with_users();
    explain(&db, "SELECT nope FROM users", false)
        .failure()
        .stderr(predicate::str::contains("unknown column 'nope'"));
}

#[test]
fn explain_invalid_query_fails() {
    let (_tmp, db) = init_db();
    explain(&db, "SELEC id FROM users", false)
        .failure()
        .stderr(predicate::str::contains("error:"));
}
