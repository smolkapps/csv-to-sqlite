//! csv-to-sqlite core library.
//!
//! Loads CSV files into SQLite tables with automatic column type inference,
//! bulk-inserts rows inside a single transaction, and runs SQL queries whose
//! results are rendered back as CSV or an ASCII table.

use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{types::Value, Connection, OptionalExtension};
use std::path::Path;

/// SQLite storage class we infer for a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColType {
    Integer,
    Real,
    Text,
}

impl ColType {
    /// The SQLite type keyword used in `CREATE TABLE`.
    pub fn sql_keyword(self) -> &'static str {
        match self {
            ColType::Integer => "INTEGER",
            ColType::Real => "REAL",
            ColType::Text => "TEXT",
        }
    }
}

/// What to do when the target table already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IfExists {
    /// Error out (default).
    Fail,
    /// Drop and recreate the table.
    Replace,
    /// Keep the table and append rows (header must match column count).
    Append,
}

/// Harden a freshly opened [`Connection`] against SQLite's "double-quoted
/// string literal" misfeature (DQS), in both DDL and DML contexts.
///
/// With DQS enabled (SQLite's historical default), a double-quoted token that
/// doesn't resolve to a real identifier is silently reinterpreted as a string
/// literal instead of raising an error. That turns a typo'd or nonexistent
/// column name in `CREATE INDEX ... ("nope")` into a constant string literal,
/// building a useless index over a fixed value while the tool reports success.
/// Disabling DQS makes such mistakes hard errors. Call this immediately after
/// every `Connection::open*`.
pub fn harden_connection(conn: &Connection) -> Result<()> {
    conn.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DQS_DDL, false)
        .context("disabling double-quoted string literals for DDL")?;
    conn.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DQS_DML, false)
        .context("disabling double-quoted string literals for DML")?;
    Ok(())
}

/// Treat a single CSV field as empty (=> NULL / ignored for inference).
fn is_empty(field: &str) -> bool {
    field.trim().is_empty()
}

/// Does this field parse as a 64-bit integer?
///
/// We require the *trimmed* field to round-trip through `i64` so that values
/// like " 42 " count but "42x", "1.0", "0xff", and "" do not.
fn parses_int(field: &str) -> bool {
    let t = field.trim();
    !t.is_empty() && t.parse::<i64>().is_ok()
}

/// Does this field parse as a finite real number?
///
/// `f64::parse` accepts "inf"/"nan"; we reject those so a column of the word
/// "nan" stays TEXT. Integers also parse as f64, which is fine — the column
/// classifier checks the integer predicate first.
fn parses_real(field: &str) -> bool {
    let t = field.trim();
    if t.is_empty() {
        return false;
    }
    match t.parse::<f64>() {
        Ok(v) => v.is_finite(),
        Err(_) => false,
    }
}

/// Infer the [`ColType`] for one column given all of its sampled string values.
///
/// Rules (per spec):
/// - empty values are ignored (they become NULL and don't constrain the type);
/// - INTEGER if every non-empty value parses as an integer;
/// - REAL if every non-empty value parses as a (finite) number;
/// - otherwise TEXT.
///
/// A column that is entirely empty (or has zero rows) defaults to TEXT, the
/// safest catch-all.
pub fn infer_column_type<'a, I>(values: I) -> ColType
where
    I: IntoIterator<Item = &'a str>,
{
    let mut saw_value = false;
    let mut all_int = true;
    let mut all_real = true;

    for v in values {
        if is_empty(v) {
            continue;
        }
        saw_value = true;
        if all_int && !parses_int(v) {
            all_int = false;
        }
        if all_real && !parses_real(v) {
            all_real = false;
        }
        if !all_real {
            // Once it can't be REAL it can't be INTEGER either; stop early.
            break;
        }
    }

    if !saw_value {
        return ColType::Text;
    }
    if all_int {
        ColType::Integer
    } else if all_real {
        ColType::Real
    } else {
        ColType::Text
    }
}

