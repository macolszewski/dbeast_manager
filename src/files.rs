//! Data files opened ad hoc, without adding a permanent data source: CSV/TSV,
//! Frictionless Data Packages (datapackage.json) and SQLite database files.
//!
//! CSV data is imported into a temporary SQLite database, so the regular
//! grid, SQL console and editing work on it; edits are written back to the
//! CSV file. Column types are only inferred when every value converts back to
//! the exact same text, so saving never reformats untouched cells.

use crate::model::{DataSource, DriverKind};
use anyhow::{Context, Result, bail};
use rusqlite::types::ValueRef;
use std::io::BufRead;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    Csv,
    DataPackage,
    Sqlite,
}

impl FileKind {
    pub fn label(self) -> &'static str {
        match self {
            FileKind::Csv => "CSV",
            FileKind::DataPackage => "Data Package",
            FileKind::Sqlite => "SQLite",
        }
    }
}

/// Extensions offered in file dialogs.
pub const EXTENSIONS: &[&str] = &["csv", "tsv", "json", "sqlite", "sqlite3", "db", "db3"];

/// Supported format of a path, by name: `None` for everything else.
pub fn kind_of(path: &Path) -> Option<FileKind> {
    let name = path.file_name()?.to_str()?.to_ascii_lowercase();
    if name == "datapackage.json" || name.ends_with(".datapackage.json") {
        return Some(FileKind::DataPackage);
    }
    match name.rsplit_once('.')?.1 {
        "csv" | "tsv" => Some(FileKind::Csv),
        "sqlite" | "sqlite3" | "db" | "db3" => Some(FileKind::Sqlite),
        _ => None,
    }
}

/// A table imported from a CSV file, written back there after edits.
#[derive(Clone, Debug, PartialEq)]
pub struct CsvTable {
    pub table: String,
    pub path: PathBuf,
    pub delimiter: u8,
}

/// An opened file and the SQLite database that represents it.
#[derive(Clone, Debug)]
pub struct OpenedFile {
    pub origin: PathBuf,
    pub kind: FileKind,
    pub db_path: PathBuf,
    pub tables: Vec<CsvTable>,
    pub source: DataSource,
}

impl OpenedFile {
    pub fn csv_table(&self, table: &str) -> Option<&CsvTable> {
        self.tables.iter().find(|t| t.table == table)
    }

    /// Removes the temporary database of imported files; SQLite files are left alone.
    pub fn close(&self) {
        if self.kind != FileKind::Sqlite {
            let _ = std::fs::remove_file(&self.db_path);
        }
    }
}

/// Opens a supported file. CSV and Data Package resources are imported into a
/// new temporary SQLite database under `temp_dir`.
pub fn open(path: &Path, temp_dir: &Path) -> Result<OpenedFile> {
    let kind = kind_of(path).with_context(|| format!("unsupported file: {}", path.display()))?;
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let (db_path, tables) = match kind {
        FileKind::Sqlite => (path.to_path_buf(), Vec::new()),
        FileKind::Csv | FileKind::DataPackage => {
            std::fs::create_dir_all(temp_dir)?;
            let db_path = temp_dir.join(format!("{}.sqlite", uuid::Uuid::new_v4()));
            let conn = rusqlite::Connection::open(&db_path)?;
            let tables = if kind == FileKind::Csv {
                vec![CsvTable {
                    table: table_name(path),
                    path: path.to_path_buf(),
                    delimiter: detect_delimiter(path)?,
                }]
            } else {
                data_package_tables(path)?
            };
            for t in &tables {
                import_csv(&conn, &t.table, &t.path, t.delimiter)
                    .with_context(|| format!("cannot import {}", t.path.display()))?;
            }
            (db_path, tables)
        }
    };
    let mut source = DataSource::new(DriverKind::Sqlite);
    source.id = format!("file:{}", uuid::Uuid::new_v4());
    source.name = name;
    source.file = db_path.to_string_lossy().into_owned();
    Ok(OpenedFile {
        origin: path.to_path_buf(),
        kind,
        db_path,
        tables,
        source,
    })
}

fn table_name(path: &Path) -> String {
    path.file_stem()
        .map_or_else(|| "data".into(), |s| s.to_string_lossy().into_owned())
}

/// Longest header prefix read to detect the separator.
const MAX_HEADER_SAMPLE: u64 = 1 << 20;

