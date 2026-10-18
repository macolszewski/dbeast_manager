//! Import Data from File: CSV/TSV settings, a preview with inferred column
//! types, the DDL of a new target table and a streaming record reader that
//! applies the column mapping. The database side is in `backend::transfer`.

use crate::model::DriverKind;
use crate::sqlgen;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::Path;

/// Bytes read from the start of a file to detect its separator and preview it.
const SNIFF_BYTES: usize = 64 * 1024;
/// Records whose values decide the inferred column types.
pub const SAMPLE_ROWS: usize = 1000;
/// Bytes of the file the preview reads at most, so one huge field or many
/// wide records cannot make the preview read the whole file into memory.
const SAMPLE_BYTES: u64 = 8 << 20;
/// Characters of a field shown in the preview grid; the import reads it whole.
pub const PREVIEW_FIELD_CHARS: usize = 256;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CsvOptions {
    pub delimiter: u8,
    pub quote: u8,
    /// The first record holds the column names.
    pub header: bool,
    /// Lines before the first record (titles, comments).
    pub skip_lines: usize,
    /// A value equal to this text becomes NULL; empty by default.
    pub null_text: String,
    pub trim: bool,
}

impl Default for CsvOptions {
    fn default() -> Self {
        Self {
            delimiter: b',',
            quote: b'"',
            header: true,
            skip_lines: 0,
            null_text: String::new(),
            trim: false,
        }
    }
}

impl CsvOptions {
    fn reader<R: Read>(&self, input: R) -> csv::Reader<R> {
        csv::ReaderBuilder::new()
            .delimiter(self.delimiter)
            .quote(self.quote)
            .has_headers(false)
            .flexible(true)
            .trim(if self.trim { csv::Trim::All } else { csv::Trim::None })
            .from_reader(input)
    }

    /// The value as it goes to the database: NULL text maps to `None`.
    pub fn value(&self, field: &str) -> Option<String> {
        (field != self.null_text).then(|| field.to_string())
    }
}

/// Picks the separator that splits the first line into the most fields.
pub fn detect_delimiter(sample: &[u8], path: &Path) -> u8 {
    if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("tsv")) {
        return b'\t';
    }
    let first_line = sample.split(|&b| b == b'\n').next().unwrap_or_default();
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
    let best = (*b",;\t|").into_iter().max_by_key(|&d| count(d)).unwrap_or(b',');
    if count(best) == 0 { b',' } else { best }
}

/// Column types the import can infer, mapped to each dialect's type name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InferredType {
    Integer,
    BigInt,
    Decimal,
    Double,
    Boolean,
    Date,
    Timestamp,
    Text,
}

impl InferredType {
    pub fn sql(self, driver: DriverKind) -> &'static str {
        use InferredType::*;
        match (driver, self) {
            (DriverKind::Sqlite, Integer | BigInt | Boolean) => "INTEGER",
            (DriverKind::Sqlite, Decimal | Double) => "REAL",
            (DriverKind::Sqlite, Date | Timestamp | Text) => "TEXT",
            (_, Integer) => "INTEGER",
            (_, BigInt) => "BIGINT",
            (_, Decimal) => "NUMERIC",
            (DriverKind::MySql, Double) => "DOUBLE",
            (_, Double) => "DOUBLE PRECISION",
            (_, Boolean) => "BOOLEAN",
            (_, Date) => "DATE",
            (DriverKind::MySql, Timestamp) => "DATETIME",
            (_, Timestamp) => "TIMESTAMP",
            (_, Text) => "TEXT",
        }
    }
}

/// The most specific type that holds every non-NULL sample value.
pub fn infer_type<'a>(values: impl Iterator<Item = &'a str>) -> InferredType {
    use InferredType::*;
    // Candidates narrow as values rule them out; empty columns stay text.
    let mut int = true;
    let mut big = true;
    let mut decimal = true;
    let mut double = true;
    let mut boolean = true;
    let mut date = true;
    let mut timestamp = true;
    let mut seen = false;
    for v in values {
        seen = true;
        // Leading zeros are identifiers (zip codes, phone numbers), not numbers.
        let leading_zero = v.len() > 1 && v.starts_with('0') && !v.starts_with("0.");
        let as_int = v.parse::<i64>().ok().filter(|_| !leading_zero);
        int &= as_int.is_some_and(|i| i32::try_from(i).is_ok());
        big &= as_int.is_some();
        decimal &= !leading_zero && is_decimal(v);
        double &= !leading_zero && v.parse::<f64>().is_ok_and(f64::is_finite);
        boolean &= matches!(v.to_ascii_lowercase().as_str(), "true" | "false");
        date &= chrono::NaiveDate::parse_from_str(v, "%Y-%m-%d").is_ok();
        timestamp &= ["%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"]
            .iter()
            .any(|format| chrono::NaiveDateTime::parse_from_str(v, format).is_ok());
    }
    if !seen {
        return Text;
    }
    if int {
        Integer
    } else if big {
        BigInt
    } else if decimal {
        Decimal
    } else if double {
        Double
    } else if boolean {
        Boolean
    } else if date {
        Date
    } else if timestamp {
        Timestamp
    } else {
        Text
    }
}

