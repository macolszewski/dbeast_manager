//! Background transfers against the test containers and local SQLite files:
//! streamed export, stopping, and the throughput of a large export.

use dbeast::backend::{Backend, Event, ExportItem, ExportJob, TaskEvent};
use dbeast::export::{ExportFormat, ExportOptions, TableName};
use dbeast::model::{BoundQuery, DataSource, DriverKind};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn pg() -> Option<DataSource> {
    if let Err(error) = std::net::TcpStream::connect("127.0.0.1:55432") {
        eprintln!("skipping PostgreSQL transfer test: TCP probe failed: {error}");
        return None;
    }
    let mut ds = DataSource::new(DriverKind::Postgres);
    ds.port = 55432;
    ds.user = "dbeast".into();
    ds.password = "dbeast".into();
    ds.database = "dbeast".into();
    Some(ds)
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dbeast_transfer_{name}_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Collects log lines until the task finishes.
fn outcome(backend: &Backend, id: u64, timeout: Duration) -> (Result<String, String>, Vec<String>) {
    let deadline = Instant::now() + timeout;
    let mut log = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match backend.rx.recv_timeout(left).expect("transfer outcome") {
            Event::Task { id: task, event } if task == id => match event {
                TaskEvent::Finished(result) => return (result, log),
                TaskEvent::Log { text, .. } => log.push(text),
                TaskEvent::Lines(lines) => log.extend(lines.into_iter().map(|line| line.text)),
                TaskEvent::Progress(_) => {}
            },
            _ => {}
        }
    }
}

fn item(ds: &DataSource, sql: &str, table: &str, path: &Path) -> ExportItem {
    ExportItem {
        ds: ds.clone(),
        query: BoundQuery { sql: sql.into(), params: Vec::new() },
        context: None,
        table: TableName { schema: Some("shop".into()), table: table.into() },
        path: path.into(),
    }
}

#[test]
fn sqlite_tables_export_to_one_file_each() {
    let dir = temp_dir("sqlite");
    let database = dir.join("crm.sqlite");
    let db = rusqlite::Connection::open(&database).unwrap();
    db.execute_batch(
        "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT, note TEXT);
         INSERT INTO people VALUES (1, 'Ada', 'first, \"quoted\"'), (2, 'Olek', NULL);
         CREATE TABLE cities (name TEXT, population INTEGER);
         INSERT INTO cities VALUES ('Kraków', 800000);",
    )
    .unwrap();
    let mut ds = DataSource::new(DriverKind::Sqlite);
    ds.file = database.to_string_lossy().into_owned();
    let backend = Backend::new(egui::Context::default());
    let out = dir.join("out");
    backend.export_data(
        1,
        ExportJob {
            items: vec![
                item(&ds, "SELECT * FROM people", "people", &out.join("people.json")),
                item(&ds, "SELECT * FROM cities", "cities", &out.join("cities.json")),
            ],
            options: ExportOptions::with_format(ExportFormat::Json),
        },
    );
    let (result, log) = outcome(&backend, 1, Duration::from_secs(20));
    assert_eq!(result.unwrap().split(" in ").next().unwrap(), "3 rows exported to 2 files");
    assert!(log.iter().any(|line| line.starts_with("2 rows,")), "{log:?}");
    let people: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.join("people.json")).unwrap()).unwrap();
    assert_eq!(people[0]["note"], "first, \"quoted\"");
    assert!(people[1]["note"].is_null());
    let cities: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.join("cities.json")).unwrap()).unwrap();
    assert_eq!(cities[0]["population"], 800000);
    assert_eq!(std::fs::read_dir(&out).unwrap().count(), 2, "no partial files are left");
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn a_failed_export_keeps_the_existing_file() {
    let dir = temp_dir("failed");
    let database = dir.join("crm.sqlite");
    rusqlite::Connection::open(&database).unwrap().execute_batch("CREATE TABLE t (id INTEGER);").unwrap();
    let mut ds = DataSource::new(DriverKind::Sqlite);
    ds.file = database.to_string_lossy().into_owned();
    let target = dir.join("t.csv");
    std::fs::write(&target, "previous export\n").unwrap();
    let backend = Backend::new(egui::Context::default());
    backend.export_data(
        2,
        ExportJob {
            items: vec![item(&ds, "SELECT * FROM missing_table", "t", &target)],
            options: ExportOptions::default(),
        },
    );
    let (result, _) = outcome(&backend, 2, Duration::from_secs(20));
    assert!(result.unwrap_err().contains("missing_table"));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "previous export\n");
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2, "no partial file is left");
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn stopping_an_export_removes_the_partial_file() {
    let Some(ds) = pg() else { return };
    let dir = temp_dir("stop");
    let target = dir.join("series.csv");
    let backend = Backend::new(egui::Context::default());
    backend.export_data(
        3,
        ExportJob {
            items: vec![item(&ds, "SELECT g, pg_sleep(0.001) FROM generate_series(1, 100000) g", "series", &target)],
            options: ExportOptions::default(),
        },
    );
    // Wait until rows flow, then stop.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Event::Task { id: 3, event: TaskEvent::Progress(progress) } =
            backend.rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap()
            && progress.rows > 0
        {
            break;
        }
    }
    backend.stop_task(3);
    let (result, _) = outcome(&backend, 3, Duration::from_secs(20));
    assert_eq!(result.unwrap_err(), "Stopped");
    assert!(!target.exists());
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "the partial file is removed");
    std::fs::remove_dir_all(dir).ok();
}

