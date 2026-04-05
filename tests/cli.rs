use assert_cmd::Command;
use predicates::prelude::*;

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