/// A column's parsed string values and inferred type are held column-major so
/// we can infer per-column without re-walking the row matrix.
#[derive(Debug)]
pub struct LoadedTable {
    pub name: String,
    pub columns: Vec<String>,
    pub types: Vec<ColType>,
    /// Row-major data; each inner vec has `columns.len()` fields.
    pub rows: Vec<Vec<String>>,
}

/// Quote a SQL identifier (table/column name) by wrapping in double quotes and
/// doubling any embedded double quotes. Prevents both injection and breakage on
/// names with spaces or punctuation from arbitrary CSV headers.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Derive a table name from a file path: the file stem, sanitized so it's a
/// usable SQL identifier. Non-alphanumeric chars become `_`; a leading digit is
/// prefixed with `_` (SQLite tolerates quoted numeric names, but a clean
/// identifier is friendlier for hand-written queries).
pub fn table_name_from_path(path: &Path) -> String {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("table");
    sanitize_table_name(stem)
}

fn sanitize_table_name(stem: &str) -> String {
    let mut out: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        out.push_str("table");
    }
    if out
        .chars()
        .next()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false)
    {
        out.insert(0, '_');
    }
    out
}

/// Build column headers for a CSV without a header row: `col1..colN`.
fn synthesize_headers(n: usize) -> Vec<String> {
    (1..=n).map(|i| format!("col{i}")).collect()
}

/// Read a CSV file fully into a [`LoadedTable`] (headers + rows + inferred
/// types), without touching SQLite. Pure parsing so it is trivially testable.
pub fn read_csv_table(
    path: &Path,
    table: &str,
    delimiter: u8,
    has_header: bool,
) -> Result<LoadedTable> {
    let mut rdr = csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .has_headers(false) // we always read records manually so we control header logic
        .flexible(true)
        .from_path(path)
        .with_context(|| format!("opening CSV {}", path.display()))?;

    let mut records = rdr.records();

    // Determine headers.
    let (columns, first_data_row): (Vec<String>, Option<Vec<String>>) = if has_header {
        match records.next() {
            Some(rec) => {
                let rec = rec.context("reading CSV header row")?;
                let cols: Vec<String> = rec.iter().map(|s| s.to_string()).collect();
                if cols.is_empty() {
                    bail!("CSV {} has an empty header row", path.display());
                }
                (cols, None)
            }
            None => bail!("CSV {} is empty (no header row)", path.display()),
        }
    } else {
        // Peek the first record to size synthesized headers.
        match records.next() {
            Some(rec) => {
                let rec = rec.context("reading first CSV row")?;
                let first: Vec<String> = rec.iter().map(|s| s.to_string()).collect();
                let cols = synthesize_headers(first.len());
                (cols, Some(first))
            }
            None => bail!("CSV {} is empty", path.display()),
        }
    };

    let ncols = columns.len();
    let mut rows: Vec<Vec<String>> = Vec::new();

    let push_row = |mut fields: Vec<String>, rows: &mut Vec<Vec<String>>| {
        // Normalize ragged rows to exactly `ncols` fields.
        if fields.len() < ncols {
            fields.resize(ncols, String::new());
        } else if fields.len() > ncols {
            fields.truncate(ncols);
        }
        rows.push(fields);
    };

    if let Some(first) = first_data_row {
        push_row(first, &mut rows);
    }

    for rec in records {
        let rec = rec.context("reading CSV data row")?;
        let fields: Vec<String> = rec.iter().map(|s| s.to_string()).collect();
        push_row(fields, &mut rows);
    }

    // Infer types column by column.
    let mut types = Vec::with_capacity(ncols);
    for c in 0..ncols {
        let col_vals = rows.iter().map(|r| r[c].as_str());
        types.push(infer_column_type(col_vals));
    }

    Ok(LoadedTable {
        name: table.to_string(),
        columns,
        types,
        rows,
    })
}

