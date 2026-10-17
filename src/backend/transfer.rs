//! Background transfers shown in the Run tool window: data export, import and
//! the PostgreSQL client tools. Each task reports progress, log lines and one
//! final outcome, and stops when its cancellation token fires.

use super::{Backend, Event, fmt_err};
use crate::drivers::{Connection, RowBatch};
use crate::export::{ExportOptions, TableName, Writer};
use crate::model::{BoundQuery, DataSource, DriverKind, ResolvedContext, Value};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub type TaskId = u64;

/// Counters of a running transfer; the UI derives rates from them.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Progress {
    pub rows: u64,
    pub bytes: u64,
    /// Rows that failed and were skipped (import).
    pub errors: u64,
    /// Index of the current file or table, and how many there are.
    pub item: usize,
    pub items: usize,
    /// Size of the input when it is known (import), for a share-done bar.
    pub total_bytes: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LogLine {
    pub error: bool,
    pub text: String,
}

#[derive(Debug)]
pub enum TaskEvent {
    Progress(Progress),
    Log { error: bool, text: String },
    /// Output collected between progress ticks: a chatty tool costs one event
    /// (and one repaint) per tick instead of one per line.
    Lines(Vec<LogLine>),
    /// Summary on success, the reason otherwise. Sent exactly once.
    Finished(Result<String, String>),
}

/// How often running transfers report their counters.
pub const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Clone)]
pub(super) struct TaskSender {
    pub id: TaskId,
    send: Arc<dyn Fn(Event) + Send + Sync>,
}

impl TaskSender {
    pub fn progress(&self, progress: Progress) {
        (self.send)(Event::Task {
            id: self.id,
            event: TaskEvent::Progress(progress),
        });
    }

    pub fn log(&self, text: impl Into<String>) {
        (self.send)(Event::Task {
            id: self.id,
            event: TaskEvent::Log {
                error: false,
                text: text.into(),
            },
        });
    }

    pub fn error(&self, text: impl Into<String>) {
        (self.send)(Event::Task {
            id: self.id,
            event: TaskEvent::Log {
                error: true,
                text: text.into(),
            },
        });
    }

    pub fn lines(&self, lines: Vec<LogLine>) {
        if lines.is_empty() {
            return;
        }
        (self.send)(Event::Task {
            id: self.id,
            event: TaskEvent::Lines(lines),
        });
    }

    fn finished(&self, result: Result<String, String>) {
        (self.send)(Event::Task {
            id: self.id,
            event: TaskEvent::Finished(result),
        });
    }
}

/// One table or result written to one file.
#[derive(Clone, Debug)]
pub struct ExportItem {
    pub ds: DataSource,
    pub query: BoundQuery,
    /// Namespace the query was written in; applied before it runs.
    pub context: Option<ResolvedContext>,
    pub table: TableName,
    pub path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct ExportJob {
    pub items: Vec<ExportItem>,
    pub options: ExportOptions,
}

/// Counts bytes on their way to the file, for the progress display.
struct Counting<W> {
    inner: W,
    bytes: Arc<AtomicU64>,
}

impl<W: Write> Write for Counting<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.bytes.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl Backend {
    pub(super) fn task_sender(&self, id: TaskId) -> TaskSender {
        let send = self.send();
        TaskSender {
            id,
            send: Arc::new(send),
        }
    }

    /// Registers a task and returns the token that stops it.
    pub(super) fn register_task(&self, id: TaskId) -> CancellationToken {
        let token = CancellationToken::new();
        self.tasks.lock().unwrap().insert(id, token.clone());
        token
    }

    pub fn stop_task(&self, id: TaskId) {
        if let Some(token) = self.tasks.lock().unwrap().get(&id) {
            token.cancel();
        }
    }

    /// Runs a task body and reports its outcome. The body watches the token
    /// itself, so it can clean up (partial files, child processes) on Stop.
    pub(super) fn spawn_task<F>(&self, id: TaskId, body: impl FnOnce(TaskSender, CancellationToken) -> F)
    where
        F: std::future::Future<Output = anyhow::Result<String>> + Send + 'static,
    {
        let token = self.register_task(id);
        let sender = self.task_sender(id);
        let tasks = self.tasks.clone();
        let future = body(sender.clone(), token.clone());
        self.rt.spawn(async move {
            let result = future.await;
            tasks.lock().unwrap().remove(&id);
            sender.finished(result.map_err(fmt_err));
        });
    }

