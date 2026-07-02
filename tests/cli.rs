//! End-to-end CLI tests: spawn the real `csv-to-sqlite` binary, marshalling
//! args exactly as a user would, and assert on stdout / exit codes.

use assert_cmd::Command;
use predicates::prelude::*;
use std::io::Write;
use std::path::Path;
use tempfile::{NamedTempFile, TempDir};

fn write_csv(dir: &Path, name: &str, contents: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(contents.as_bytes()).unwrap();
    f.flush().unwrap();
    path
}

const PEOPLE_CSV: &str = "id,name,price\n1,alice,9.99\n2,bob,19.50\n3,carol,5.00\n";

#[test]
fn load_then_query_csv_output() {
    let dir = TempDir::new().unwrap();
    let csv = write_csv(dir.path(), "people.csv", PEOPLE_CSV);
    let db = dir.path().join("out.db");

    Command::cargo_bin("csv-to-sqlite")
        .unwrap()
        .arg(&csv)
        .arg("-o")
        .arg(&db)
        .arg("--query")
        .arg("SELECT name, price FROM people ORDER BY id")
        .arg("--format")
        .arg("csv")
        .assert()
        .success()
        .stdout(predicate::str::contains("name,price"))
        .stdout(predicate::str::contains("alice,9.99"))
        .stdout(predicate::str::contains("bob,19.5"));
}

#[test]
fn table_name_derived_from_filename() {
    let dir = TempDir::new().unwrap();
    let csv = write_csv(dir.path(), "people.csv", PEOPLE_CSV);
    let db = dir.path().join("out.db");

    // Query the table by the derived name "people" — proves naming works.
    Command::cargo_bin("csv-to-sqlite")
        .unwrap()
        .arg(&csv)
        .arg("-o")
        .arg(&db)
        .arg("--query")
        .arg("SELECT COUNT(*) AS n FROM people")
        .assert()
        .success()
        .stdout(predicate::str::contains("3"));
}

#[test]
fn custom_table_name_flag() {
    let dir = TempDir::new().unwrap();
    let csv = write_csv(dir.path(), "people.csv", PEOPLE_CSV);
    let db = dir.path().join("out.db");

    Command::cargo_bin("csv-to-sqlite")
        .unwrap()
        .arg(&csv)
        .arg("-o")
        .arg(&db)
        .arg("--table")
        .arg("humans")
        .arg("--query")
        .arg("SELECT COUNT(*) FROM humans")
        .assert()
        .success()
        .stdout(predicate::str::contains("3"));
}

#[test]
fn query_existing_db_without_loading() {
    let dir = TempDir::new().unwrap();
    let csv = write_csv(dir.path(), "people.csv", PEOPLE_CSV);
    let db = dir.path().join("out.db");

    // First, load.
    Command::cargo_bin("csv-to-sqlite")
        .unwrap()
        .arg(&csv)
        .arg("-o")
        .arg(&db)
        .assert()
        .success();

    // Then query --db with no inputs.
    Command::cargo_bin("csv-to-sqlite")
        .unwrap()
        .arg("--db")
        .arg(&db)
        .arg("--query")
        .arg("SELECT name FROM people WHERE price > 10")
        .arg("--format")
        .arg("csv")
        .assert()
        .success()
        .stdout(predicate::str::contains("bob"));
}

#[test]
fn if_exists_append_via_cli() {
    let dir = TempDir::new().unwrap();
    let csv = write_csv(dir.path(), "people.csv", PEOPLE_CSV);
    let db = dir.path().join("out.db");

    Command::cargo_bin("csv-to-sqlite")
        .unwrap()
        .arg(&csv)
        .arg("-o")
        .arg(&db)
        .assert()
        .success();

    let more = write_csv(dir.path(), "people.csv", "id,name,price\n4,dave,1.00\n");
    Command::cargo_bin("csv-to-sqlite")
        .unwrap()
        .arg(&more)
        .arg("-o")
        .arg(&db)
        .arg("--if-exists")
        .arg("append")
        .arg("--query")
        .arg("SELECT COUNT(*) FROM people")
        .assert()
        .success()
        .stdout(predicate::str::contains("4"));
}

#[test]
fn if_exists_fail_is_default_and_errors() {
    let dir = TempDir::new().unwrap();
    let csv = write_csv(dir.path(), "people.csv", PEOPLE_CSV);
    let db = dir.path().join("out.db");

    Command::cargo_bin("csv-to-sqlite")
        .unwrap()
        .arg(&csv)
        .arg("-o")
        .arg(&db)
        .assert()
        .success();

    // Loading the same table again with no --if-exists must fail.
    Command::cargo_bin("csv-to-sqlite")
        .unwrap()
        .arg(&csv)
        .arg("-o")
        .arg(&db)
        .assert()
        .failure()
        .stderr(predicate::str::contains("already exists"));
}