/// Generate the `CREATE TABLE` DDL for a loaded table.
pub fn create_table_ddl(table: &LoadedTable) -> String {
    let cols: Vec<String> = table
        .columns
        .iter()
        .zip(&table.types)
        .map(|(name, ty)| format!("{} {}", quote_ident(name), ty.sql_keyword()))
        .collect();
    format!(
        "CREATE TABLE {} ({})",
        quote_ident(&table.name),
        cols.join(", ")
    )
}

/// Convert a raw CSV string field to the [`rusqlite::types::Value`] to bind,
/// honoring the inferred column type. Empty fields become NULL regardless of
/// type. Values that "should" parse but don't (shouldn't happen given the
/// inference, but be defensive) fall back to TEXT.
fn field_to_value(field: &str, ty: ColType) -> Value {
    if is_empty(field) {
        return Value::Null;
    }
    let t = field.trim();
    match ty {
        ColType::Integer => match t.parse::<i64>() {
            Ok(i) => Value::Integer(i),
            Err(_) => Value::Text(field.to_string()),
        },
        ColType::Real => match t.parse::<f64>() {
            Ok(f) => Value::Real(f),
            Err(_) => Value::Text(field.to_string()),
        },
        ColType::Text => Value::Text(field.to_string()),
    }
}

/// Create the table (respecting `if_exists`) and bulk-insert every row inside a
/// single transaction. Returns the number of rows inserted.
pub fn load_table_into_db(
    conn: &mut Connection,
    table: &LoadedTable,
    if_exists: IfExists,
) -> Result<usize> {
    let exists = table_exists(conn, &table.name)?;

    match (exists, if_exists) {
        (true, IfExists::Fail) => bail!(
            "table \"{}\" already exists (use --if-exists replace|append)",
            table.name
        ),
        (true, IfExists::Replace) => {
            conn.execute(&format!("DROP TABLE {}", quote_ident(&table.name)), [])
                .with_context(|| format!("dropping existing table {}", table.name))?;
            conn.execute_batch(&create_table_ddl(table))
                .with_context(|| format!("creating table {}", table.name))?;
        }
        (true, IfExists::Append) => {
            // Verify the existing table's column count matches so we don't bind
            // a mismatched number of params.
            let existing_cols = table_column_count(conn, &table.name)?;
            if existing_cols != table.columns.len() {
                bail!(
                    "cannot append to \"{}\": existing table has {} columns but CSV has {}",
                    table.name,
                    existing_cols,
                    table.columns.len()
                );
            }
        }
        (false, _) => {
            conn.execute_batch(&create_table_ddl(table))
                .with_context(|| format!("creating table {}", table.name))?;
        }
    }

    insert_rows(conn, table)
}

/// Bulk-insert `table.rows` into an existing table within one transaction.
fn insert_rows(conn: &mut Connection, table: &LoadedTable) -> Result<usize> {
    let ncols = table.columns.len();
    let placeholders = (1..=ncols)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let col_list = table
        .columns
        .iter()
        .map(|c| quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "INSERT INTO {} ({}) VALUES ({})",
        quote_ident(&table.name),
        col_list,
        placeholders
    );

    let tx = conn.transaction().context("starting insert transaction")?;
    let mut inserted = 0usize;
    {
        let mut stmt = tx.prepare(&sql).context("preparing INSERT statement")?;
        for row in &table.rows {
            let values: Vec<Value> = row
                .iter()
                .zip(&table.types)
                .map(|(field, ty)| field_to_value(field, *ty))
                .collect();
            let params = rusqlite::params_from_iter(values.iter());
            stmt.execute(params).context("inserting row")?;
            inserted += 1;
        }
    }
    tx.commit().context("committing insert transaction")?;
    Ok(inserted)
}

