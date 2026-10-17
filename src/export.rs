//! Data extractors: CSV, TSV, JSON, SQL INSERT,
//! Markdown, HTML and XML. [`Writer`] streams rows to any `io::Write`, so a
//! whole table is exported in batches without being held in memory; the
//! clipboard copies of a fetched page use the same code.

use crate::model::{Column, DriverKind, QueryResult, Value};
use crate::sqlgen;
use serde::{Deserialize, Serialize};
use std::io::{self, Write};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExportFormat {
    Csv,
    Tsv,
    Json,
    SqlInsert,
    Markdown,
    Html,
    Xml,
}

impl ExportFormat {
    pub const ALL: [ExportFormat; 7] = [
        ExportFormat::Csv,
        ExportFormat::Tsv,
        ExportFormat::Json,
        ExportFormat::SqlInsert,
        ExportFormat::Markdown,
        ExportFormat::Html,
        ExportFormat::Xml,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ExportFormat::Csv => "CSV",
            ExportFormat::Tsv => "TSV",
            ExportFormat::Json => "JSON",
            ExportFormat::SqlInsert => "SQL Inserts",
            ExportFormat::Markdown => "Markdown",
            ExportFormat::Html => "HTML Table",
            ExportFormat::Xml => "XML",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Csv => "csv",
            ExportFormat::Tsv => "tsv",
            ExportFormat::Json => "json",
            ExportFormat::SqlInsert => "sql",
            ExportFormat::Markdown => "md",
            ExportFormat::Html => "html",
            ExportFormat::Xml => "xml",
        }
    }

    /// Formats whose layout the separator, quoting and NULL text options change.
    pub fn delimited(self) -> bool {
        matches!(self, ExportFormat::Csv | ExportFormat::Tsv)
    }

    pub fn has_header_option(self) -> bool {
        matches!(
            self,
            ExportFormat::Csv | ExportFormat::Tsv | ExportFormat::Markdown | ExportFormat::Html
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Quote {
    /// Only values containing the separator, a quote or a line break.
    #[default]
    WhenNeeded,
    Always,
    Never,
}

impl Quote {
    pub const ALL: [Quote; 3] = [Quote::WhenNeeded, Quote::Always, Quote::Never];

    pub fn label(self) -> &'static str {
        match self {
            Quote::WhenNeeded => "When needed",
            Quote::Always => "Always",
            Quote::Never => "Never",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum LineEnding {
    #[default]
    Lf,
    CrLf,
}

impl LineEnding {
    fn text(self) -> &'static str {
        match self {
            LineEnding::Lf => "\n",
            LineEnding::CrLf => "\r\n",
        }
    }
}

/// Extractor settings of the Export Data dialog. Remembered between exports.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExportOptions {
    pub format: ExportFormat,
    /// First line with column names (CSV, TSV, Markdown, HTML).
    pub header: bool,
    /// CSV field separator; TSV always uses a tab.
    pub separator: String,
    pub quote: Quote,
    /// Text written for NULL in CSV and TSV.
    pub null_text: String,
    pub line_ending: LineEnding,
    /// Rows per INSERT statement; 1 writes one statement per row.
    pub rows_per_insert: usize,
    /// Schema-qualify the table name of INSERT statements.
    pub qualify_table: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            format: ExportFormat::Csv,
            header: true,
            separator: ",".into(),
            quote: Quote::WhenNeeded,
            null_text: String::new(),
            line_ending: LineEnding::Lf,
            rows_per_insert: 1,
            qualify_table: false,
        }
    }
}

impl ExportOptions {
    pub fn with_format(format: ExportFormat) -> Self {
        Self {
            format,
            ..Self::default()
        }
    }

    fn separator(&self) -> &str {
        match self.format {
            ExportFormat::Tsv => "\t",
            _ if self.separator.is_empty() => ",",
            _ => &self.separator,
        }
    }
}

/// Target of SQL INSERT statements.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TableName {
    pub schema: Option<String>,
    pub table: String,
}

impl TableName {
    pub fn new(table: impl Into<String>) -> Self {
        Self {
            schema: None,
            table: table.into(),
        }
    }
}