#[test]
fn multi_file_two_tables_via_cli() {
    let dir = TempDir::new().unwrap();
    let people = write_csv(dir.path(), "people.csv", PEOPLE_CSV);
    let orders = write_csv(dir.path(), "orders.csv", "order_id,amount\n100,3\n101,4\n");
    let db = dir.path().join("out.db");

    Command::cargo_bin("csv-to-sqlite")
        .unwrap()
        .arg(&people)
        .arg(&orders)
        .arg("-o")
        .arg(&db)
        .assert()
        .success();

    // Join across both tables to prove both loaded.
    Command::cargo_bin("csv-to-sqlite")
        .unwrap()
        .arg("--db")
        .arg(&db)
        .arg("--query")
        .arg("SELECT (SELECT COUNT(*) FROM people) AS p, (SELECT COUNT(*) FROM orders) AS o")
        .arg("--format")
        .arg("csv")
        .assert()
        .success()
        .stdout(predicate::str::contains("3,2"));
}

#[test]
fn no_header_via_cli() {
    let dir = TempDir::new().unwrap();
    let csv = write_csv(dir.path(), "raw.csv", "1,alice,9.99\n2,bob,19.50\n");
    let db = dir.path().join("out.db");

    Command::cargo_bin("csv-to-sqlite")
        .unwrap()
        .arg(&csv)
        .arg("-o")
        .arg(&db)
        .arg("--no-header")
        .arg("--query")
        .arg("SELECT col2 FROM raw WHERE col1 = 2")
        .arg("--format")
        .arg("csv")
        .assert()
        .success()
        .stdout(predicate::str::contains("bob"));
}

#[test]
fn custom_delimiter_via_cli() {
    let dir = TempDir::new().unwrap();
    let csv = write_csv(dir.path(), "semi.csv", "a;b\n1;x\n2;y\n");
    let db = dir.path().join("out.db");

    Command::cargo_bin("csv-to-sqlite")
        .unwrap()
        .arg(&csv)
        .arg("-o")
        .arg(&db)
        .arg("--delim")
        .arg(";")
        .arg("--query")
        .arg("SELECT b FROM semi WHERE a = 1")
        .arg("--format")
        .arg("csv")
        .assert()
        .success()
        .stdout(predicate::str::contains("x"));
}

#[test]
fn create_index_via_cli() {
    let dir = TempDir::new().unwrap();
    let csv = write_csv(dir.path(), "people.csv", PEOPLE_CSV);
    let db = dir.path().join("out.db");

    // Load and build one single-column and one composite index.
    Command::cargo_bin("csv-to-sqlite")
        .unwrap()
        .arg(&csv)
        .arg("-o")
        .arg(&db)
        .arg("--index")
        .arg("name")
        .arg("--index")
        .arg("id,price")
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "Created index \"idx_people_name\"",
        ))
        .stderr(predicate::str::contains(
            "Created index \"idx_people_id_price\"",
        ));

    // Both indexes must be present in the resulting database.
    Command::cargo_bin("csv-to-sqlite")
        .unwrap()
        .arg("--db")
        .arg(&db)
        .arg("--query")
        .arg("SELECT name FROM sqlite_master WHERE type='index' ORDER BY name")
        .arg("--format")
        .arg("csv")
        .assert()
        .success()
        .stdout(predicate::str::contains("idx_people_id_price"))
        .stdout(predicate::str::contains("idx_people_name"));
}

#[test]
fn index_on_existing_db_with_table() {
    let dir = TempDir::new().unwrap();
    let csv = write_csv(dir.path(), "people.csv", PEOPLE_CSV);
    let db = dir.path().join("out.db");

    // Load first, without indexing.
    Command::cargo_bin("csv-to-sqlite")
        .unwrap()
        .arg(&csv)
        .arg("-o")
        .arg(&db)
        .assert()
        .success();

    // Index an existing database; --table names the target.
    Command::cargo_bin("csv-to-sqlite")
        .unwrap()
        .arg("--db")
        .arg(&db)
        .arg("--table")
        .arg("people")
        .arg("--index")
        .arg("price")
        .assert()
        .success()
        .stderr(predicate::str::contains("idx_people_price"));
}

#[test]
fn index_requires_table_for_multiple_inputs() {
    let dir = TempDir::new().unwrap();
    let people = write_csv(dir.path(), "people.csv", PEOPLE_CSV);
    let orders = write_csv(dir.path(), "orders.csv", "order_id,amount\n100,3\n");
    let db = dir.path().join("out.db");

    // Ambiguous which table to index across two files -> clean error.
    Command::cargo_bin("csv-to-sqlite")
        .unwrap()
        .arg(&people)
        .arg(&orders)
        .arg("-o")
        .arg(&db)
        .arg("--index")
        .arg("id")
        .assert()
        .failure()
        .stderr(predicate::str::contains("--index requires --table"));
}

#[test]
fn missing_db_arg_errors() {
    let dir = TempDir::new().unwrap();
    let csv = write_csv(dir.path(), "people.csv", PEOPLE_CSV);

    // No -o and no --db: should error, not panic.
    Command::cargo_bin("csv-to-sqlite")
        .unwrap()
        .arg(&csv)
        .assert()
        .failure()
        .stderr(predicate::str::contains("output database"));
}

#[test]
fn empty_csv_errors_cleanly() {
    let empty = NamedTempFile::with_suffix(".csv").unwrap();
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("out.db");

    Command::cargo_bin("csv-to-sqlite")
        .unwrap()
        .arg(empty.path())
        .arg("-o")
        .arg(&db)
        .assert()
        .failure()
        .stderr(predicate::str::contains("empty"));
}