/// Sanitize a string into a component safe to embed in a generated identifier
/// (used to build index names): non-alphanumerics become `_`. Never empty.
fn sanitize_ident_part(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        "_".to_string()
    } else {
        out
    }
}

/// Derive a deterministic index name from a table and its columns, e.g.
/// `idx_people_name` or `idx_people_name_price` for a composite index.
fn index_name(table: &str, columns: &[String]) -> String {
    let mut parts = vec![sanitize_ident_part(table)];
    parts.extend(columns.iter().map(|c| sanitize_ident_part(c)));
    format!("idx_{}", parts.join("_"))
}

/// Whether [`create_index`] actually created a new index or found an identical
/// one already present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexOutcome {
    /// A new index was created.
    Created,
    /// An identical index (same table and columns) already existed; nothing
    /// was created.
    AlreadyExists,
}

/// An existing index's target table and indexed column list, as read back from
/// the schema.
struct ExistingIndex {
    table: String,
    columns: Vec<String>,
}

/// Case-insensitive comparison of two identifier lists (SQLite resolves table
/// and column names case-insensitively).
fn same_idents(a: &[String], b: &[String]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

/// Look up an existing index by name, returning the table it's on and its
/// indexed columns, or `None` if no index with that name exists.
fn existing_index(conn: &Connection, name: &str) -> Result<Option<ExistingIndex>> {
    let tbl: Option<String> = conn
        .query_row(
            "SELECT tbl_name FROM sqlite_master WHERE type='index' AND name=?1",
            [name],
            |r| r.get(0),
        )
        .optional()
        .context("looking up existing index")?;
    let Some(table) = tbl else {
        return Ok(None);
    };
    let mut stmt = conn.prepare(&format!("PRAGMA index_info({})", quote_ident(name)))?;
    let columns: Vec<String> = stmt
        .query_map([], |r| r.get::<_, Option<String>>(2))?
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect();
    Ok(Some(ExistingIndex { table, columns }))
}

/// The column names of an existing table, in declaration order.
fn table_columns(conn: &Connection, name: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", quote_ident(name)))?;
    let cols = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(cols)
}

/// Create an index on one or more `columns` of `table`.
///
/// Identifiers are quoted (so column names with spaces/punctuation work).
/// Returns the generated index name and whether a new index was created or an
/// identical one already existed. Passing multiple columns creates a single
/// composite index over them, in order.
///
/// Two safety checks guard against silent no-ops that would otherwise report a
/// false success:
/// - every requested column must exist on `table` (otherwise we'd build an
///   index over a nonexistent/typo'd column);
/// - the derived index name is checked against the schema. Because
///   [`index_name`] flattens the table/column boundary, distinct
///   `(table, columns)` inputs can collapse to the same name; if a *different*
///   index already owns that name we [`bail!`] with a collision error instead
///   of issuing a `CREATE INDEX IF NOT EXISTS` that silently does nothing.
pub fn create_index(
    conn: &Connection,
    table: &str,
    columns: &[String],
) -> Result<(String, IndexOutcome)> {
    if columns.is_empty() {
        bail!("cannot create an index with no columns");
    }

    // Validate the target table and every requested column up front, so a typo
    // is a clear error rather than (with DQS) a constant-valued index.
    if !table_exists(conn, table)? {
        bail!("cannot index table \"{table}\": no such table");
    }
    let existing_cols = table_columns(conn, table)?;
    for col in columns {
        if !existing_cols.iter().any(|c| c.eq_ignore_ascii_case(col)) {
            bail!("cannot index \"{table}\": no such column \"{col}\"");
        }
    }

    let idx_name = index_name(table, columns);

    // Guard against derived-name collisions. If an index with this name already
    // exists, either it is the very same index (idempotent no-op) or a distinct
    // (table, columns) request collapsed onto the same name (a bug we must not
    // paper over with a success message).
    if let Some(existing) = existing_index(conn, &idx_name)? {
        if existing.table.eq_ignore_ascii_case(table) && same_idents(&existing.columns, columns) {
            return Ok((idx_name, IndexOutcome::AlreadyExists));
        }
        bail!(
            "index name \"{}\" already exists on \"{}\"({}); refusing to create a \
             different index on \"{}\"({}) whose derived name collides with it",
            idx_name,
            existing.table,
            existing.columns.join(", "),
            table,
            columns.join(", ")
        );
    }

    let col_list = columns
        .iter()
        .map(|c| quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "CREATE INDEX {} ON {} ({})",
        quote_ident(&idx_name),
        quote_ident(table),
        col_list
    );
    conn.execute(&sql, [])
        .with_context(|| format!("creating index on {}({})", table, columns.join(", ")))?;
    Ok((idx_name, IndexOutcome::Created))
}

/// Does a table with this name exist in the main schema?
pub fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |r| r.get(0),
    )?;
    Ok(count > 0)
}