/// A million rows go through with flat memory. Prints the throughput.
#[test]
fn large_export_streams() {
    let Some(ds) = pg() else { return };
    let dir = temp_dir("large");
    let target = dir.join("large.csv");
    let backend = Backend::new(egui::Context::default());
    let started = Instant::now();
    backend.export_data(
        4,
        ExportJob {
            items: vec![item(
                &ds,
                "SELECT g AS id, 'user' || g AS name, now() AS created, g * 1.5 AS amount FROM generate_series(1, 1000000) g",
                "large",
                &target,
            )],
            options: ExportOptions::default(),
        },
    );
    let (result, _) = outcome(&backend, 4, Duration::from_secs(120));
    let summary = result.unwrap();
    let elapsed = started.elapsed();
    let size = std::fs::metadata(&target).unwrap().len();
    eprintln!(
        "large export: {summary}; {:.0} rows/s, {:.1} MB",
        1_000_000.0 / elapsed.as_secs_f64(),
        size as f64 / 1_048_576.0
    );
    assert!(summary.starts_with("1000000 rows exported"));
    let text = std::fs::read_to_string(&target).unwrap();
    assert_eq!(text.lines().count(), 1_000_001);
    std::fs::remove_dir_all(dir).ok();
}

fn mysql() -> Option<DataSource> {
    if let Err(error) = std::net::TcpStream::connect("127.0.0.1:53306") {
        eprintln!("skipping MySQL transfer test: TCP probe failed: {error}");
        return None;
    }
    let mut ds = DataSource::new(DriverKind::MySql);
    ds.host = "127.0.0.1".into();
    ds.port = 53306;
    ds.user = "dbeast".into();
    ds.password = "dbeast".into();
    ds.database = "shop".into();
    Some(ds)
}

/// Runs statements on a fresh connection and returns the rows of the last one.
fn sql(ds: &DataSource, statements: &[&str]) -> Vec<Vec<dbeast::model::Value>> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let mut conn = dbeast::drivers::Connection::open(ds).await.unwrap();
        let mut rows = Vec::new();
        for statement in statements {
            rows = conn.execute(statement, dbeast::model::Page::ALL).await.unwrap().rows;
        }
        rows
    })
}

fn import_job(ds: &DataSource, path: &Path, table: &str) -> dbeast::backend::ImportJob {
    dbeast::backend::ImportJob {
        ds: ds.clone(),
        path: path.into(),
        csv: dbeast::import::CsvOptions::default(),
        schema: None,
        table: table.into(),
        create: None,
        columns: Vec::new(),
        sources: Vec::new(),
        clear: false,
        on_error: dbeast::backend::OnError::Abort,
        errors_file: None,
    }
}