/// Streaming writer of one extractor. Call [`Writer::begin`] with the columns,
/// [`Writer::rows`] for each batch and [`Writer::finish`] at the end.
pub struct Writer<W: Write> {
    out: W,
    options: ExportOptions,
    driver: DriverKind,
    table: TableName,
    columns: Vec<String>,
    /// JSON: each distinct column name, encoded once, with the indexes of the
    /// columns that carry it; a repeated name takes its last value.
    json_keys: Vec<(String, Vec<usize>)>,
    /// Rows written so far, for JSON separators and multi-row INSERTs.
    rows: u64,
    /// Rows already in the open multi-row INSERT statement.
    open_insert: usize,
    insert_prefix: String,
}

impl<W: Write> Writer<W> {
    pub fn new(out: W, options: ExportOptions, driver: DriverKind, table: TableName) -> Self {
        Self {
            out,
            options,
            driver,
            table,
            columns: Vec::new(),
            json_keys: Vec::new(),
            rows: 0,
            open_insert: 0,
            insert_prefix: String::new(),
        }
    }

    pub fn rows_written(&self) -> u64 {
        self.rows
    }

    pub fn begin(&mut self, columns: &[Column]) -> io::Result<()> {
        self.columns = columns.iter().map(|c| c.name.clone()).collect();
        let nl = self.options.line_ending.text();
        match self.options.format {
            ExportFormat::Csv | ExportFormat::Tsv => {
                if self.options.header {
                    let line = self
                        .columns
                        .iter()
                        .map(|name| self.delimited_field(name))
                        .collect::<Vec<_>>()
                        .join(self.options.separator());
                    write!(self.out, "{line}{nl}")?;
                }
            }
            ExportFormat::Json => {
                self.json_keys.clear();
                for (i, name) in self.columns.iter().enumerate() {
                    match self.json_keys.iter_mut().find(|(_, indexes)| self.columns[indexes[0]] == *name) {
                        Some((_, indexes)) => indexes.push(i),
                        None => self.json_keys.push((serde_json::to_string(name).map_err(io::Error::from)?, vec![i])),
                    }
                }
                write!(self.out, "[")?
            }
            ExportFormat::SqlInsert => {
                let target = match (&self.table.schema, self.options.qualify_table) {
                    (Some(schema), true) => format!(
                        "{}.{}",
                        sqlgen::ident(self.driver, schema),
                        sqlgen::ident(self.driver, &self.table.table)
                    ),
                    _ => sqlgen::ident(self.driver, &self.table.table),
                };
                let cols = self
                    .columns
                    .iter()
                    .map(|n| sqlgen::ident(self.driver, n))
                    .collect::<Vec<_>>()
                    .join(", ");
                self.insert_prefix = format!("INSERT INTO {target} ({cols}) VALUES ");
            }
            ExportFormat::Markdown => {
                // A Markdown table needs a header row; without the option it is empty.
                let names: Vec<String> = if self.options.header {
                    self.columns.iter().map(|n| markdown_cell(n)).collect()
                } else {
                    vec![String::new(); self.columns.len()]
                };
                write!(self.out, "| {} |{nl}", names.join(" | "))?;
                write!(self.out, "|{}{nl}", "---|".repeat(self.columns.len()))?;
            }
            ExportFormat::Html => {
                write!(
                    self.out,
                    "<!DOCTYPE html>{nl}<html>{nl}<head>{nl}<meta charset=\"UTF-8\">{nl}<title>{}</title>{nl}</head>{nl}<body>{nl}<table border=\"1\" style=\"border-collapse:collapse\">{nl}",
                    html(&self.table.table)
                )?;
                if self.options.header {
                    write!(self.out, "<tr>")?;
                    for name in &self.columns {
                        write!(self.out, "<th>{}</th>", html(name))?;
                    }
                    write!(self.out, "</tr>{nl}")?;
                }
            }
            ExportFormat::Xml => {
                write!(
                    self.out,
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>{nl}<data table=\"{}\">{nl}",
                    html(&self.table.table)
                )?;
            }
        }
        Ok(())
    }

    pub fn rows(&mut self, rows: &[Vec<Value>]) -> io::Result<()> {
        for row in rows {
            self.row(row)?;
        }
        Ok(())
    }