/// Picks the separator that splits the header line into the most fields.
pub fn detect_delimiter(path: &Path) -> Result<u8> {
    if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("tsv"))
    {
        return Ok(b'\t');
    }
    // Only the header line is analysed; large files are not read whole.
    let mut first_line = Vec::new();
    std::io::BufReader::new(std::io::Read::take(
        std::fs::File::open(path)?,
        MAX_HEADER_SAMPLE,
    ))
    .read_until(b'\n', &mut first_line)?;
    let count = |d: u8| {
        let mut in_quotes = false;
        first_line
            .iter()
            .filter(|&&b| {
                if b == b'"' {
                    in_quotes = !in_quotes;
                }
                !in_quotes && b == d
            })
            .count()
    };
    let best = (*b",;\t|").into_iter().max_by_key(|&d| count(d)).unwrap();
    Ok(if count(best) == 0 { b',' } else { best })
}

/// CSV resources of a Frictionless Data Package, with paths relative to the descriptor.
fn data_package_tables(descriptor: &Path) -> Result<Vec<CsvTable>> {
    let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(descriptor)?)?;
    let base = descriptor.parent().unwrap_or(Path::new("."));
    let resources = json["resources"]
        .as_array()
        .context("datapackage.json has no resources")?;
    let mut tables = Vec::new();
    for (i, r) in resources.iter().enumerate() {
        let rel = match &r["path"] {
            serde_json::Value::String(p) => p.clone(),
            serde_json::Value::Array(parts) => parts
                .first()
                .and_then(|p| p.as_str())
                .unwrap_or_default()
                .to_string(),
            _ => continue,
        };
        if rel.is_empty() || rel.contains("://") {
            continue;
        }
        let path = base.join(&rel);
        if kind_of(&path) != Some(FileKind::Csv) {
            continue;
        }
        let delimiter = match r["dialect"]["delimiter"]
            .as_str()
            .and_then(|d| d.bytes().next())
        {
            Some(d) => d,
            None => detect_delimiter(&path)?,
        };
        let table = r["name"]
            .as_str()
            .map_or_else(|| table_name(&path), str::to_string);
        let table = if tables.iter().any(|t: &CsvTable| t.table == table) {
            format!("{table}_{i}")
        } else {
            table
        };
        tables.push(CsvTable {
            table,
            path,
            delimiter,
        });
    }
    if tables.is_empty() {
        bail!("datapackage.json lists no local CSV resources");
    }
    Ok(tables)
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum ColumnType {
    Integer,
    Real,
    Text,
}

impl ColumnType {
    fn sql(self) -> &'static str {
        match self {
            ColumnType::Integer => "INTEGER",
            ColumnType::Real => "REAL",
            ColumnType::Text => "TEXT",
        }
    }
}

/// The most specific type every non-empty value round-trips through unchanged.
#[cfg(test)]
fn infer_type<'a>(values: impl Iterator<Item = &'a str>) -> ColumnType {
    values.fold(ColumnType::Integer, refine_type)
}

/// Narrows `ty` so that `value` also round-trips through it unchanged.
fn refine_type(ty: ColumnType, value: &str) -> ColumnType {
    if value.is_empty() || ty == ColumnType::Text {
        return ty;
    }
    if ty == ColumnType::Integer && value.parse::<i64>().is_ok_and(|i| i.to_string() == value) {
        return ty;
    }
    if value
        .parse::<f64>()
        .is_ok_and(|f| f.is_finite() && format_real(f) == value)
    {
        return ColumnType::Real;
    }
    ColumnType::Text
}

fn format_real(f: f64) -> String {
    f.to_string()
}