/// Plain decimal notation: optional sign, digits, one optional point.
fn is_decimal(v: &str) -> bool {
    let digits = v.strip_prefix(['-', '+']).unwrap_or(v);
    let mut parts = digits.splitn(2, '.');
    let whole = parts.next().unwrap_or("");
    let fraction = parts.next();
    !whole.is_empty()
        && whole.bytes().all(|b| b.is_ascii_digit())
        && fraction.is_none_or(|f| !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()))
}

/// First records of a file with column names and inferred types.
#[derive(Clone, Debug, Default)]
pub struct CsvPreview {
    pub columns: Vec<String>,
    pub types: Vec<InferredType>,
    pub rows: Vec<Vec<String>>,
    /// The separator found in the file when the dialog asked for detection.
    pub detected_delimiter: Option<u8>,
    pub file_size: u64,
    /// The sample stopped at [`SAMPLE_BYTES`]: types come from fewer records.
    pub sample_cut: bool,
    /// Some shown fields are cut to [`PREVIEW_FIELD_CHARS`].
    pub shortened: bool,
}

/// Reads at most `left` bytes and notes whether the input went on.
struct Limited<R> {
    inner: R,
    left: u64,
    cut: std::rc::Rc<std::cell::Cell<bool>>,
}

impl<R: Read> Read for Limited<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.left == 0 {
            if !self.cut.get() && !buf.is_empty() {
                let mut probe = [0u8; 1];
                self.cut.set(self.inner.read(&mut probe)? > 0);
            }
            return Ok(0);
        }
        let len = buf.len().min(usize::try_from(self.left).unwrap_or(usize::MAX));
        let n = self.inner.read(&mut buf[..len])?;
        self.left -= n as u64;
        Ok(n)
    }
}

/// Reads the start of a file for the Import dialog. With `detect`, the
/// separator is chosen from the first line before parsing.
pub fn preview(path: &Path, options: &CsvOptions, detect: bool, rows: usize) -> Result<CsvPreview> {
    let file_size = std::fs::metadata(path).with_context(|| path.display().to_string())?.len();
    let mut options = options.clone();
    let mut detected = None;
    if detect {
        let mut sample = vec![0; SNIFF_BYTES];
        let mut file = std::fs::File::open(path)?;
        let n = file.read(&mut sample)?;
        sample.truncate(n);
        let delimiter = detect_delimiter(skip_lines_of(&sample, options.skip_lines), path);
        options.delimiter = delimiter;
        detected = Some(delimiter);
    }
    let cut = std::rc::Rc::new(std::cell::Cell::new(false));
    let limited = Limited {
        inner: std::fs::File::open(path).with_context(|| path.display().to_string())?,
        left: SAMPLE_BYTES,
        cut: cut.clone(),
    };
    let mut reader = Records::from_reader(std::io::BufReader::new(limited), &options)?;
    let columns = reader.columns.clone();
    let mut sample: Vec<Vec<String>> = Vec::new();
    while sample.len() < SAMPLE_ROWS.max(rows) {
        match reader.next_fields()? {
            Some(fields) => sample.push(fields),
            None => break,
        }
    }
    let sample_cut = cut.get();
    if sample_cut && sample.len() < SAMPLE_ROWS.max(rows) {
        // The read stopped at the limit, inside the last record at the latest.
        sample.pop();
    }
    let width = sample.iter().map(Vec::len).max().unwrap_or(0).max(columns.len());
    let mut columns = columns;
    while columns.len() < width {
        columns.push(format!("column_{}", columns.len() + 1));
    }
    let types = (0..width)
        .map(|c| {
            infer_type(
                sample
                    .iter()
                    .filter_map(|row| row.get(c))
                    .filter(|v| **v != options.null_text)
                    .map(String::as_str),
            )
        })
        .collect();
    sample.truncate(rows);
    let mut shortened = false;
    for field in sample.iter_mut().flatten() {
        if let Some((end, _)) = field.char_indices().nth(PREVIEW_FIELD_CHARS) {
            let mut short = String::with_capacity(end + 3);
            short.push_str(&field[..end]);
            short.push('…');
            *field = short;
            shortened = true;
        }
    }
    Ok(CsvPreview {
        columns,
        types,
        rows: sample,
        detected_delimiter: detected,
        file_size,
        sample_cut,
        shortened,
    })
}