    pub fn row(&mut self, row: &[Value]) -> io::Result<()> {
        let nl = self.options.line_ending.text();
        match self.options.format {
            ExportFormat::Csv | ExportFormat::Tsv => {
                let separator = self.options.separator();
                for (i, value) in row.iter().enumerate() {
                    if i > 0 {
                        self.out.write_all(separator.as_bytes())?;
                    }
                    let field = match value {
                        Value::Null => self.options.null_text.clone(),
                        other => self.delimited_field(&plain(other)),
                    };
                    self.out.write_all(field.as_bytes())?;
                }
                self.out.write_all(nl.as_bytes())?;
            }
            ExportFormat::Json => {
                // The layout of serde_json's pretty printer, indented one level.
                write!(self.out, "{}{nl}  {{", if self.rows == 0 { "" } else { "," })?;
                let mut fields = 0;
                for (key, indexes) in &self.json_keys {
                    let Some(&i) = indexes.iter().rev().find(|&&i| i < row.len()) else {
                        continue;
                    };
                    write!(self.out, "{}{nl}    {key}: ", if fields == 0 { "" } else { "," })?;
                    write_json(&mut self.out, &row[i])?;
                    fields += 1;
                }
                if fields > 0 {
                    write!(self.out, "{nl}  ")?;
                }
                self.out.write_all(b"}")?;
            }
            ExportFormat::SqlInsert => {
                let values = row
                    .iter()
                    .map(|v| sqlgen::literal(self.driver, v))
                    .collect::<Vec<_>>()
                    .join(", ");
                let per_statement = self.options.rows_per_insert.max(1);
                if self.open_insert == 0 {
                    write!(self.out, "{}({values})", self.insert_prefix)?;
                } else {
                    write!(self.out, ",{nl}       ({values})")?;
                }
                self.open_insert += 1;
                if self.open_insert == per_statement {
                    write!(self.out, ";{nl}")?;
                    self.open_insert = 0;
                }
            }
            ExportFormat::Markdown => {
                let cells: Vec<String> = row.iter().map(|v| markdown_cell(&plain(v))).collect();
                write!(self.out, "| {} |{nl}", cells.join(" | "))?;
            }
            ExportFormat::Html => {
                write!(self.out, "<tr>")?;
                for value in row {
                    match value {
                        Value::Null => write!(self.out, "<td></td>")?,
                        other => write!(self.out, "<td>{}</td>", html(&plain(other)))?,
                    }
                }
                write!(self.out, "</tr>{nl}")?;
            }
            ExportFormat::Xml => {
                write!(self.out, "  <row>{nl}")?;
                for (name, value) in self.columns.iter().zip(row) {
                    match value {
                        Value::Null => write!(
                            self.out,
                            "    <column name=\"{}\" null=\"true\"/>{nl}",
                            html(name)
                        )?,
                        other => write!(
                            self.out,
                            "    <column name=\"{}\">{}</column>{nl}",
                            html(name),
                            html(&plain(other))
                        )?,
                    }
                }
                write!(self.out, "  </row>{nl}")?;
            }
        }
        self.rows += 1;
        Ok(())
    }

    /// Closes open structures and flushes; returns the inner writer.
    pub fn finish(mut self) -> io::Result<W> {
        let nl = self.options.line_ending.text();
        match self.options.format {
            ExportFormat::Json => {
                if self.rows == 0 {
                    write!(self.out, "]{nl}")?;
                } else {
                    write!(self.out, "{nl}]{nl}")?;
                }
            }
            ExportFormat::SqlInsert if self.open_insert > 0 => write!(self.out, ";{nl}")?,
            ExportFormat::Html => write!(self.out, "</table>{nl}</body>{nl}</html>{nl}")?,
            ExportFormat::Xml => write!(self.out, "</data>{nl}")?,
            _ => {}
        }
        self.out.flush()?;
        Ok(self.out)
    }

    fn delimited_field(&self, s: &str) -> String {
        let separator = self.options.separator();
        let quote = match self.options.quote {
            Quote::Always => true,
            Quote::Never => false,
            // TSV readers rarely understand quotes; line breaks become spaces below.
            Quote::WhenNeeded if self.options.format == ExportFormat::Tsv => false,
            Quote::WhenNeeded => {
                s.contains(separator)
                    || s.contains(['"', '\n', '\r'])
                    || (self.options.format == ExportFormat::Csv
                        && !self.options.null_text.is_empty()
                        && s == self.options.null_text)
            }
        };
        if quote {
            format!("\"{}\"", s.replace('"', "\"\""))
        } else if self.options.format == ExportFormat::Tsv {
            // TSV has no quoting by default; keep each record on one line.
            s.replace(['\t', '\n', '\r'], " ")
        } else {
            s.to_string()
        }
    }
}

/// Renders rows with default options; `table` names the target of INSERT statements.
pub fn render(format: ExportFormat, data: &QueryResult, table: &str, driver: DriverKind) -> String {
    render_rows(
        format,
        data,
        data.rows.iter().map(Vec::as_slice),
        table,
        driver,
    )
}