/// Imports a CSV file as a table; returns the number of rows.
pub fn import_csv(
    conn: &rusqlite::Connection,
    table: &str,
    path: &Path,
    delimiter: u8,
) -> Result<usize> {
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .flexible(true)
        .from_path(path)?;
    let mut columns: Vec<String> = Vec::new();
    for (i, h) in reader.headers()?.iter().enumerate() {
        let base = if h.trim().is_empty() {
            format!("column_{}", i + 1)
        } else {
            h.trim().to_string()
        };
        let mut name = base.clone();
        let mut n = 2;
        while columns.contains(&name) {
            name = format!("{base}_{n}");
            n += 1;
        }
        columns.push(name);
    }
    // First pass: width and types over every record, without keeping the rows,
    // so a late value can still widen a column type exactly as before.
    let mut record = csv::StringRecord::new();
    let mut types = vec![ColumnType::Integer; columns.len()];
    while reader.read_record(&mut record)? {
        if record.len() > types.len() {
            // Rows longer than the header get extra columns instead of losing data.
            types.resize(record.len(), ColumnType::Integer);
        }
        for (ty, value) in types.iter_mut().zip(record.iter()) {
            *ty = refine_type(*ty, value);
        }
    }
    let width = types.len();
    while columns.len() < width {
        columns.push(format!("column_{}", columns.len() + 1));
    }

    let q = |s: &str| crate::drivers::quote_ident(s, '"');
    let defs = columns
        .iter()
        .zip(&types)
        .map(|(c, t)| format!("{} {}", q(c), t.sql()))
        .collect::<Vec<_>>()
        .join(", ");
    conn.execute_batch(&format!(
        "DROP TABLE IF EXISTS {0}; CREATE TABLE {0} ({defs});",
        q(table)
    ))?;
    let placeholders = vec!["?"; width].join(", ");
    let tx = conn.unchecked_transaction()?;
    let mut count = 0;
    {
        let mut insert =
            tx.prepare(&format!("INSERT INTO {} VALUES ({placeholders})", q(table)))?;
        // Second pass streams the records into SQLite; text is bound from the
        // reused record buffer instead of being copied.
        let mut reader = csv::ReaderBuilder::new()
            .delimiter(delimiter)
            .flexible(true)
            .from_path(path)?;
        let changed = || anyhow::anyhow!("the file changed while it was being opened");
        while reader.read_record(&mut record)? {
            if record.len() > width {
                return Err(changed());
            }
            let mut values = Vec::with_capacity(width);
            for (c, ty) in types.iter().enumerate() {
                let v = record.get(c).unwrap_or("");
                values.push(match (v.is_empty(), ty) {
                    (true, _) => ValueRef::Null,
                    (false, ColumnType::Integer) => {
                        ValueRef::Integer(v.parse().map_err(|_| changed())?)
                    }
                    (false, ColumnType::Real) => ValueRef::Real(v.parse().map_err(|_| changed())?),
                    (false, ColumnType::Text) => ValueRef::Text(v.as_bytes()),
                });
            }
            insert.execute(rusqlite::params_from_iter(
                values
                    .into_iter()
                    .map(rusqlite::types::ToSqlOutput::Borrowed),
            ))?;
            count += 1;
        }
    }
    tx.commit()?;
    Ok(count)
}

/// Writes a table back to CSV in rowid order (the order rows were read in, new
/// rows last). The file is replaced atomically. Returns the number of rows.
pub fn export_csv(
    conn: &rusqlite::Connection,
    table: &str,
    path: &Path,
    delimiter: u8,
) -> Result<usize> {
    let mut stmt = conn.prepare(&format!(
        "SELECT * FROM {} ORDER BY rowid",
        crate::drivers::quote_ident(table, '"')
    ))?;
    let names: Vec<String> = stmt
        .column_names()
        .into_iter()
        .map(str::to_string)
        .collect();
    let tmp = path.with_extension("dbeast-saving");
    let mut writer = csv::WriterBuilder::new()
        .delimiter(delimiter)
        .from_path(&tmp)?;
    writer.write_record(&names)?;
    let mut rows = stmt.query([])?;
    let mut count = 0;
    while let Some(row) = rows.next()? {
        let fields: Vec<String> = (0..names.len())
            .map(|i| match row.get_ref_unwrap(i) {
                ValueRef::Null => String::new(),
                ValueRef::Integer(v) => v.to_string(),
                ValueRef::Real(v) => format_real(v),
                ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
                ValueRef::Blob(b) => b.iter().map(|x| format!("{x:02X}")).collect(),
            })
            .collect();
        writer.write_record(&fields)?;
        count += 1;
    }
    writer.flush()?;
    drop(writer);
    std::fs::rename(&tmp, path)?;
    Ok(count)
}

/// Writes every imported table of an opened file back to its CSV file.
pub fn save(opened: &OpenedFile, only_table: Option<&str>) -> Result<usize> {
    let conn = rusqlite::Connection::open(&opened.db_path)?;
    let mut rows = 0;
    for t in opened
        .tables
        .iter()
        .filter(|t| only_table.is_none_or(|name| t.table == name))
    {
        rows += export_csv(&conn, &t.table, &t.path, t.delimiter)?;
    }
    Ok(rows)
}

