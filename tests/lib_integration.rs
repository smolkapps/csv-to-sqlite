//! Library-level integration tests: load CSVs into a real (in-memory or temp)
//! SQLite database and assert row counts, inferred types, and query results.

use csv_to_sqlite::{
    create_table_ddl, load_table_into_db, read_csv_table, run_query, ColType, IfExists,
    OutputFormat,
};
use rusqlite::Connection;
use std::io::Write;
use std::path::Path;
use tempfile::NamedTempFile;

/// Write `contents` to a temp file whose name ends with `.csv`, returning the
/// handle (kept alive so the file persists for the test's duration).
fn csv_file(contents: &str) -> NamedTempFile {
    let mut f = tempfile::Builder::new()
        .suffix(".csv")
        .tempfile()
        .expect("create temp csv");
    f.write_all(contents.as_bytes()).expect("write csv");
    f.flush().expect("flush csv");
    f
}

const PEOPLE_CSV: &str = "id,name,price\n1,alice,9.99\n2,bob,19.50\n3,carol,5.00\n";

#[test]
fn load_infers_types_and_counts_rows() {
    let f = csv_file(PEOPLE_CSV);
    let table = read_csv_table(Path::new(f.path()), "people", b',', true).unwrap();

    // Inferred types: id INTEGER, name TEXT, price REAL.
    assert_eq!(table.columns, vec!["id", "name", "price"]);
    assert_eq!(
        table.types,
        vec![ColType::Integer, ColType::Text, ColType::Real]
    );
    assert_eq!(table.rows.len(), 3);

    let mut conn = Connection::open_in_memory().unwrap();
    let n = load_table_into_db(&mut conn, &table, IfExists::Fail).unwrap();
    assert_eq!(n, 3);

    // SELECT COUNT(*) returns 3.
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM people", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 3);

    // The declared column types match what we inferred.
    let id_type: String = conn
        .query_row(
            "SELECT type FROM pragma_table_info('people') WHERE name='id'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let name_type: String = conn
        .query_row(
            "SELECT type FROM pragma_table_info('people') WHERE name='name'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let price_type: String = conn
        .query_row(
            "SELECT type FROM pragma_table_info('people') WHERE name='price'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(id_type, "INTEGER");
    assert_eq!(name_type, "TEXT");
    assert_eq!(price_type, "REAL");
}

#[test]
fn select_where_returns_expected_row() {
    let f = csv_file(PEOPLE_CSV);
    let table = read_csv_table(Path::new(f.path()), "people", b',', true).unwrap();
    let mut conn = Connection::open_in_memory().unwrap();
    load_table_into_db(&mut conn, &table, IfExists::Fail).unwrap();

    // WHERE on the numeric column relies on REAL typing working.
    let name: String = conn
        .query_row("SELECT name FROM people WHERE price > 10", [], |r| r.get(0))
        .unwrap();
    assert_eq!(name, "bob");

    // Sum proves values stored as numbers, not text.
    let total: f64 = conn
        .query_row("SELECT SUM(price) FROM people", [], |r| r.get(0))
        .unwrap();
    assert!((total - 34.49).abs() < 1e-9, "got {total}");
}

#[test]
fn if_exists_append_adds_rows() {
    let f1 = csv_file(PEOPLE_CSV);
    let table1 = read_csv_table(Path::new(f1.path()), "people", b',', true).unwrap();
    let mut conn = Connection::open_in_memory().unwrap();
    load_table_into_db(&mut conn, &table1, IfExists::Fail).unwrap();

    let more = "id,name,price\n4,dave,1.00\n5,erin,2.00\n";
    let f2 = csv_file(more);
    let table2 = read_csv_table(Path::new(f2.path()), "people", b',', true).unwrap();
    let n = load_table_into_db(&mut conn, &table2, IfExists::Append).unwrap();
    assert_eq!(n, 2);

    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM people", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 5);
}

#[test]
fn if_exists_fail_errors_when_table_present() {
    let f = csv_file(PEOPLE_CSV);
    let table = read_csv_table(Path::new(f.path()), "people", b',', true).unwrap();
    let mut conn = Connection::open_in_memory().unwrap();
    load_table_into_db(&mut conn, &table, IfExists::Fail).unwrap();

    // Second load with Fail must error.
    let err = load_table_into_db(&mut conn, &table, IfExists::Fail).unwrap_err();
    assert!(
        err.to_string().contains("already exists"),
        "unexpected error: {err}"
    );
}

#[test]
fn if_exists_replace_overwrites() {
    let f = csv_file(PEOPLE_CSV);
    let table = read_csv_table(Path::new(f.path()), "people", b',', true).unwrap();
    let mut conn = Connection::open_in_memory().unwrap();
    load_table_into_db(&mut conn, &table, IfExists::Fail).unwrap();

    let smaller = "id,name,price\n9,zoe,7.77\n";
    let f2 = csv_file(smaller);
    let table2 = read_csv_table(Path::new(f2.path()), "people", b',', true).unwrap();
    load_table_into_db(&mut conn, &table2, IfExists::Replace).unwrap();

    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM people", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn multi_file_makes_two_tables() {
    let people = csv_file(PEOPLE_CSV);
    let orders = csv_file("order_id,amount\n100,3\n101,4\n102,5\n");

    let t_people = read_csv_table(Path::new(people.path()), "people", b',', true).unwrap();
    let t_orders = read_csv_table(Path::new(orders.path()), "orders", b',', true).unwrap();

    let mut conn = Connection::open_in_memory().unwrap();
    load_table_into_db(&mut conn, &t_people, IfExists::Fail).unwrap();
    load_table_into_db(&mut conn, &t_orders, IfExists::Fail).unwrap();

    let tables: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('people','orders')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tables, 2);

    let people_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM people", [], |r| r.get(0))
        .unwrap();
    let orders_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM orders", [], |r| r.get(0))
        .unwrap();
    assert_eq!(people_count, 3);
    assert_eq!(orders_count, 3);

    // amount column is INTEGER.
    let amount_type: String = conn
        .query_row(
            "SELECT type FROM pragma_table_info('orders') WHERE name='amount'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(amount_type, "INTEGER");
}

#[test]
fn no_header_synthesizes_columns() {
    let raw = "1,alice,9.99\n2,bob,19.50\n";
    let f = csv_file(raw);
    let table = read_csv_table(Path::new(f.path()), "nohdr", b',', false).unwrap();

    assert_eq!(table.columns, vec!["col1", "col2", "col3"]);
    assert_eq!(table.rows.len(), 2);
    assert_eq!(
        table.types,
        vec![ColType::Integer, ColType::Text, ColType::Real]
    );

    let mut conn = Connection::open_in_memory().unwrap();
    let n = load_table_into_db(&mut conn, &table, IfExists::Fail).unwrap();
    assert_eq!(n, 2);

    let v: String = conn
        .query_row("SELECT col2 FROM nohdr WHERE col1 = 2", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, "bob");
}

#[test]
fn custom_delimiter_semicolon() {
    let raw = "a;b\n1;x\n2;y\n";
    let f = csv_file(raw);
    let table = read_csv_table(Path::new(f.path()), "semi", b';', true).unwrap();
    assert_eq!(table.columns, vec!["a", "b"]);
    assert_eq!(table.rows.len(), 2);
    assert_eq!(table.types[0], ColType::Integer);
    assert_eq!(table.types[1], ColType::Text);
}

#[test]
fn empty_field_becomes_null() {
    let raw = "id,note\n1,hello\n2,\n";
    let f = csv_file(raw);
    let table = read_csv_table(Path::new(f.path()), "t", b',', true).unwrap();
    let mut conn = Connection::open_in_memory().unwrap();
    load_table_into_db(&mut conn, &table, IfExists::Fail).unwrap();

    let nulls: i64 = conn
        .query_row("SELECT COUNT(*) FROM t WHERE note IS NULL", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(nulls, 1);
}

#[test]
fn query_renders_csv_output() {
    let f = csv_file(PEOPLE_CSV);
    let table = read_csv_table(Path::new(f.path()), "people", b',', true).unwrap();
    let mut conn = Connection::open_in_memory().unwrap();
    load_table_into_db(&mut conn, &table, IfExists::Fail).unwrap();

    let out = run_query(
        &conn,
        "SELECT name, price FROM people ORDER BY id",
        OutputFormat::Csv,
    )
    .unwrap();
    // Note: SQLite stores 5.00 as the REAL 5.0, and Rust's f64::to_string
    // renders that as "5" (no trailing .0). 19.50 -> 19.5 likewise.
    let expected = "name,price\nalice,9.99\nbob,19.5\ncarol,5\n";
    assert_eq!(out, expected);
}

#[test]
fn query_renders_table_output() {
    let f = csv_file(PEOPLE_CSV);
    let table = read_csv_table(Path::new(f.path()), "people", b',', true).unwrap();
    let mut conn = Connection::open_in_memory().unwrap();
    load_table_into_db(&mut conn, &table, IfExists::Fail).unwrap();

    let out = run_query(
        &conn,
        "SELECT COUNT(*) AS n FROM people",
        OutputFormat::Table,
    )
    .unwrap();
    // The header name and the value 3 must both appear.
    assert!(out.contains("n"), "table output: {out}");
    assert!(out.contains("3"), "table output: {out}");
    assert!(out.contains("+"), "table output should have borders: {out}");
}

#[test]
fn loads_into_a_real_db_file_on_disk() {
    let f = csv_file(PEOPLE_CSV);
    let db = NamedTempFile::new().unwrap();
    let table = read_csv_table(Path::new(f.path()), "people", b',', true).unwrap();
    {
        let mut conn = Connection::open(db.path()).unwrap();
        load_table_into_db(&mut conn, &table, IfExists::Fail).unwrap();
    }
    // Reopen to confirm it persisted to the file.
    let conn = Connection::open(db.path()).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM people", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 3);
}

#[test]
fn ddl_matches_inferred_types() {
    let f = csv_file(PEOPLE_CSV);
    let table = read_csv_table(Path::new(f.path()), "people", b',', true).unwrap();
    let ddl = create_table_ddl(&table);
    assert_eq!(
        ddl,
        r#"CREATE TABLE "people" ("id" INTEGER, "name" TEXT, "price" REAL)"#
    );
}
