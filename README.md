# csv-to-sqlite

Load CSV files into a queryable SQLite database, with automatic per-column type
inference and a built-in query runner. A single, dependency-light Rust CLI.

## Features

- **Type inference** — each column becomes `INTEGER` if every value parses as an
  integer, `REAL` if every value is numeric, otherwise `TEXT`. Empty cells are
  stored as `NULL` and don't constrain the inferred type.
- **Bulk load in a transaction** — all rows for a file are inserted inside one
  SQLite transaction (fast and atomic).
- **One table per file** — pass several CSVs and each lands in its own table.
- **Query runner** — run a `SELECT` after loading (or against an existing
  database) and print results as an ASCII table or CSV.
- **Index creation** — add one or more indexes after loading with `--index`,
  including composite indexes, to speed up later queries.

## Install / build

Requires a Rust toolchain. SQLite is compiled from source via the `rusqlite`
`bundled` feature, so there is no system SQLite dependency.

```sh
cargo build --release
# binary at target/release/csv-to-sqlite
```

## Usage

```
csv-to-sqlite [OPTIONS] [CSV]...

Arguments:
  [CSV]...  CSV file(s) to load. One table is created per file.

Options:
  -o, --output <DB>       Output SQLite database file to create/append to
      --db <DB>           Query an existing database without loading any CSV
      --table <NAME>      Table name (single input only; defaults to file stem)
      --if-exists <WHAT>  replace | append | fail   [default: fail]
      --delim <CHAR>      Field delimiter (use \t for tab)   [default: ,]
      --no-header         CSV has no header; synthesize col1..colN
      --index <COL,...>   Index column(s) after loading (repeatable; composite)
      --query <SQL>       Run a query after loading and print results
      --format <FMT>      csv | table   [default: table]
  -h, --help              Print help
  -V, --version           Print version
```

### Examples

Load a CSV into a new database (table named from the filename):

```sh
csv-to-sqlite data.csv -o data.db
```

Load with an explicit table name and run a query:

```sh
csv-to-sqlite sales.csv -o shop.db --table sales \
  --query "SELECT region, SUM(total) FROM sales GROUP BY region"
```

Load several files (one table each):

```sh
csv-to-sqlite customers.csv orders.csv -o shop.db
```

Append more rows to an existing table:

```sh
csv-to-sqlite more_sales.csv -o shop.db --table sales --if-exists append
```

Load and build indexes to speed up later lookups (repeat `--index` for more;
comma-separate columns for a composite index):

```sh
csv-to-sqlite sales.csv -o shop.db --table sales \
  --index region --index "year,month"
```

Index an already-loaded table (name it with `--table`):

```sh
csv-to-sqlite --db shop.db --table sales --index region
```

Query an existing database without loading anything, as CSV:

```sh
csv-to-sqlite --db shop.db --format csv \
  --query "SELECT * FROM sales WHERE total > 100"
```

A headerless, tab-separated file:

```sh
csv-to-sqlite raw.tsv -o raw.db --no-header --delim '\t'
```

## How type inference works

For each column, every non-empty value is tested:

| Condition                                   | Inferred type |
|---------------------------------------------|---------------|
| all values parse as `i64`                   | `INTEGER`     |
| all values parse as finite `f64` (not all int) | `REAL`     |
| anything else, or column entirely empty     | `TEXT`        |

`inf`/`nan` are treated as text (so a column literally containing "nan" stays
`TEXT`). Integers too large for `i64` fall through to `REAL`.

## License

MIT — see [LICENSE](LICENSE).