pub fn render_rows<'a>(
    format: ExportFormat,
    data: &QueryResult,
    rows: impl Iterator<Item = &'a [Value]>,
    table: &str,
    driver: DriverKind,
) -> String {
    let mut writer = Writer::new(
        Vec::new(),
        ExportOptions::with_format(format),
        driver,
        TableName::new(table),
    );
    // Writing to memory cannot fail.
    writer.begin(&data.columns).expect("in-memory write");
    for row in rows {
        writer.row(row).expect("in-memory write");
    }
    let bytes = writer.finish().expect("writing to memory cannot fail");
    String::from_utf8(bytes).unwrap_or_default()
}

/// Text form used by text formats: binary data as hex.
fn plain(v: &Value) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    match v {
        Value::Null => String::new(),
        Value::Bytes(b) => {
            let mut text = String::with_capacity(2 + b.len() * 2);
            text.push_str("0x");
            for &byte in b {
                text.push(HEX[usize::from(byte >> 4)] as char);
                text.push(HEX[usize::from(byte & 0xF)] as char);
            }
            text
        }
        other => other.to_string(),
    }
}

fn markdown_cell(s: &str) -> String {
    s.replace('|', "\\|").replace(['\n', '\r'], " ")
}

fn html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            other => out.push(other),
        }
    }
    out
}

/// One value as JSON, without building a `serde_json::Value` for text.
fn write_json(out: &mut impl Write, v: &Value) -> io::Result<()> {
    match v {
        Value::Text(s) => serde_json::to_writer(out, s.as_str()),
        other => serde_json::to_writer(out, &json(other)),
    }
    .map_err(io::Error::from)
}

