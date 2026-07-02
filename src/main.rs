//! Thin CLI front-end over the `csv_to_sqlite` library.

use anyhow::{bail, Context, Result};
use clap::Parser;
use csv_to_sqlite::{
    create_index, harden_connection, load_table_into_db, read_csv_table, run_query,
    table_name_from_path, IfExists, IndexOutcome, OutputFormat,
};
use rusqlite::Connection;
use std::path::PathBuf;

/// Load CSV files into a queryable SQLite database with automatic type inference.
#[derive(Parser, Debug)]
#[command(name = "csv-to-sqlite", version, about, long_about = None)]
struct Cli {
    /// CSV file(s) to load. One table is created per file.
    #[arg(value_name = "CSV")]
    inputs: Vec<PathBuf>,

    /// Output SQLite database file to create/append to.
    #[arg(short = 'o', long = "output", value_name = "DB")]
    output: Option<PathBuf>,

    /// Operate on an existing database without loading any CSV (for --query).
    #[arg(long = "db", value_name = "DB", conflicts_with = "output")]
    db: Option<PathBuf>,

    /// Table name (only valid with a single input; defaults to the file stem).
    #[arg(long = "table", value_name = "NAME")]
    table: Option<String>,

    /// Behavior when a target table already exists.
    #[arg(long = "if-exists", value_enum, default_value_t = IfExistsArg::Fail)]
    if_exists: IfExistsArg,

    /// Field delimiter (single character). Defaults to comma.
    #[arg(long = "delim", value_name = "CHAR", default_value = ",")]
    delim: String,

    /// CSV has no header row; synthesize columns col1..colN.
    #[arg(long = "no-header", action = clap::ArgAction::SetTrue)]
    no_header: bool,

    /// Create an index on the given column(s) after loading. Repeatable; a
    /// value may be a comma-separated list of columns for a composite index
    /// (e.g. --index region --index "year,month").
    #[arg(long = "index", value_name = "COL[,COL...]")]
    index: Vec<String>,

    /// Run this SQL query after loading (or on --db) and print results.
    #[arg(long = "query", value_name = "SQL")]
    query: Option<String>,

    /// Output format for --query results.
    #[arg(long = "format", value_enum, default_value_t = FormatArg::Table)]
    format: FormatArg,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
enum IfExistsArg {
    Fail,
    Replace,
    Append,
}

impl From<IfExistsArg> for IfExists {
    fn from(a: IfExistsArg) -> Self {
        match a {
            IfExistsArg::Fail => IfExists::Fail,
            IfExistsArg::Replace => IfExists::Replace,
            IfExistsArg::Append => IfExists::Append,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
enum FormatArg {
    Csv,
    Table,
}

impl From<FormatArg> for OutputFormat {
    fn from(a: FormatArg) -> Self {
        match a {
            FormatArg::Csv => OutputFormat::Csv,
            FormatArg::Table => OutputFormat::Table,
        }
    }
}

fn delimiter_byte(s: &str) -> Result<u8> {
    // Allow common escape spellings for tab.
    let resolved = match s {
        "\\t" | "tab" | "TAB" => "\t",
        other => other,
    };
    let bytes = resolved.as_bytes();
    if bytes.len() != 1 {
        bail!(
            "--delim must be a single byte character (got {:?}); use \\t for tab",
            s
        );
    }
    Ok(bytes[0])
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    if cli.table.is_some() && cli.inputs.len() > 1 {
        bail!("--table cannot be used with multiple input files");
    }

    let delimiter = delimiter_byte(&cli.delim)?;
    let has_header = !cli.no_header;

    // Determine the database path / mode.
    // - With --db: open existing DB, no loading (inputs not allowed).
    // - With -o/--output: create/open DB and load inputs.
    let mut conn = if let Some(db_path) = &cli.db {
        if !cli.inputs.is_empty() {
            bail!("--db is for querying an existing database; do not pass CSV inputs (use -o to load)");
        }
        Connection::open(db_path)
            .with_context(|| format!("opening database {}", db_path.display()))?
    } else if let Some(out_path) = &cli.output {
        if cli.inputs.is_empty() {
            bail!("no input CSV files given (pass at least one, with -o DB)");
        }
        Connection::open(out_path)
            .with_context(|| format!("creating/opening database {}", out_path.display()))?
    } else if cli.query.is_some() && cli.inputs.is_empty() {
        // --query with neither -o nor --db: query an in-memory DB (rarely useful,
        // but well-defined: yields nothing unless the query is self-contained).
        Connection::open_in_memory().context("opening in-memory database")?
    } else {
        bail!("must specify an output database with -o DB (to load) or --db DB (to query)");
    };

    // Disable SQLite's double-quoted-string-literal misfeature so a typo'd
    // column name is an error, not a silently constant index.
    harden_connection(&conn)?;

    // Load each input CSV as its own table.
    if cli.db.is_none() {
        for input in &cli.inputs {
            let table_name = match &cli.table {
                Some(name) => name.clone(),
                None => table_name_from_path(input),
            };
            let loaded = read_csv_table(input, &table_name, delimiter, has_header)
                .with_context(|| format!("reading CSV {}", input.display()))?;
            let n = load_table_into_db(&mut conn, &loaded, cli.if_exists.into())
                .with_context(|| format!("loading table {table_name}"))?;
            eprintln!(
                "Loaded {n} row(s) into table \"{table_name}\" ({} column(s))",
                loaded.columns.len()
            );
        }
    }

    // Create any requested indexes (after loading, before querying so the
    // query planner can use them).
    if !cli.index.is_empty() {
        let table_name = match (&cli.table, cli.inputs.len()) {
            (Some(name), _) => name.clone(),
            (None, 1) => table_name_from_path(&cli.inputs[0]),
            (None, 0) => {
                bail!("--index requires --table to name the target table when not loading a CSV")
            }
            (None, _) => {
                bail!("--index requires --table to pick which table to index when loading multiple files")
            }
        };
        for spec in &cli.index {
            let cols: Vec<String> = spec
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if cols.is_empty() {
                bail!("--index requires at least one column name (got {spec:?})");
            }
            let (idx, outcome) = create_index(&conn, &table_name, &cols)
                .with_context(|| format!("indexing table {table_name}"))?;
            match outcome {
                IndexOutcome::Created => eprintln!(
                    "Created index \"{idx}\" on \"{table_name}\" ({})",
                    cols.join(", ")
                ),
                IndexOutcome::AlreadyExists => eprintln!(
                    "Index \"{idx}\" already exists on \"{table_name}\" ({}); skipping",
                    cols.join(", ")
                ),
            }
        }
    }

    // Run the query, if any.
    if let Some(sql) = &cli.query {
        let out = run_query(&conn, sql, cli.format.into())?;
        println!("{out}");
    }

    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