/// Number of columns in an existing table.
fn table_column_count(conn: &Connection, name: &str) -> Result<usize> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", quote_ident(name)))?;
    let n = stmt.query_map([], |_| Ok(()))?.count();
    Ok(n)
}

/// How a query result should be rendered to stdout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Csv,
    Table,
}

/// Run a SQL query and return the rendered output (header + rows) as a String.
///
/// Works for any read query; column names come from the prepared statement so
/// `SELECT *`, aggregates, and aliases all render correctly.
pub fn run_query(conn: &Connection, sql: &str, format: OutputFormat) -> Result<String> {
    let mut stmt = conn
        .prepare(sql)
        .with_context(|| format!("preparing query: {sql}"))?;
    let ncols = stmt.column_count();
    let headers: Vec<String> = (0..ncols)
        .map(|i| stmt.column_name(i).map(|s| s.to_string()))
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("reading query column names")?;

    let mut rows_out: Vec<Vec<String>> = Vec::new();
    let mut query_rows = stmt.query([]).context("executing query")?;
    while let Some(row) = query_rows.next().context("fetching query row")? {
        let mut out_row = Vec::with_capacity(ncols);
        for i in 0..ncols {
            let v: Value = row.get(i).context("reading query cell")?;
            out_row.push(value_to_display(&v));
        }
        rows_out.push(out_row);
    }

    match format {
        OutputFormat::Csv => render_csv(&headers, &rows_out),
        OutputFormat::Table => Ok(render_table(&headers, &rows_out)),
    }
}

/// Render a SQLite value as a display string for output.
fn value_to_display(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Real(f) => {
            // Avoid trailing ".0" surprises being lost: format compactly but
            // keep it a valid number representation.
            let s = f.to_string();
            s
        }
        Value::Text(t) => t.clone(),
        Value::Blob(b) => format!("<{} bytes blob>", b.len()),
    }
}

/// Render rows as RFC-4180 CSV using the `csv` crate (proper quoting/escaping).
fn render_csv(headers: &[String], rows: &[Vec<String>]) -> Result<String> {
    let mut wtr = csv::Writer::from_writer(vec![]);
    wtr.write_record(headers).context("writing CSV header")?;
    for row in rows {
        wtr.write_record(row).context("writing CSV row")?;
    }
    wtr.flush().context("flushing CSV output")?;
    let bytes = wtr
        .into_inner()
        .map_err(|e| anyhow!("finalizing CSV output: {e}"))?;
    String::from_utf8(bytes).context("CSV output was not valid UTF-8")
}