/// Directories that are never worth descending into when browsing or searching.
fn skipped_dir(name: &str) -> bool {
    name.starts_with('.') || matches!(name, "node_modules" | "target" | "__pycache__" | "Library")
}

/// Subdirectories and supported files of a directory, both sorted by name.
pub fn list_dir(dir: &Path) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)?.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        match entry.file_type() {
            Ok(t) if t.is_dir() && !skipped_dir(&name) => dirs.push(path),
            Ok(t) if t.is_file() && kind_of(&path).is_some() => files.push(path),
            _ => {}
        }
    }
    let by_name = |p: &PathBuf| p.file_name().map(|n| n.to_string_lossy().to_lowercase());
    dirs.sort_by_key(by_name);
    files.sort_by_key(by_name);
    Ok((dirs, files))
}

/// Supported files under `root` whose name contains `query` (case-insensitive),
/// breadth-first so shallow matches come first. Stops after `limit` results, a
/// bounded number of visited entries, or as soon as `cancel` is set, checked
/// before each directory.
pub fn search(
    root: &Path,
    query: &str,
    limit: usize,
    cancel: &std::sync::atomic::AtomicBool,
) -> Vec<PathBuf> {
    let query = query.to_lowercase();
    let mut found = Vec::new();
    let mut queue = std::collections::VecDeque::from([root.to_path_buf()]);
    let mut visited = 0usize;
    while let Some(dir) = queue.pop_front() {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        let Ok((dirs, files)) = list_dir(&dir) else {
            continue;
        };
        visited += dirs.len() + files.len();
        for f in files {
            let name = f
                .file_name()
                .map(|n| n.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            if name.contains(&query) {
                found.push(f);
                if found.len() >= limit {
                    return found;
                }
            }
        }
        if visited > 200_000 {
            break;
        }
        queue.extend(dirs);
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dbeast_files_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn recognizes_supported_files() {
        assert_eq!(kind_of(Path::new("a/B.CSV")), Some(FileKind::Csv));
        assert_eq!(kind_of(Path::new("x.tsv")), Some(FileKind::Csv));
        assert_eq!(
            kind_of(Path::new("pkg/datapackage.json")),
            Some(FileKind::DataPackage)
        );
        assert_eq!(kind_of(Path::new("app.db")), Some(FileKind::Sqlite));
        assert_eq!(kind_of(Path::new("other.json")), None);
        assert_eq!(kind_of(Path::new("notes.txt")), None);
    }

    #[test]
    fn types_only_when_lossless() {
        assert_eq!(
            infer_type(["1", "-20", ""].into_iter()),
            ColumnType::Integer
        );
        assert_eq!(infer_type(["1", "2.5"].into_iter()), ColumnType::Real);
        assert_eq!(infer_type(["007"].into_iter()), ColumnType::Text);
        assert_eq!(infer_type(["1.50"].into_iter()), ColumnType::Text);
        assert_eq!(infer_type(["1e3"].into_iter()), ColumnType::Text);
    }

    #[test]
    fn delimiter_detection() {
        let dir = temp();
        let semi = dir.join("a.csv");
        std::fs::write(&semi, "a;b;\"c,d\"\n1;2;3\n").unwrap();
        assert_eq!(detect_delimiter(&semi).unwrap(), b';');
        let tsv = dir.join("a.tsv");
        std::fs::write(&tsv, "a,b\n").unwrap();
        assert_eq!(detect_delimiter(&tsv).unwrap(), b'\t');
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn csv_round_trip_keeps_untouched_text() {
        let dir = temp();
        let csv = dir.join("people.csv");
        let original = "id;name;code;score;note\n1;Ana;007;1.50;\"a;b\"\n2;Zoë;010;2;\n";
        std::fs::write(&csv, original).unwrap();
        let opened = open(&csv, &dir.join("tmp")).unwrap();
        assert_eq!(
            opened.tables[0],
            CsvTable {
                table: "people".into(),
                path: csv.clone(),
                delimiter: b';'
            }
        );

        let conn = rusqlite::Connection::open(&opened.db_path).unwrap();
        let ty: String = conn
            .query_row(
                "select typeof(id) || typeof(code) || typeof(score) from people",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ty, "integertexttext");
        assert_eq!(save(&opened, None).unwrap(), 2);
        assert_eq!(std::fs::read_to_string(&csv).unwrap(), original);

        conn.execute("update people set name = 'Ann' where id = 1", [])
            .unwrap();
        conn.execute("insert into people (id, name) values (3, 'New')", [])
            .unwrap();
        save(&opened, Some("people")).unwrap();
        assert_eq!(
            std::fs::read_to_string(&csv).unwrap(),
            "id;name;code;score;note\n1;Ann;007;1.50;\"a;b\"\n2;Zoë;010;2;\n3;New;;;\n"
        );
        // Windows cannot delete a database file that is still open.
        drop(conn);
        opened.close();
        assert!(!opened.db_path.exists());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn streamed_import_types_from_every_record() {
        let dir = temp();
        let csv = dir.join("late.csv");
        // A long integer run is followed by values that only fit TEXT or REAL,
        // a row wider than the header and a quoted multiline field.
        let mut original = String::from("n,big,amount\n");
        for i in 0..5000 {
            original.push_str(&format!("{i},{},{i}\n", i64::MAX - i));
        }
        original.push_str("0042,99999999999999999999,2.5\n");
        original.push_str(",,,extra,\"line one\nline, two ł\"\n");
        std::fs::write(&csv, &original).unwrap();
        let opened = open(&csv, &dir.join("tmp")).unwrap();
        let conn = rusqlite::Connection::open(&opened.db_path).unwrap();
        let declared: Vec<String> = conn
            .prepare("select type from pragma_table_info('late')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(declared, ["TEXT", "TEXT", "REAL", "TEXT", "TEXT"]);
        let count: i64 = conn
            .query_row("select count(*) from late", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 5002);
        let last: String = conn
            .query_row("select column_5 from late where rowid = 5002", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(last, "line one\nline, two ł");
        assert_eq!(save(&opened, None).unwrap(), 5002);
        let saved = std::fs::read_to_string(&csv).unwrap();
        assert!(saved.starts_with("n,big,amount,column_4,column_5\n0,9223372036854775807,0,,\n"));
        assert!(saved.contains("\n0042,99999999999999999999,2.5,,\n"));
        assert!(saved.ends_with(",,,extra,\"line one\nline, two ł\"\n"));
        drop(conn);
        opened.close();
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn data_package_imports_each_resource() {
        let dir = temp();
        std::fs::create_dir_all(dir.join("data")).unwrap();
        std::fs::write(dir.join("data/cities.csv"), "city|pop\nKraków|800000\n").unwrap();
        std::fs::write(
            dir.join("data/rivers.csv"),
            "river,km\nWisła,1047\nOdra,840\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("datapackage.json"),
            r#"{"name": "pl", "resources": [
                {"name": "cities", "path": "data/cities.csv", "dialect": {"delimiter": "|"}},
                {"name": "rivers", "path": "data/rivers.csv"},
                {"name": "remote", "path": "https://example.com/x.csv"}
            ]}"#,
        )
        .unwrap();
        let opened = open(&dir.join("datapackage.json"), &dir.join("tmp")).unwrap();
        assert_eq!(
            opened
                .tables
                .iter()
                .map(|t| t.table.as_str())
                .collect::<Vec<_>>(),
            ["cities", "rivers"]
        );
        let conn = rusqlite::Connection::open(&opened.db_path).unwrap();
        let km: i64 = conn
            .query_row("select sum(km) from rivers", [], |r| r.get(0))
            .unwrap();
        assert_eq!(km, 1887);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn listing_and_search_show_only_supported_files() {
        let dir = temp();
        std::fs::create_dir_all(dir.join("sub/deeper")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        for f in [
            "a.csv",
            "readme.md",
            "sub/b.tsv",
            "sub/deeper/sales.db",
            ".git/c.csv",
            "sub/sales.csv",
        ] {
            std::fs::write(dir.join(f), "x\n").unwrap();
        }
        let (dirs, files) = list_dir(&dir).unwrap();
        assert_eq!(dirs, [dir.join("sub")]);
        assert_eq!(files, [dir.join("a.csv")]);
        assert_eq!(
            search(&dir, "SALES", 10, &Default::default()),
            [dir.join("sub/sales.csv"), dir.join("sub/deeper/sales.db")]
        );
        assert_eq!(search(&dir, "", 2, &Default::default()).len(), 2);
        let cancelled = std::sync::atomic::AtomicBool::new(true);
        assert!(search(&dir, "", 10, &cancelled).is_empty(), "a cancelled scan stops at once");
        std::fs::remove_dir_all(dir).ok();
    }
}