fn skip_lines_of(sample: &[u8], lines: usize) -> &[u8] {
    let mut rest = sample;
    for _ in 0..lines {
        match rest.iter().position(|&b| b == b'\n') {
            Some(i) => rest = &rest[i + 1..],
            None => return &[],
        }
    }
    rest
}

/// Unique, non-empty column names from a header record.
pub fn column_names(header: &[String]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for (i, h) in header.iter().enumerate() {
        let base = if h.trim().is_empty() { format!("column_{}", i + 1) } else { h.trim().to_string() };
        let mut name = base.clone();
        let mut n = 2;
        while names.contains(&name) {
            name = format!("{base}_{n}");
            n += 1;
        }
        names.push(name);
    }
    names
}

/// Streaming reader of a CSV file: skips leading lines and the header and
/// reports the byte position for progress.
pub struct Records<R = std::io::BufReader<std::fs::File>> {
    reader: csv::Reader<R>,
    record: csv::StringRecord,
    pub columns: Vec<String>,
    /// Line in the file of the record returned last, for error messages.
    pub line: u64,
}

impl Records {
    pub fn open(path: &Path, options: &CsvOptions) -> Result<Self> {
        Self::from_reader(
            std::io::BufReader::with_capacity(1 << 20, std::fs::File::open(path)?),
            options,
        )
    }
}

impl<R: std::io::BufRead> Records<R> {
    fn from_reader(mut file: R, options: &CsvOptions) -> Result<Self> {
        let mut skipped = 0u64;
        for _ in 0..options.skip_lines {
            let mut line = Vec::new();
            if std::io::BufRead::read_until(&mut file, b'\n', &mut line)? == 0 {
                break;
            }
            skipped += 1;
        }
        let mut reader = options.reader(file);
        let mut record = csv::StringRecord::new();
        let mut columns = Vec::new();
        let mut line = skipped;
        if options.header && reader.read_record(&mut record)? {
            columns = column_names(&record.iter().map(str::to_string).collect::<Vec<_>>());
            line = skipped + record.position().map_or(1, |p| p.line());
        }
        Ok(Self {
            reader,
            record,
            columns,
            line,
        })
    }

    /// The next record's fields, or `None` at the end of the file.
    pub fn next_fields(&mut self) -> Result<Option<Vec<String>>> {
        if !self.reader.read_record(&mut self.record)? {
            return Ok(None);
        }
        self.line += 1;
        Ok(Some(self.record.iter().map(str::to_string).collect()))
    }

    /// Bytes consumed so far, for progress against the file size.
    pub fn position(&self) -> u64 {
        self.reader.position().byte()
    }
}

/// `CREATE TABLE` for a new import target.
pub fn create_table_sql(driver: DriverKind, schema: Option<&str>, table: &str, columns: &[(String, String)]) -> String {
    let target = qualified(driver, schema, table);
    let defs = columns
        .iter()
        .map(|(name, ty)| format!("    {} {ty}", sqlgen::ident(driver, name)))
        .collect::<Vec<_>>()
        .join(",\n");
    format!("CREATE TABLE {target} (\n{defs}\n)")
}

pub fn qualified(driver: DriverKind, schema: Option<&str>, table: &str) -> String {
    match schema {
        Some(schema) => format!("{}.{}", sqlgen::ident(driver, schema), sqlgen::ident(driver, table)),
        None => sqlgen::ident(driver, table),
    }
}