#[test]
fn sqlite_import_creates_a_table_and_skips_bad_rows() {
    use dbeast::model::Value;
    let dir = temp_dir("import_sqlite");
    let database = dir.join("crm.sqlite");
    rusqlite::Connection::open(&database).unwrap();
    let mut ds = DataSource::new(DriverKind::Sqlite);
    ds.file = database.to_string_lossy().into_owned();
    let file = dir.join("people.csv");
    std::fs::write(&file, "id,name,born\n1,Ada,1815-12-10\n2,\"Olek, \"\"the\"\" second\",\n1,Duplicate,2000-01-01\n3,\"two\nlines\",1990-05-01\n").unwrap();
    let preview = dbeast::import::preview(&file, &Default::default(), true, 10).unwrap();
    let columns: Vec<(String, String)> = preview
        .columns
        .iter()
        .zip(&preview.types)
        .map(|(name, ty)| (name.clone(), ty.sql(DriverKind::Sqlite).to_string()))
        .collect();
    let mut job = import_job(&ds, &file, "people");
    job.create = Some(dbeast::import::create_table_sql(DriverKind::Sqlite, None, "people", &columns).replace("\"id\" INTEGER", "\"id\" INTEGER PRIMARY KEY"));
    job.columns = preview.columns.clone();
    job.sources = vec![0, 1, 2];
    job.on_error = dbeast::backend::OnError::Skip { max: 10 };
    job.errors_file = Some(dir.join("people.errors.csv"));
    let backend = Backend::new(egui::Context::default());
    backend.import_data(10, job);
    let (result, log) = outcome(&backend, 10, Duration::from_secs(20));
    let summary = result.unwrap();
    assert!(summary.starts_with("3 rows imported into people"), "{summary}");
    assert!(summary.contains("1 row skipped"), "{summary}");
    assert!(log.iter().any(|line| line.starts_with("line 4:") && line.contains("UNIQUE")), "{log:?}");
    let rows = sql(&ds, &["SELECT id, name, born FROM people ORDER BY id"]);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[1][1], Value::Text("Olek, \"the\" second".into()));
    assert_eq!(rows[1][2], Value::Null, "empty fields are NULL");
    assert_eq!(rows[2][1], Value::Text("two\nlines".into()));
    let errors = std::fs::read_to_string(dir.join("people.errors.csv")).unwrap();
    assert!(errors.starts_with("1,Duplicate,2000-01-01,"), "{errors}");
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn one_bad_row_in_a_batch_skips_only_that_row() {
    use dbeast::model::Value;
    let dir = temp_dir("import_bisect");
    let database = dir.join("crm.sqlite");
    rusqlite::Connection::open(&database)
        .unwrap()
        .execute_batch("CREATE TABLE items (id INTEGER PRIMARY KEY, label TEXT NOT NULL)")
        .unwrap();
    let mut ds = DataSource::new(DriverKind::Sqlite);
    ds.file = database.to_string_lossy().into_owned();
    // 1000 records in one batch; id 437 (file line 438) has no label.
    let mut csv = String::from("id,label\n");
    for id in 1..=1000 {
        let label = if id == 437 { String::new() } else { format!("item {id}") };
        csv.push_str(&format!("{id},{label}\n"));
    }
    let file = dir.join("items.csv");
    std::fs::write(&file, csv).unwrap();
    let mut job = import_job(&ds, &file, "items");
    job.columns = vec!["id".into(), "label".into()];
    job.sources = vec![0, 1];
    job.on_error = dbeast::backend::OnError::Skip { max: 10 };
    job.errors_file = Some(dir.join("items.errors.csv"));
    let backend = Backend::new(egui::Context::default());
    backend.import_data(15, job.clone());
    let (result, log) = outcome(&backend, 15, Duration::from_secs(20));
    let summary = result.unwrap();
    assert!(summary.starts_with("999 rows imported into items") && summary.contains("1 row skipped"), "{summary}");
    let errors: Vec<&String> = log.iter().filter(|line| line.starts_with("line ")).collect();
    assert!(errors.len() == 1 && errors[0].starts_with("line 438:") && errors[0].contains("NOT NULL"), "{log:?}");
    let written = std::fs::read_to_string(dir.join("items.errors.csv")).unwrap();
    assert!(written.starts_with("437,,") && written.lines().count() == 1, "{written}");
    let rows = sql(&ds, &["SELECT count(*), min(id), max(id) FROM items WHERE id <> 437"]);
    assert_eq!(rows[0], vec![Value::Int(999), Value::Int(1), Value::Int(1000)]);

    // Abort stops at the same row and leaves nothing behind.
    sql(&ds, &["DELETE FROM items"]);
    job.on_error = dbeast::backend::OnError::Abort;
    job.errors_file = None;
    backend.import_data(16, job);
    let (result, _) = outcome(&backend, 16, Duration::from_secs(20));
    let error = result.unwrap_err();
    assert!(error.starts_with("line 438:"), "{error}");
    assert_eq!(sql(&ds, &["SELECT count(*) FROM items"])[0][0], Value::Int(0));
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn postgres_import_maps_columns_and_rolls_back_on_error() {
    use dbeast::model::Value;
    let Some(ds) = pg() else { return };
    let table = format!("import_{}", std::process::id());
    sql(&ds, &[&format!("DROP TABLE IF EXISTS {table}"), &format!("CREATE TABLE {table} (id int PRIMARY KEY, label text, amount numeric(10,2), seen date, extra text DEFAULT 'kept')"), &format!("INSERT INTO {table} (id, label) VALUES (99, 'old')")]);
    let dir = temp_dir("import_pg");
    let file = dir.join("data.csv");
    std::fs::write(&file, "amount;id;label;seen\n10.50;1;\"semi;colon\";2026-10-17\n\\N;2;\"multi\nline \"\"quoted\"\"\";\\N\n3.25;3;\\N;2026-01-01\n").unwrap();
    let mut job = import_job(&ds, &file, &table);
    job.csv.delimiter = b';';
    job.csv.null_text = "\\N".into();
    job.columns = vec!["id".into(), "label".into(), "amount".into(), "seen".into()];
    job.sources = vec![1, 2, 0, 3];
    job.clear = true;
    let backend = Backend::new(egui::Context::default());
    backend.import_data(11, job.clone());
    let (result, _) = outcome(&backend, 11, Duration::from_secs(20));
    assert!(result.unwrap().starts_with("3 rows imported"));
    let rows = sql(&ds, &[&format!("SELECT id, label, amount::text, seen::text, extra FROM {table} ORDER BY id")]);
    assert_eq!(rows.len(), 3, "existing rows were deleted first");
    assert_eq!(rows[0][1], Value::Text("semi;colon".into()));
    assert_eq!(rows[0][2], Value::Text("10.50".into()));
    assert_eq!(rows[1][1], Value::Text("multi\nline \"quoted\"".into()));
    assert_eq!(rows[1][2], Value::Null, "the NULL text becomes NULL");
    assert_eq!(rows[2][1], Value::Null);
    assert_eq!(rows[2][4], Value::Text("kept".into()), "unmapped columns get their default");

    // A bad value stops the import and leaves the table as it was.
    std::fs::write(&file, "amount;id;label;seen\n1;10;a;2026-10-17\nnot-a-number;11;b;2026-10-17\n").unwrap();
    job.clear = false;
    job.csv.null_text = String::new();
    backend.import_data(12, job);
    let (result, _) = outcome(&backend, 12, Duration::from_secs(20));
    let error = result.unwrap_err();
    assert!(error.starts_with("line 3:") && error.contains("numeric"), "{error}");
    assert_eq!(sql(&ds, &[&format!("SELECT count(*) FROM {table}")])[0][0], Value::Int(3));
    sql(&ds, &[&format!("DROP TABLE {table}")]);
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn mysql_import_into_a_new_table() {
    use dbeast::model::Value;
    let Some(ds) = mysql() else { return };
    let table = format!("import_{}", std::process::id());
    sql(&ds, &[&format!("DROP TABLE IF EXISTS {table}")]);
    let dir = temp_dir("import_mysql");
    let file = dir.join("data.tsv");
    std::fs::write(&file, "id\tcity\tpopulation\n1\tKraków\t800000\n2\tGdańsk\t470000\n").unwrap();
    let mut job = import_job(&ds, &file, &table);
    job.csv.delimiter = b'\t';
    job.create = Some(dbeast::import::create_table_sql(
        DriverKind::MySql,
        None,
        &table,
        &[("id".into(), "INTEGER".into()), ("city".into(), "TEXT".into()), ("population".into(), "BIGINT".into())],
    ));
    job.columns = vec!["id".into(), "city".into(), "population".into()];
    job.sources = vec![0, 1, 2];
    let backend = Backend::new(egui::Context::default());
    backend.import_data(13, job);
    let (result, _) = outcome(&backend, 13, Duration::from_secs(20));
    assert!(result.unwrap().starts_with("2 rows imported"));
    let rows = sql(&ds, &[&format!("SELECT city, population FROM {table} ORDER BY id")]);
    assert_eq!(rows[0][0], Value::Text("Kraków".into()));
    assert_eq!(rows[1][1], Value::Int(470000));
    sql(&ds, &[&format!("DROP TABLE {table}")]);
    std::fs::remove_dir_all(dir).ok();
}

/// InnoDB rolls back a failed import; MyISAM cannot, so options that rely on
/// rollback are refused before any change and Skip mode writes row by row.
#[test]
fn mysql_import_respects_non_transactional_engines() {
    use dbeast::backend::OnError;
    use dbeast::model::Value;
    let Some(ds) = mysql() else { return };
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let (innodb, myisam, keyless) = (format!("import_innodb_{suffix}"), format!("import_myisam_{suffix}"), format!("import_keyless_{suffix}"));
    let dir = temp_dir("import_engines");
    let file = dir.join("data.csv");
    // Line 4 repeats id 2.
    std::fs::write(&file, "id,label\n1,a\n2,b\n2,dup\n3,c\n").unwrap();
    let count = |table: &str| sql(&ds, &[&format!("SELECT count(*) FROM {table}")])[0][0].clone();
    let backend = Backend::new(egui::Context::default());
    let mut next_id = 30;
    let mut import = |table: &str, clear: bool, on_error: OnError| {
        let mut job = import_job(&ds, &file, table);
        job.columns = vec!["id".into(), "label".into()];
        job.sources = vec![0, 1];
        job.clear = clear;
        job.on_error = on_error;
        next_id += 1;
        backend.import_data(next_id, job);
        outcome(&backend, next_id, Duration::from_secs(20))
    };
    let test = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        for (table, engine) in [(&innodb, "InnoDB"), (&myisam, "MyISAM")] {
            sql(&ds, &[
                &format!("CREATE TABLE {table} (id integer PRIMARY KEY, label varchar(10)) ENGINE={engine}"),
                &format!("INSERT INTO {table} VALUES (99, 'old')"),
            ]);
        }

        // Control: InnoDB rolls back the delete and the partial import.
        let (result, _) = import(&innodb, true, OnError::Abort);
        let error = result.unwrap_err();
        assert!(error.starts_with("line 4:"), "{error}");
        assert_eq!(count(&innodb), Value::Int(1));

        for (clear, on_error) in [(true, OnError::Abort), (false, OnError::Abort), (true, OnError::Skip { max: 10 })] {
            let (result, _) = import(&myisam, clear, on_error);
            let error = result.unwrap_err();
            assert!(error.contains("MyISAM engine, which cannot roll back") && error.contains("Nothing was changed"), "{error}");
            assert_eq!(sql(&ds, &[&format!("SELECT id FROM {myisam}")]), vec![vec![Value::Int(99)]], "the old row stays");
        }

        let (result, log) = import(&myisam, false, OnError::Skip { max: 10 });
        let summary = result.unwrap();
        assert!(summary.starts_with("3 rows imported") && summary.contains("1 row skipped"), "{summary}");
        let errors: Vec<&String> = log.iter().filter(|line| line.starts_with("line ")).collect();
        assert!(errors.len() == 1 && errors[0].starts_with("line 4:"), "{log:?}");
        assert_eq!(count(&myisam), Value::Int(4));

        // Without a key nothing deduplicates a retried row: each goes in once.
        sql(&ds, &[&format!("CREATE TABLE {keyless} (id integer NOT NULL, label varchar(3)) ENGINE=MyISAM")]);
        std::fs::write(&file, "id,label\n1,a\n2,b\n3,too long\n4,d\n").unwrap();
        let (result, log) = import(&keyless, false, OnError::Skip { max: 10 });
        let summary = result.unwrap();
        assert!(summary.starts_with("3 rows imported") && summary.contains("1 row skipped"), "{summary}");
        assert!(log.iter().any(|line| line.starts_with("line 4:")), "{log:?}");
        assert_eq!(sql(&ds, &[&format!("SELECT count(*), count(DISTINCT id) FROM {keyless}")])[0], vec![Value::Int(3), Value::Int(3)]);
    }));
    for table in [&innodb, &myisam, &keyless] {
        sql(&ds, &[&format!("DROP TABLE IF EXISTS {table}")]);
    }
    std::fs::remove_dir_all(dir).ok();
    if let Err(panic) = test {
        std::panic::resume_unwind(panic);
    }
}