    /// Exports each item to its file, streaming rows from the database to disk.
    /// A failed or stopped export leaves existing files as they were.
    pub fn export_data(&self, id: TaskId, job: ExportJob) {
        self.spawn_task(id, move |sender, token| async move {
            let items = job.items.len();
            let started = std::time::Instant::now();
            let mut total_rows = 0u64;
            let mut total_bytes = 0u64;
            let mut connection: Option<(DataSource, Connection)> = None;
            for (index, item) in job.items.into_iter().enumerate() {
                sender.log(format!("Exporting {} to {}", item.table.table, item.path.display()));
                let conn = match &mut connection {
                    Some((ds, conn)) if *ds == item.ds => conn,
                    _ => {
                        let conn = Connection::open(&item.ds).await?;
                        &mut connection.insert((item.ds.clone(), conn)).1
                    }
                };
                if let Some(context) = &item.context {
                    conn.apply_resolved_context(context).await?;
                }
                let base = Progress {
                    rows: total_rows,
                    bytes: total_bytes,
                    item: index,
                    items,
                    ..Progress::default()
                };
                let (rows, bytes) = export_item(conn, item, job.options.clone(), &sender, &token, base).await?;
                total_rows += rows;
                total_bytes += bytes;
            }
            sender.progress(Progress {
                rows: total_rows,
                bytes: total_bytes,
                item: items,
                items,
                ..Progress::default()
            });
            Ok(format!(
                "{} exported to {} in {}",
                rows_text(total_rows),
                if items == 1 { "1 file".to_string() } else { format!("{items} files") },
                elapsed_text(started.elapsed())
            ))
        });
    }
}

async fn export_item(
    conn: &mut Connection,
    item: ExportItem,
    options: ExportOptions,
    sender: &TaskSender,
    token: &CancellationToken,
    base: Progress,
) -> anyhow::Result<(u64, u64)> {
    let rows = Arc::new(AtomicU64::new(0));
    let bytes = Arc::new(AtomicU64::new(0));
    // Few batches in flight: the database read waits for the disk, not memory.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<RowBatch>(4);
    let partial = partial_path(&item.path);
    let writer = {
        let (rows, bytes) = (rows.clone(), bytes.clone());
        let path = partial.clone();
        let (driver, table) = (item.ds.driver, item.table.clone());
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let file = std::fs::File::create(&path)?;
            // Counting under the buffer touches the atomic once per flush, not per field.
            let out = std::io::BufWriter::with_capacity(1 << 20, Counting { inner: file, bytes });
            let mut writer = Writer::new(out, options, driver, table);
            while let Some(batch) = rx.blocking_recv() {
                match batch {
                    RowBatch::Columns(columns) => writer.begin(&columns)?,
                    RowBatch::Rows(batch) => {
                        writer.rows(&batch)?;
                        rows.fetch_add(batch.len() as u64, Ordering::Relaxed);
                    }
                }
            }
            writer.finish()?;
            Ok(())
        })
    };
    let mut read = Box::pin(conn.stream_rows(&item.query, tx));
    let mut ticker = tokio::time::interval(PROGRESS_INTERVAL);
    let read_result = loop {
        tokio::select! {
            result = &mut read => break result,
            () = token.cancelled() => break Err(crate::drivers::stopped()),
            _ = ticker.tick() => sender.progress(Progress {
                rows: base.rows + rows.load(Ordering::Relaxed),
                bytes: base.bytes + bytes.load(Ordering::Relaxed),
                ..base
            }),
        }
    };
    // Dropping the read closes the channel, so the writer ends. A write error
    // also closes it and makes the read report "Stopped": show the write error.
    drop(read);
    let outcome = match writer.await {
        Ok(Ok(())) => read_result.map(|_| ()),
        Ok(Err(error)) => Err(error),
        Err(error) => Err(error.into()),
    }
    .and_then(|()| Ok(std::fs::rename(&partial, &item.path)?));
    if let Err(error) = outcome {
        let _ = std::fs::remove_file(&partial);
        return Err(error);
    }
    let (rows, bytes) = (rows.load(Ordering::Relaxed), bytes.load(Ordering::Relaxed));
    sender.log(format!("{}, {} written to {}", rows_text(rows), bytes_text(bytes), item.path.display()));
    Ok((rows, bytes))
}

