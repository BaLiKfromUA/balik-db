use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

fn balik_cli() -> Command {
    Command::cargo_bin("balik-cli").unwrap()
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