fn json(v: &Value) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        Value::Null => J::Null,
        Value::Bool(b) => J::Bool(*b),
        Value::Int(i) => J::from(*i),
        Value::Float(f) => serde_json::Number::from_f64(*f).map_or(J::Null, J::Number),
        // Kept as written so no precision is lost.
        Value::Decimal(d) => serde_json::from_str(d).unwrap_or_else(|_| J::String(d.clone())),
        Value::Text(s) => J::String(s.clone()),
        Value::Bytes(_) => J::String(plain(v)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Column;

    fn data() -> QueryResult {
        QueryResult {
            columns: ["id", "name", "price"]
                .iter()
                .map(|n| Column {
                    name: n.to_string(),
                    type_name: String::new(),
                })
                .collect(),
            rows: vec![
                vec![
                    Value::Int(1),
                    Value::Text("a, \"b\"".into()),
                    Value::Decimal("10.50".into()),
                ],
                vec![Value::Int(2), Value::Null, Value::Decimal("0.01".into())],
            ],
            ..Default::default()
        }
    }

    fn write(options: ExportOptions, driver: DriverKind) -> String {
        let data = data();
        let mut writer = Writer::new(Vec::new(), options, driver, TableName {
            schema: Some("shop".into()),
            table: "items".into(),
        });
        writer.begin(&data.columns).unwrap();
        // Two batches, as a streamed export delivers them.
        writer.rows(&data.rows[..1]).unwrap();
        writer.rows(&data.rows[1..]).unwrap();
        String::from_utf8(writer.finish().unwrap()).unwrap()
    }

    #[test]
    fn csv_quotes_special_fields() {
        assert_eq!(
            render(ExportFormat::Csv, &data(), "t", DriverKind::Postgres),
            "id,name,price\n1,\"a, \"\"b\"\"\",10.50\n2,,0.01\n"
        );
    }

    #[test]
    fn csv_options_change_separator_quotes_nulls_and_line_endings() {
        let options = ExportOptions {
            separator: ";".into(),
            quote: Quote::Always,
            null_text: "\\N".into(),
            line_ending: LineEnding::CrLf,
            header: false,
            ..ExportOptions::default()
        };
        assert_eq!(
            write(options, DriverKind::Postgres),
            "\"1\";\"a, \"\"b\"\"\";\"10.50\"\r\n\"2\";\\N;\"0.01\"\r\n"
        );
    }

    #[test]
    fn tsv_keeps_records_on_one_line() {
        let mut data = data();
        data.rows[0][1] = Value::Text("two\nlines\tand tab".into());
        let out = render(ExportFormat::Tsv, &data, "t", DriverKind::Postgres);
        assert_eq!(out.lines().nth(1).unwrap(), "1\ttwo lines and tab\t10.50");
    }

    #[test]
    fn json_keeps_types_across_batches() {
        let out: serde_json::Value =
            serde_json::from_str(&write(ExportOptions::with_format(ExportFormat::Json), DriverKind::Postgres))
                .unwrap();
        assert_eq!(out[0]["id"], 1);
        assert_eq!(out[0]["price"].to_string(), "10.50");
        assert!(out[1]["name"].is_null());
        let empty = render(ExportFormat::Json, &QueryResult::default(), "t", DriverKind::Postgres);
        assert_eq!(serde_json::from_str::<serde_json::Value>(&empty).unwrap(), serde_json::json!([]));
    }

    #[test]
    fn json_layout_is_exact() {
        let columns = ["id", "name", "id", "data", "ok", "ratio"]
            .iter()
            .map(|n| Column { name: n.to_string(), type_name: String::new() })
            .collect::<Vec<_>>();
        let rows = vec![
            vec![
                Value::Int(1),
                Value::Text("x\n\"y\"\u{1}ł".into()),
                Value::Int(2),
                Value::Bytes(vec![0x00, 0xAB, 0xff]),
                Value::Bool(true),
                Value::Float(f64::NAN),
            ],
            vec![Value::Int(3), Value::Null, Value::Null, Value::Bytes(vec![]), Value::Bool(false), Value::Decimal("1.50".into())],
        ];
        let options = ExportOptions { line_ending: LineEnding::CrLf, ..ExportOptions::with_format(ExportFormat::Json) };
        let mut writer = Writer::new(Vec::new(), options, DriverKind::Postgres, TableName::new("t"));
        writer.begin(&columns).unwrap();
        writer.rows(&rows).unwrap();
        let out = String::from_utf8(writer.finish().unwrap()).unwrap();
        // A repeated column name keeps its first place and its last value.
        assert_eq!(
            out,
            "[\r\n  {\r\n    \"id\": 2,\r\n    \"name\": \"x\\n\\\"y\\\"\\u0001ł\",\r\n    \"data\": \"0x00ABFF\",\r\n    \"ok\": true,\r\n    \"ratio\": null\r\n  },\r\n  {\r\n    \"id\": null,\r\n    \"name\": null,\r\n    \"data\": \"0x\",\r\n    \"ok\": false,\r\n    \"ratio\": 1.50\r\n  }\r\n]\r\n"
        );
    }

    #[test]
    fn sql_inserts_use_dialect_quoting() {
        let out = render(ExportFormat::SqlInsert, &data(), "items", DriverKind::MySql);
        assert_eq!(
            out.lines().nth(1).unwrap(),
            "INSERT INTO `items` (`id`, `name`, `price`) VALUES (2, NULL, 0.01);"
        );
    }

    #[test]
    fn multi_row_inserts_and_qualified_names() {
        let options = ExportOptions {
            format: ExportFormat::SqlInsert,
            rows_per_insert: 100,
            qualify_table: true,
            ..ExportOptions::default()
        };
        assert_eq!(
            write(options, DriverKind::Postgres),
            "INSERT INTO \"shop\".\"items\" (\"id\", \"name\", \"price\") VALUES (1, 'a, \"b\"', 10.50),\n       (2, NULL, 0.01);\n"
        );
    }

    #[test]
    fn markdown_table() {
        let out = render(ExportFormat::Markdown, &data(), "t", DriverKind::Postgres);
        assert!(out.starts_with("| id | name | price |\n|---|---|---|\n| 1 | a, \"b\" | 10.50 |"));
    }

    #[test]
    fn html_and_xml_escape_text() {
        let mut data = data();
        data.rows[0][1] = Value::Text("<b>&\"".into());
        let html_out = render(ExportFormat::Html, &data, "t", DriverKind::Postgres);
        assert!(html_out.contains("<th>name</th>"));
        assert!(html_out.contains("<td>&lt;b&gt;&amp;&quot;</td>"));
        assert!(html_out.trim_end().ends_with("</html>"));
        let xml_out = render(ExportFormat::Xml, &data, "t", DriverKind::Postgres);
        assert!(xml_out.contains("<column name=\"name\">&lt;b&gt;&amp;&quot;</column>"));
        assert!(xml_out.contains("<column name=\"name\" null=\"true\"/>"));
        assert!(xml_out.trim_end().ends_with("</data>"));
    }
}
