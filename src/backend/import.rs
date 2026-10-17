//! Import Data from File as a background transfer. A blocking thread parses
//! the file into batches; the async side writes each batch under a savepoint
//! inside one transaction. A failed batch is split in halves until the bad
//! rows are found exactly; they either stop the import or are skipped and
//! logged, and the good rows around them still go in.

use super::transfer::{PROGRESS_INTERVAL, Progress, TaskId, elapsed_text, rows_text};
use super::{Backend, Event, fmt_err};
use crate::drivers::{Connection, InsertTarget};
use crate::import::{CsvOptions, Records};
use crate::model::{BoundQuery, DataSource, DriverKind, Page, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnError {
    /// Roll everything back at the first bad row.
    Abort,
    /// Skip bad rows until more than `max` have failed.
    Skip { max: u64 },
}

#[derive(Clone, Debug)]
pub struct ImportJob {
    pub ds: DataSource,
    pub path: PathBuf,
    pub csv: CsvOptions,
    pub schema: Option<String>,
    pub table: String,
    /// `CREATE TABLE` of a new target, run before the data.
    pub create: Option<String>,
    /// Target columns and, for each, the source field it is filled from.
    pub columns: Vec<String>,
    pub sources: Vec<usize>,
    /// Delete existing rows first, in the same transaction.
    pub clear: bool,
    pub on_error: OnError,
    /// Failed records with their error, as CSV.
    pub errors_file: Option<PathBuf>,
}

/// Rows per batch: large enough for COPY and multi-row INSERT to pay off.
fn batch_size(driver: DriverKind) -> usize {
    match driver {
        DriverKind::Postgres => 10_000,
        DriverKind::MySql | DriverKind::Sqlite => 2_000,
    }
}

/// Parsed records: the file line each starts on, the mapped values and the
/// raw fields. Kept as parallel vectors so any range of values goes to the
/// driver as a slice, without copying.
struct Rows {
    lines: Vec<u64>,
    values: Vec<Vec<Option<String>>>,
    raw: Vec<Vec<String>>,
}

impl Rows {
    fn with_capacity(size: usize) -> Self {
        Rows { lines: Vec::with_capacity(size), values: Vec::with_capacity(size), raw: Vec::with_capacity(size) }
    }

    fn len(&self) -> usize {
        self.lines.len()
    }
}

enum Batch {
    Rows(Rows),
    Failed(String),
}

impl Backend {
    pub fn import_data(&self, id: TaskId, job: ImportJob) {
        self.spawn_task(id, move |sender, token| async move {
            let started = std::time::Instant::now();
            let driver = job.ds.driver;
            let file_size = std::fs::metadata(&job.path)?.len();
            let mut conn = Connection::open(&job.ds).await?;
            if let Some(ddl) = &job.create {
                sender.log(format!("{ddl};"));
                conn.execute(ddl, Page::first(0)).await?;
            }
            let target = InsertTarget {
                schema: job.schema.clone(),
                table: job.table.clone(),
                columns: job.columns.clone(),
            };
            let qualified = crate::import::qualified(driver, job.schema.as_deref(), &job.table);
            let read_bytes = Arc::new(AtomicU64::new(0));
            let (tx, mut rx) = tokio::sync::mpsc::channel::<Batch>(4);
            let reader = {
                let (path, csv, sources, read_bytes) = (job.path.clone(), job.csv.clone(), job.sources.clone(), read_bytes.clone());
                let size = batch_size(driver);
                tokio::task::spawn_blocking(move || {
                    let mut records = match Records::open(&path, &csv) {
                        Ok(records) => records,
                        Err(error) => {
                            let _ = tx.blocking_send(Batch::Failed(fmt_err(error)));
                            return;
                        }
                    };
                    let mut batch = Rows::with_capacity(size);
                    loop {
                        match records.next_fields() {
                            Ok(Some(raw)) => {
                                let values = sources
                                    .iter()
                                    .map(|&i| raw.get(i).and_then(|field| csv.value(field)))
                                    .collect();
                                batch.lines.push(records.line);
                                batch.values.push(values);
                                batch.raw.push(raw);
                                if batch.len() == size {
                                    read_bytes.store(records.position(), Ordering::Relaxed);
                                    let full = std::mem::replace(&mut batch, Rows::with_capacity(size));
                                    if tx.blocking_send(Batch::Rows(full)).is_err() {
                                        return;
                                    }
                                }
                            }
                            Ok(None) => break,
                            Err(error) => {
                                let _ = tx.blocking_send(Batch::Failed(format!("line {}: {}", records.line + 1, fmt_err(error))));
                                return;
                            }
                        }
                    }
                    read_bytes.store(records.position(), Ordering::Relaxed);
                    if batch.len() > 0 {
                        let _ = tx.blocking_send(Batch::Rows(batch));
                    }
                })
            };
            let mut errors_out = match &job.errors_file {
                Some(path) => Some(csv::WriterBuilder::new().delimiter(job.csv.delimiter).from_path(path)?),
                None => None,
            };
            conn.execute(crate::drivers::begin_statement(driver), Page::first(0)).await?;
            let work = async {
                if job.clear {
                    sender.log(format!("DELETE FROM {qualified};"));
                    conn.execute(&format!("DELETE FROM {qualified}"), Page::first(0)).await?;
                }
                let mut imported = 0u64;
                let mut failed = 0u64;
                let mut ticker = tokio::time::interval(PROGRESS_INTERVAL);
                loop {
                    let batch = tokio::select! {
                        batch = rx.recv() => batch,
                        () = token.cancelled() => return Err(crate::drivers::stopped()),
                        _ = ticker.tick() => {
                            sender.progress(Progress { rows: imported, bytes: read_bytes.load(Ordering::Relaxed), errors: failed, item: 0, items: 1, total_bytes: file_size });
                            continue;
                        }
                    };
                    let rows = match batch {
                        None => break,
                        Some(Batch::Failed(error)) => anyhow::bail!(error),
                        Some(Batch::Rows(rows)) => rows,
                    };
                    // A failed range is split in halves, left first so rows go in
                    // file order; only the halves that fail are tried again.
                    let mut ranges = Vec::with_capacity(32);
                    ranges.push(0..rows.len());
                    while let Some(range) = ranges.pop() {
                        match insert_batch(&mut conn, &target, &rows.values[range.clone()]).await {
                            Ok(()) => imported += range.len() as u64,
                            Err(error) if range.len() == 1 => {
                                let (line, message) = (rows.lines[range.start], fmt_err(error));
                                let OnError::Skip { max } = job.on_error else {
                                    anyhow::bail!("line {line}: {message}");
                                };
                                anyhow::ensure!(failed < max, "line {line}: {message}");
                                failed += 1;
                                sender.error(format!("line {line}: {message}"));
                                if let Some(out) = errors_out.as_mut() {
                                    let mut record = rows.raw[range.start].clone();
                                    record.push(message);
                                    out.write_record(&record)?;
                                }
                            }
                            Err(_) => {
                                let middle = range.start + range.len() / 2;
                                ranges.push(middle..range.end);
                                ranges.push(range.start..middle);
                            }
                        }
                    }
                }
                anyhow::Ok((imported, failed))
            };
            let outcome = work.await;
            drop(rx);
            let _ = reader.await;
            if let Some(mut out) = errors_out {
                out.flush()?;
            }
            match outcome {
                Ok((imported, failed)) => {
                    conn.execute("COMMIT", Page::first(0)).await?;
                    sender.progress(Progress { rows: imported, bytes: file_size, errors: failed, item: 1, items: 1, total_bytes: file_size });
                    let mut summary = format!("{} imported into {} in {}", rows_text(imported), job.table, elapsed_text(started.elapsed()));
                    if failed > 0 {
                        summary.push_str(&format!(", {} skipped", rows_text(failed)));
                        if let Some(path) = &job.errors_file {
                            summary.push_str(&format!(" (see {})", path.display()));
                        }
                    }
                    Ok(summary)
                }
                Err(error) => {
                    // Nothing of a failed or stopped import stays in the table.
                    let _ = conn.execute("ROLLBACK", Page::first(0)).await;
                    if job.create.is_some() {
                        // The new table was created before the transaction began.
                        let _ = conn.execute(&format!("DROP TABLE {qualified}"), Page::first(0)).await;
                    }
                    Err(error)
                }
            }
        });
    }

    /// Columns of a table with their types, for the Import dialog's mapping.
    pub fn import_columns(&self, request: u64, ds: DataSource, schema: Option<String>, table: String) {
        let send = self.send();
        self.rt.spawn(async move {
            let result = async {
                let mut conn = Connection::open(&ds).await?;
                table_columns(&mut conn, ds.driver, schema.as_deref(), &table).await
            }
            .await;
            send(Event::ImportColumns { request, result: result.map_err(fmt_err) });
        });
    }

    /// First records of a file, parsed off the UI thread.
    pub fn import_preview(&self, request: u64, path: PathBuf, csv: CsvOptions, detect: bool) {
        let send = self.send();
        self.rt.spawn_blocking(move || {
            let result = crate::import::preview(&path, &csv, detect, 50);
            send(Event::ImportPreview { request, result: result.map_err(fmt_err) });
        });
    }
}

/// One batch under a savepoint, so a failure leaves earlier batches in place.
async fn insert_batch(conn: &mut Connection, target: &InsertTarget, rows: &[Vec<Option<String>>]) -> anyhow::Result<()> {
    conn.execute("SAVEPOINT dbeast_import", Page::first(0)).await?;
    match conn.insert_text_rows(target, rows).await {
        Ok(()) => {
            conn.execute("RELEASE SAVEPOINT dbeast_import", Page::first(0)).await?;
            Ok(())
        }
        Err(error) => {
            conn.execute("ROLLBACK TO SAVEPOINT dbeast_import", Page::first(0)).await?;
            conn.execute("RELEASE SAVEPOINT dbeast_import", Page::first(0)).await?;
            Err(error)
        }
    }
}

pub async fn table_columns(conn: &mut Connection, driver: DriverKind, schema: Option<&str>, table: &str) -> anyhow::Result<Vec<(String, String)>> {
    let query = match driver {
        DriverKind::Postgres => BoundQuery {
            sql: "SELECT a.attname::text, pg_catalog.format_type(a.atttypid, a.atttypmod) FROM pg_catalog.pg_attribute a WHERE a.attrelid = $1::text::regclass AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum".into(),
            params: vec![Value::Text(crate::import::qualified(driver, schema, table))],
        },
        DriverKind::MySql => BoundQuery {
            sql: "SELECT COLUMN_NAME, COLUMN_TYPE FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = COALESCE(?, DATABASE()) AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION".into(),
            params: vec![schema.map_or(Value::Null, |s| Value::Text(s.into())), Value::Text(table.into())],
        },
        DriverKind::Sqlite => BoundQuery {
            sql: "SELECT name, type FROM pragma_table_info(?, ?)".into(),
            params: vec![Value::Text(table.into()), Value::Text(schema.unwrap_or("main").into())],
        },
    };
    let data = conn.execute_bound_query(&query, Page::ALL, None).await?;
    anyhow::ensure!(!data.rows.is_empty(), "Table {table} has no columns or does not exist.");
    Ok(data
        .rows
        .into_iter()
        .map(|row| {
            let text = |v: Option<&Value>| match v {
                Some(Value::Text(s)) => s.clone(),
                Some(Value::Bytes(b)) => String::from_utf8_lossy(b).into_owned(),
                Some(other) => other.to_string(),
                None => String::new(),
            };
            (text(row.first()), text(row.get(1)))
        })
        .collect())
}