/// Table name derived from a file name: letters, digits and underscores.
pub fn table_name_for(path: &Path) -> String {
    let stem = path.file_stem().map_or_else(|| "imported".into(), |s| s.to_string_lossy().into_owned());
    let name: String = stem
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
    if name.is_empty() { "imported".into() } else { name }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str, text: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("dbeast_import_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn types_are_inferred_conservatively() {
        use InferredType::*;
        assert_eq!(infer_type(["1", "-20", "300"].into_iter()), Integer);
        assert_eq!(infer_type(["1", "9000000000"].into_iter()), BigInt);
        assert_eq!(infer_type(["1.50", "2"].into_iter()), Decimal);
        assert_eq!(infer_type(["1e3", "2.5"].into_iter()), Double);
        assert_eq!(infer_type(["007", "12"].into_iter()), Text, "leading zeros keep identifiers");
        assert_eq!(infer_type(["true", "FALSE"].into_iter()), Boolean);
        assert_eq!(infer_type(["2026-10-17", "2024-02-29"].into_iter()), Date);
        assert_eq!(infer_type(["2026-10-17 10:00:00", "2026-10-17T11:30:05.25"].into_iter()), Timestamp);
        assert_eq!(infer_type(["a", "1"].into_iter()), Text);
        assert_eq!(infer_type(std::iter::empty()), Text);
        assert_eq!(Decimal.sql(DriverKind::Sqlite), "REAL");
        assert_eq!(Timestamp.sql(DriverKind::MySql), "DATETIME");
        assert_eq!(Double.sql(DriverKind::Postgres), "DOUBLE PRECISION");
    }

    #[test]
    fn preview_detects_separator_skips_lines_and_names_columns() {
        let path = temp_file("people.csv", "Exported 2026-10-17\nid;name;;name\n1;Ada;x;A\n2;Olek;;B;extra\n");
        let options = CsvOptions { skip_lines: 1, ..CsvOptions::default() };
        let preview = preview(&path, &options, true, 10).unwrap();
        assert_eq!(preview.detected_delimiter, Some(b';'));
        assert_eq!(preview.columns, ["id", "name", "column_3", "name_2", "column_5"]);
        assert_eq!(preview.rows.len(), 2);
        assert_eq!(preview.types[0], InferredType::Integer);
        assert_eq!(preview.types[2], InferredType::Text);
    }

    #[test]
    fn records_report_lines_and_positions() {
        let path = temp_file("a.csv", "id,note\n1,\"two\nlines\"\n2,b\n");
        let mut records = Records::open(&path, &CsvOptions::default()).unwrap();
        assert_eq!(records.next_fields().unwrap().unwrap(), ["1", "two\nlines"]);
        assert_eq!(records.next_fields().unwrap().unwrap(), ["2", "b"]);
        assert!(records.next_fields().unwrap().is_none());
        assert_eq!(records.position(), std::fs::metadata(&path).unwrap().len());
    }

    #[test]
    fn ddl_uses_the_dialect() {
        let columns = vec![("id".to_string(), "INTEGER".to_string()), ("full name".to_string(), "TEXT".to_string())];
        assert_eq!(
            create_table_sql(DriverKind::MySql, Some("shop"), "people", &columns),
            "CREATE TABLE `shop`.`people` (\n    `id` INTEGER,\n    `full name` TEXT\n)"
        );
        assert_eq!(table_name_for(Path::new("/x/sales 2026.csv")), "sales_2026");
    }

    #[test]
    fn null_text_maps_to_none() {
        let options = CsvOptions { null_text: "\\N".into(), ..CsvOptions::default() };
        assert_eq!(options.value("\\N"), None);
        assert_eq!(options.value(""), Some(String::new()));
        assert_eq!(CsvOptions::default().value(""), None);
    }

    #[test]
    fn preview_reads_a_bounded_sample_and_shortens_shown_fields() {
        // One field far larger than the sample: the preview reads 8 MiB of it
        // and drops the record it could not finish.
        let huge = "x".repeat((SAMPLE_BYTES as usize) * 2);
        let text = format!("id,doc\n1,short\n2,\"{huge}\"\n3,after\n");
        let path = temp_file("huge_field.csv", &text);
        let options = CsvOptions::default();
        let first = preview(&path, &options, false, 50).unwrap();
        assert!(first.sample_cut);
        assert_eq!(first.rows, [["1", "short"]]);
        assert_eq!(first.types[0], InferredType::Integer);

        // Long fields within the sample are shown shortened, Unicode intact.
        let long = "żółć 😀 ".repeat(100);
        let text = format!("id,note\n1,\"{long}\"\n2,\"a\nb\"\n");
        let path = temp_file("long_field.csv", &text);
        let second = preview(&path, &options, false, 50).unwrap();
        assert!(!second.sample_cut);
        assert!(second.shortened);
        let shown = &second.rows[0][1];
        assert_eq!(shown.chars().count(), PREVIEW_FIELD_CHARS + 1);
        assert!(shown.ends_with('…') && long.starts_with(shown.trim_end_matches('…')));
        assert_eq!(second.rows[1][1], "a\nb");

        // The import itself still reads the whole field.
        let mut records = Records::open(&path, &options).unwrap();
        assert_eq!(records.next_fields().unwrap().unwrap()[1], long);
    }

    #[test]
    #[ignore = "release benchmark: cargo test --release --lib import_preview_cost -- --ignored --nocapture"]
    fn import_preview_cost_benchmark() {
        let huge = "y".repeat(200 << 20);
        let path = temp_file("huge.csv", &format!("id,doc\n1,a\n2,\"{huge}\"\n"));
        drop(huge);
        let start = std::time::Instant::now();
        let preview = preview(&path, &CsvOptions::default(), true, 50).unwrap();
        println!(
            "import preview of a 200 MiB field: {:?}, {} records kept, sample cut: {}",
            start.elapsed(),
            preview.rows.len(),
            preview.sample_cut
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