/// Rows shown in the preview of the Export and Import dialogs.
pub const PREVIEW_ROWS: usize = 10;

/// Tables and views of a schema (MySQL database, SQLite file), with a view flag.
pub async fn list_tables(conn: &mut Connection, driver: DriverKind, schema: Option<&str>) -> anyhow::Result<Vec<(String, bool)>> {
    let query = match (driver, schema) {
        (DriverKind::Sqlite, _) => BoundQuery {
            sql: "SELECT name, type = 'view' FROM sqlite_master WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%' ORDER BY name".into(),
            params: Vec::new(),
        },
        (DriverKind::Postgres, schema) => BoundQuery {
            sql: "SELECT c.relname::text, c.relkind IN ('v', 'm') FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = $1 AND c.relkind IN ('r', 'p', 'v', 'm', 'f') ORDER BY 1".into(),
            params: vec![Value::Text(schema.unwrap_or("public").into())],
        },
        (DriverKind::MySql, schema) => BoundQuery {
            sql: "SELECT TABLE_NAME, TABLE_TYPE = 'VIEW' FROM information_schema.TABLES WHERE TABLE_SCHEMA = COALESCE(?, DATABASE()) ORDER BY 1".into(),
            params: vec![schema.map_or(Value::Null, |s| Value::Text(s.into()))],
        },
    };
    let data = conn.execute_bound_query(&query, crate::model::Page::ALL, None).await?;
    Ok(data
        .rows
        .into_iter()
        .filter_map(|row| {
            let name = match row.first()? {
                Value::Text(name) => name.clone(),
                other => other.to_string(),
            };
            let view = matches!(row.get(1), Some(Value::Bool(true)) | Some(Value::Int(1)));
            Some((name, view))
        })
        .collect())
}

impl Backend {
    /// First rows of a query for a dialog preview, on a separate connection.
    pub fn transfer_preview(&self, request: u64, ds: DataSource, query: BoundQuery, context: Option<ResolvedContext>) {
        let send = self.send();
        self.rt.spawn(async move {
            let result = async {
                let mut conn = Connection::open(&ds).await?;
                if let Some(context) = &context {
                    conn.apply_resolved_context(context).await?;
                }
                conn.execute_bound_query(&query, crate::model::Page::first(PREVIEW_ROWS), None).await
            }
            .await;
            send(Event::TransferPreview {
                request,
                result: result.map_err(fmt_err),
            });
        });
    }

    /// Tables of a schema for the Export dialog's table list.
    pub fn transfer_tables(&self, request: u64, ds: DataSource, schema: Option<String>) {
        let send = self.send();
        self.rt.spawn(async move {
            let result = async {
                let mut conn = Connection::open(&ds).await?;
                list_tables(&mut conn, ds.driver, schema.as_deref()).await
            }
            .await;
            send(Event::TransferTables {
                request,
                result: result.map_err(fmt_err),
            });
        });
    }
}

/// Hidden file next to the target that becomes it when the write succeeds,
/// so a failed export never leaves a truncated file behind.
pub(super) fn partial_path(path: &std::path::Path) -> PathBuf {
    let name = path.file_name().map_or_else(|| "export".into(), |n| n.to_string_lossy().into_owned());
    path.with_file_name(format!(".{name}.part"))
}

pub fn rows_text(rows: u64) -> String {
    if rows == 1 { "1 row".into() } else { format!("{rows} rows") }
}

pub fn bytes_text(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "kB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

pub fn elapsed_text(elapsed: Duration) -> String {
    let ms = elapsed.as_millis();
    if ms < 1000 {
        format!("{ms} ms")
    } else if ms < 60_000 {
        format!("{:.1} s", elapsed.as_secs_f64())
    } else {
        format!("{} min {} s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texts_for_counts_sizes_and_times() {
        assert_eq!(rows_text(1), "1 row");
        assert_eq!(rows_text(1200), "1200 rows");
        assert_eq!(bytes_text(512), "512 B");
        assert_eq!(bytes_text(1536), "1.5 kB");
        assert_eq!(bytes_text(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(elapsed_text(Duration::from_millis(250)), "250 ms");
        assert_eq!(elapsed_text(Duration::from_millis(2500)), "2.5 s");
        assert_eq!(elapsed_text(Duration::from_secs(125)), "2 min 5 s");
    }
}