/// Render rows as a simple aligned ASCII table.
fn render_table(headers: &[String], rows: &[Vec<String>]) -> String {
    let ncols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < ncols {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }
    }

    let sep = |widths: &[usize]| -> String {
        let parts: Vec<String> = widths.iter().map(|w| "-".repeat(w + 2)).collect();
        format!("+{}+", parts.join("+"))
    };
    let fmt_row = |cells: &[String], widths: &[usize]| -> String {
        let parts: Vec<String> = (0..ncols)
            .map(|i| {
                let c = cells.get(i).map(|s| s.as_str()).unwrap_or("");
                let pad = widths[i].saturating_sub(c.chars().count());
                format!(" {}{} ", c, " ".repeat(pad))
            })
            .collect();
        format!("|{}|", parts.join("|"))
    };

    let mut out = String::new();
    out.push_str(&sep(&widths));
    out.push('\n');
    out.push_str(&fmt_row(headers, &widths));
    out.push('\n');
    out.push_str(&sep(&widths));
    out.push('\n');
    for row in rows {
        out.push_str(&fmt_row(row, &widths));
        out.push('\n');
    }
    out.push_str(&sep(&widths));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infer_integer_column() {
        let vals = vec!["1", "2", "42", "-7"];
        assert_eq!(infer_column_type(vals), ColType::Integer);
    }

    #[test]
    fn infer_real_column() {
        let vals = vec!["1.5", "2", "3.14", "-0.001"];
        assert_eq!(infer_column_type(vals), ColType::Real);
    }

    #[test]
    fn infer_text_column() {
        let vals = vec!["alice", "bob", "42"];
        assert_eq!(infer_column_type(vals), ColType::Text);
    }

    #[test]
    fn empty_values_are_ignored_for_inference() {
        // All non-empty values are ints; blanks should not force TEXT.
        let vals = vec!["1", "", "  ", "3"];
        assert_eq!(infer_column_type(vals), ColType::Integer);
    }

    #[test]
    fn all_empty_column_is_text() {
        let vals = vec!["", "   ", ""];
        assert_eq!(infer_column_type(vals), ColType::Text);
    }

    #[test]
    fn inf_and_nan_stay_text() {
        assert_eq!(infer_column_type(vec!["inf", "1.0"]), ColType::Text);
        assert_eq!(infer_column_type(vec!["nan"]), ColType::Text);
    }

    #[test]
    fn int_overflowing_i64_is_real_or_text() {
        // 99999999999999999999 does not fit i64 but parses as f64 -> REAL.
        let vals = vec!["99999999999999999999"];
        assert_eq!(infer_column_type(vals), ColType::Real);
    }

    #[test]
    fn sanitize_table_names() {
        assert_eq!(sanitize_table_name("my data"), "my_data");
        assert_eq!(sanitize_table_name("123abc"), "_123abc");
        assert_eq!(sanitize_table_name("clean_name"), "clean_name");
    }

    #[test]
    fn ddl_quotes_identifiers() {
        let t = LoadedTable {
            name: "people".into(),
            columns: vec!["id".into(), "full name".into()],
            types: vec![ColType::Integer, ColType::Text],
            rows: vec![],
        };
        let ddl = create_table_ddl(&t);
        assert_eq!(
            ddl,
            r#"CREATE TABLE "people" ("id" INTEGER, "full name" TEXT)"#
        );
    }

    #[test]
    fn index_names_are_derived_and_sanitized() {
        assert_eq!(index_name("people", &["name".into()]), "idx_people_name");
        assert_eq!(
            index_name("people", &["name".into(), "price".into()]),
            "idx_people_name_price"
        );
        // Non-alphanumeric characters in the table/column collapse to `_`.
        assert_eq!(
            index_name("my table", &["full name".into()]),
            "idx_my_table_full_name"
        );
    }

    #[test]
    fn field_to_value_empty_is_null() {
        assert_eq!(field_to_value("", ColType::Integer), Value::Null);
        assert_eq!(field_to_value("  ", ColType::Text), Value::Null);
    }

    #[test]
    fn field_to_value_typed() {
        assert_eq!(field_to_value("42", ColType::Integer), Value::Integer(42));
        assert_eq!(field_to_value("3.5", ColType::Real), Value::Real(3.5));
        assert_eq!(
            field_to_value("hi", ColType::Text),
            Value::Text("hi".into())
        );
    }
}