/// A million records load through COPY. Prints the throughput.
#[test]
fn large_import_uses_copy() {
    use std::io::Write;
    let Some(ds) = pg() else { return };
    let table = format!("import_large_{}", std::process::id());
    sql(&ds, &[&format!("DROP TABLE IF EXISTS {table}"), &format!("CREATE TABLE {table} (id bigint, name text, created timestamp, amount numeric)")]);
    let dir = temp_dir("import_large");
    let file = dir.join("large.csv");
    let mut out = std::io::BufWriter::new(std::fs::File::create(&file).unwrap());
    writeln!(out, "id,name,created,amount").unwrap();
    for i in 0..1_000_000u64 {
        writeln!(out, "{i},user{i},2026-10-17 10:00:00,{}.5", i % 1000).unwrap();
    }
    drop(out);
    let mut job = import_job(&ds, &file, &table);
    job.columns = vec!["id".into(), "name".into(), "created".into(), "amount".into()];
    job.sources = vec![0, 1, 2, 3];
    let backend = Backend::new(egui::Context::default());
    let started = Instant::now();
    backend.import_data(14, job);
    let (result, _) = outcome(&backend, 14, Duration::from_secs(300));
    let summary = result.unwrap();
    eprintln!("large import: {summary}; {:.0} rows/s", 1_000_000.0 / started.elapsed().as_secs_f64());
    assert!(summary.starts_with("1000000 rows imported"));
    assert_eq!(sql(&ds, &[&format!("SELECT count(*) FROM {table}")])[0][0], dbeast::model::Value::Int(1_000_000));
    sql(&ds, &[&format!("DROP TABLE {table}")]);
    std::fs::remove_dir_all(dir).ok();
}

fn pg_tool(tool: dbeast::dump::Tool) -> Option<PathBuf> {
    let found = dbeast::dump::find_tool(tool, "");
    if found.is_none() {
        eprintln!("skipping: {} is not installed", tool.binary());
    }
    found
}

#[test]
fn pg_dump_then_pg_restore_and_psql_round_trip() {
    use dbeast::backend::{ToolJob, ToolTask};
    use dbeast::dump::{DumpFormat, DumpOptions, DumpTarget, RestoreOptions, Tool};
    use dbeast::model::Value;
    let Some(ds) = pg() else { return };
    let (Some(pg_dump), Some(pg_restore), Some(psql)) = (pg_tool(Tool::PgDump), pg_tool(Tool::PgRestore), pg_tool(Tool::Psql)) else {
        return;
    };
    let suffix = std::process::id();
    let schema = format!("dump_src_{suffix}");
    let restored_db = format!("dbeast_restore_{suffix}");
    let psql_db = format!("dbeast_psql_{suffix}");
    sql(&ds, &[
        &format!("DROP SCHEMA IF EXISTS {schema} CASCADE"),
        &format!("CREATE SCHEMA {schema}"),
        &format!("CREATE TABLE {schema}.items (id int PRIMARY KEY, label text)"),
        &format!("INSERT INTO {schema}.items SELECT g, 'item ' || g FROM generate_series(1, 500) g"),
        &format!("DROP DATABASE IF EXISTS {restored_db}"),
        &format!("CREATE DATABASE {restored_db}"),
        &format!("DROP DATABASE IF EXISTS {psql_db}"),
        &format!("CREATE DATABASE {psql_db}"),
    ]);
    let dir = temp_dir("pg_dump");
    let backend = Backend::new(egui::Context::default());

    // Custom archive of one schema, restored with pg_restore.
    let archive = dir.join("items.dump");
    backend.run_tool(20, ToolJob {
        program: pg_dump.clone(),
        ds: ds.clone(),
        task: ToolTask::Dump {
            options: DumpOptions { format: DumpFormat::Custom, no_owner: true, ..DumpOptions::default() },
            target: DumpTarget { output: archive.clone(), schemas: vec![schema.clone()], tables: vec![] },
        },
    });
    let (result, log) = outcome(&backend, 20, Duration::from_secs(60));
    assert!(result.as_ref().is_ok_and(|s| s.contains("written to")), "{result:?} {log:?}");
    assert!(log[0].contains("--format c") && log[0].contains("--no-password"), "the command is logged first: {}", log[0]);
    assert!(!log.iter().any(|line| line.contains("PGPASSWORD") || line.contains("password=")), "no secret in the log");
    assert!(log.iter().any(|line| line.contains("dumping contents of table")), "verbose output streams: {log:?}");
    assert!(archive.is_file());
    let mut target = ds.clone();
    target.database = restored_db.clone();
    backend.run_tool(21, ToolJob {
        program: pg_restore,
        ds: target.clone(),
        task: ToolTask::Restore { options: RestoreOptions { no_owner: true, ..RestoreOptions::default() }, input: archive },
    });
    let (result, log) = outcome(&backend, 21, Duration::from_secs(60));
    assert!(result.is_ok(), "{result:?} {log:?}");
    assert_eq!(sql(&target, &[&format!("SELECT count(*) FROM {schema}.items")])[0][0], Value::Int(500));

    // Plain SQL with INSERTs, restored with psql.
    let script = dir.join("items.sql");
    backend.run_tool(22, ToolJob {
        program: pg_dump,
        ds: ds.clone(),
        task: ToolTask::Dump {
            options: DumpOptions { statements: dbeast::dump::Statements::Inserts, rows_per_insert: 100, no_owner: true, ..DumpOptions::default() },
            target: DumpTarget { output: script.clone(), schemas: vec![], tables: vec![format!("{schema}.items")] },
        },
    });
    let (result, _) = outcome(&backend, 22, Duration::from_secs(60));
    assert!(result.is_ok(), "{result:?}");
    let text = std::fs::read_to_string(&script).unwrap();
    assert!(text.contains(&format!("INSERT INTO {schema}.items VALUES")), "rows as INSERTs");
    let mut psql_target = ds.clone();
    psql_target.database = psql_db.clone();
    sql(&psql_target, &[&format!("CREATE SCHEMA {schema}")]);
    backend.run_tool(23, ToolJob {
        program: psql,
        ds: psql_target.clone(),
        task: ToolTask::Psql { options: RestoreOptions { single_transaction: true, ..RestoreOptions::default() }, input: script },
    });
    let (result, log) = outcome(&backend, 23, Duration::from_secs(60));
    assert!(result.is_ok(), "{result:?} {log:?}");
    assert_eq!(sql(&psql_target, &[&format!("SELECT count(*) FROM {schema}.items")])[0][0], Value::Int(500));

    sql(&ds, &[&format!("DROP SCHEMA {schema} CASCADE"), &format!("DROP DATABASE {restored_db}"), &format!("DROP DATABASE {psql_db}")]);
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn a_failed_pg_dump_reports_the_error_and_leaves_no_file() {
    use dbeast::backend::{ToolJob, ToolTask};
    use dbeast::dump::{DumpOptions, DumpTarget, Tool};
    let Some(mut ds) = pg() else { return };
    let Some(pg_dump) = pg_tool(Tool::PgDump) else { return };
    ds.password = "wrong".into();
    let dir = temp_dir("pg_dump_failed");
    let output = dir.join("shop.sql");
    let backend = Backend::new(egui::Context::default());
    backend.run_tool(24, ToolJob {
        program: pg_dump,
        ds,
        task: ToolTask::Dump { options: DumpOptions::default(), target: DumpTarget { output: output.clone(), schemas: vec![], tables: vec![] } },
    });
    let (result, _) = outcome(&backend, 24, Duration::from_secs(60));
    let error = result.unwrap_err();
    assert!(error.contains("pg_dump exited with code 1") && error.contains("password authentication failed"), "{error}");
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    std::fs::remove_dir_all(dir).ok();
}

fn export_to(backend: &Backend, id: u64, ds: &DataSource, query: &str, table: TableName, options: ExportOptions, path: &Path) {
    backend.export_data(
        id,
        ExportJob {
            items: vec![ExportItem {
                ds: ds.clone(),
                query: BoundQuery { sql: query.into(), params: Vec::new() },
                context: None,
                table,
                path: path.into(),
            }],
            options,
        },
    );
    let (result, _) = outcome(backend, id, Duration::from_secs(30));
    result.unwrap();
}

/// Arrays, NUMERIC and INTERVAL values leave through SQL INSERT, CSV and JSON
/// exactly as PostgreSQL prints them, and the INSERT file loads back into an
/// equal table.
#[test]
fn postgres_values_survive_export_unchanged() {
    use dbeast::model::Value;
    let Some(ds) = pg() else { return };
    let src = format!("dbeast_values_src_{}", std::process::id());
    let dst = format!("dbeast_values_dst_{}", std::process::id());
    sql(&ds, &[
        &format!("drop table if exists public.{src}, public.{dst}"),
        &format!("create table public.{src} (id int primary key, ta text[], ia int[], fa float8[], na numeric[], n numeric, iv interval)"),
        &format!(
            r#"insert into public.{src} values
             (1, array['a,b', 'NULL', NULL, '', 'say "hi"', 'back\slash', '{{braces}}', ' padded ', 'tab	x'], NULL,
              '{{0.1,1e20,-Infinity,NaN}}', '{{1e40,NaN,0.10}}', 1e40, '1 day 02:03:04.123456'),
             (2, '{{{{a,b}},{{c,d}}}}', '{{{{1,2}},{{3,NULL}}}}', NULL, NULL,
              12345678901234567890123456789012345.123, '-1 hour -30 minutes'),
             (3, '[0:1]={{x,y}}', '[0:1]={{5,6}}', '{{}}', '{{}}', 0.1234567890123456789012345678901, '-1 days +02:00:00.000001'),
             (4, '{{}}', '{{}}', NULL, NULL, 'NaN', '1 year 2 mons -3 days -00:00:00.5')"#
        ),
        &format!("create table public.{dst} (like public.{src})"),
    ]);
    let query = format!("select * from public.{src} order by id");
    let dir = temp_dir("pg_values");
    let backend = Backend::new(egui::Context::default());

    let mut options = ExportOptions::with_format(ExportFormat::SqlInsert);
    options.qualify_table = true;
    let insert = dir.join("values.sql");
    export_to(&backend, 30, &ds, &query, TableName { schema: Some("public".into()), table: dst.clone() }, options, &insert);
    let script = std::fs::read_to_string(&insert).unwrap();
    assert!(!script.contains("<numeric>") && !script.contains("<error"), "{script}");
    let statements: Vec<String> = dbeast::splitter::split(&script, DriverKind::Postgres)
        .into_iter()
        .map(|statement| statement.text)
        .collect();
    let statements: Vec<&str> = statements.iter().map(String::as_str).collect();
    sql(&ds, &statements);
    let differences = sql(&ds, &[&format!(
        "select count(*) from public.{src} s full join public.{dst} d on s.id = d.id
          where not (row(s.*) is not distinct from row(d.*)) or row(s.*)::text is distinct from row(d.*)::text"
    )]);
    assert_eq!(differences, vec![vec![Value::Int(0)]], "{script}");

    // CSV and JSON carry the server's own text.
    let expected: Vec<Vec<Option<String>>> = sql(&ds, &[&format!(
        "select id::text, ta::text, ia::text, fa::text, na::text, n::text, iv::text from public.{src} order by id"
    )])
    .into_iter()
    .map(|row| row.into_iter().map(|v| match v { Value::Text(t) => Some(t), Value::Null => None, other => panic!("{other:?}") }).collect())
    .collect();
    let csv_path = dir.join("values.csv");
    export_to(&backend, 31, &ds, &query, TableName::new("values"), ExportOptions::default(), &csv_path);
    let mut reader = csv::Reader::from_path(&csv_path).unwrap();
    let csv_rows: Vec<Vec<String>> = reader.records().map(|r| r.unwrap().iter().map(str::to_owned).collect()).collect();
    for (row, expected) in csv_rows.iter().zip(&expected) {
        let expected: Vec<String> = expected.iter().map(|v| v.clone().unwrap_or_default()).collect();
        assert_eq!(row, &expected);
    }
    let json_path = dir.join("values.json");
    export_to(&backend, 32, &ds, &query, TableName::new("values"), ExportOptions::with_format(ExportFormat::Json), &json_path);
    let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&json_path).unwrap()).unwrap();
    for (row, expected) in json.as_array().unwrap().iter().zip(&expected) {
        for (key, expected) in ["id", "ta", "ia", "fa", "na", "n", "iv"].iter().zip(expected) {
            let value = match &row[key] {
                serde_json::Value::Null => None,
                serde_json::Value::String(text) => Some(text.clone()),
                number => Some(number.to_string()),
            };
            assert_eq!(&value, expected, "{key}");
        }
    }
    sql(&ds, &[&format!("drop table public.{src}, public.{dst}")]);
    std::fs::remove_dir_all(dir).ok();
}

/// A value DBeast can only show as a placeholder stops the export instead of
/// being written in place of the stored value.
#[test]
fn postgres_export_stops_at_a_value_it_cannot_read_exactly() {
    let Some(ds) = pg() else { return };
    let dir = temp_dir("pg_lossy");
    let target = dir.join("money.csv");
    let backend = Backend::new(egui::Context::default());
    backend.export_data(
        33,
        ExportJob {
            items: vec![item(&ds, "select 1 as id, 12.5::money as price", "t", &target)],
            options: ExportOptions::default(),
        },
    );
    let (result, _) = outcome(&backend, 33, Duration::from_secs(20));
    let error = result.unwrap_err();
    assert!(error.contains("\"price\"") && error.contains("money"), "{error}");
    assert!(!target.exists());
    std::fs::remove_dir_all(dir).ok();
}
