//! DDL Diff reads fresh definitions on dedicated connections and never applies them.

use dbeast::backend::{Backend, DdlObjectRequest, Event, RunRequest, StructureCompareRequest};
use dbeast::ddl_diff::{ComparedDdl, DdlObjectIdentity};
use dbeast::model::{
    DataSource, DbObject, DriverKind, ExecutionContext, ObjectKind, Page, QueryResult, Value,
};
use dbeast::sqlgen;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

const CONSOLE: u64 = 701;
const OBSERVER: u64 = 702;
const TARGET: u64 = 703;
static TOKEN: AtomicU64 = AtomicU64::new(1);

struct LocalDatabases {
    directory: PathBuf,
    origin: DataSource,
    target: DataSource,
}

impl LocalDatabases {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!("dbeast_ddl_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let source = |name: &str| {
            let mut ds = DataSource::new(DriverKind::Sqlite);
            ds.file = directory.join(name).to_string_lossy().into_owned();
            rusqlite::Connection::open(&ds.file)
                .unwrap()
                .execute_batch("PRAGMA journal_mode=WAL")
                .unwrap();
            ds
        };
        let origin = source("origin.sqlite");
        let target = source("target.sqlite");
        Self {
            directory,
            origin,
            target,
        }
    }
}

impl Drop for LocalDatabases {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn object(ds: &DataSource, schema: &str, name: &str, kind: ObjectKind) -> DdlObjectRequest {
    let mut parent = Vec::new();
    if ds.driver == DriverKind::Postgres {
        parent.push(ds.database.clone());
    }
    parent.push(schema.into());
    let node = DbObject::new(kind, name, &parent);
    DdlObjectRequest {
        ds: ds.clone(),
        identity: DdlObjectIdentity {
            ds_id: ds.id.clone(),
            source: ds.identity(),
            kind,
            path: node.path.clone(),
        },
        node,
    }
}

fn done(backend: &Backend, token: u64) -> Result<ComparedDdl, String> {
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        match backend
            .rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("DDL comparison event before its bounded deadline")
        {
            Event::StructureCompareDone { request_id, result } => {
                assert_eq!(
                    request_id, token,
                    "comparison responses retain their request token"
                );
                return result.map(|boxed| *boxed);
            }
            Event::Ddl { .. } | Event::StatementDone { .. } | Event::RunFinished { .. } => {
                panic!("DDL Diff must not open SQL consoles or emit ordinary query results")
            }
            _ => {}
        }
    }
}

fn compare(
    backend: &Backend,
    origin: DdlObjectRequest,
    target: DdlObjectRequest,
) -> Result<ComparedDdl, String> {
    let token = TOKEN.fetch_add(1, Ordering::Relaxed);
    backend.compare_structure(StructureCompareRequest {
        request_id: token,
        origin,
        target,
        cancelled: Arc::new(AtomicBool::new(false)),
    });
    done(backend, token)
}

fn query(backend: &Backend, console_id: u64, ds: &DataSource, sql: &str) -> QueryResult {
    backend.run(RunRequest {
        console_id,
        ds: ds.clone(),
        statements: vec![sql.into()],
        page: Page::ALL,
        manual_tx: false,
        context: ExecutionContext::default(),
        expected_context: None,
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut result = None;
    loop {
        match backend
            .rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap()
        {
            Event::StatementDone {
                console_id: id,
                result: data,
                ..
            } if id == console_id => result = Some(data),
            Event::RunFinished { console_id: id, .. } if id == console_id => {
                return result.expect("one requested statement").unwrap();
            }
            _ => {}
        }
    }
}

fn assert_complete(diff: &ComparedDdl) {
    assert!(!diff.origin.sql.is_empty() && !diff.target.sql.is_empty());
    assert!(!diff.origin.coverage.is_empty() && !diff.target.coverage.is_empty());
    assert_eq!(
        diff.rows
            .iter()
            .filter_map(|row| row.origin)
            .collect::<Vec<_>>(),
        (0..diff.origin.lines.len()).collect::<Vec<_>>()
    );
    assert_eq!(
        diff.rows
            .iter()
            .filter_map(|row| row.target)
            .collect::<Vec<_>>(),
        (0..diff.target.lines.len()).collect::<Vec<_>>()
    );
    assert!(!diff.differences.is_empty());
}

#[test]
fn sqlite_fresh_definitions_ignore_trigger_name_collisions_and_preserve_console_transaction() {
    let local = LocalDatabases::new();
    let name = "orders \"Żółw";
    let ident = sqlgen::ident(DriverKind::Sqlite, name);
    let left = rusqlite::Connection::open(&local.origin.file).unwrap();
    left.execute_batch(&format!(
        "CREATE TABLE {ident}(id INTEGER PRIMARY KEY,note TEXT);\n\
         INSERT INTO {ident} VALUES(1,'committed');\n\
         CREATE INDEX note_index ON {ident}(note);\n\
         CREATE TRIGGER owned_trigger AFTER INSERT ON {ident} BEGIN SELECT 1; END;\n\
         CREATE TABLE other_table(id INTEGER);\n\
         CREATE TRIGGER {ident} AFTER INSERT ON other_table BEGIN SELECT 99; END;"
    ))
    .unwrap();
    let right = rusqlite::Connection::open(&local.target.file).unwrap();
    right.execute_batch(&format!("CREATE TABLE {ident}(id INTEGER PRIMARY KEY,note TEXT,quantity INTEGER DEFAULT 0); INSERT INTO {ident}(id,note) VALUES(1,'target');")).unwrap();
    let schema_before: Vec<(String, String, String)> = left
        .prepare("SELECT type,name,sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY type,name")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let backend = Backend::new(egui::Context::default());
    let original = query(
        &backend,
        CONSOLE,
        &local.origin,
        &format!("SELECT * FROM {ident}"),
    );
    query(&backend, CONSOLE, &local.origin, "BEGIN");
    query(
        &backend,
        CONSOLE,
        &local.origin,
        &format!("UPDATE {ident} SET note='pending' WHERE id=1"),
    );
    let diff = compare(
        &backend,
        object(&local.origin, "main", name, ObjectKind::Table),
        object(&local.target, "main", name, ObjectKind::Table),
    )
    .unwrap();
    assert_complete(&diff);
    assert!(diff.origin.sql.contains("note_index") && diff.origin.sql.contains("owned_trigger"));
    assert!(
        !diff.origin.sql.contains("other_table") && !diff.origin.sql.contains("SELECT 99"),
        "a trigger in a different namespace must not become the table definition"
    );
    assert!(!diff.origin.sql.contains("quantity") && diff.target.sql.contains("quantity"));
    assert_eq!(
        diff.origin.identity.source,
        local.origin.identity(),
        "forcing the dedicated connection read-only must not alter captured identity"
    );
    let pending = query(
        &backend,
        CONSOLE,
        &local.origin,
        &format!("SELECT * FROM {ident}"),
    );
    assert_eq!(pending.rows[0][1], Value::Text("pending".into()));
    assert_eq!(
        pending.source.as_ref().unwrap().session_epoch,
        original.source.as_ref().unwrap().session_epoch
    );
    assert_eq!(
        query(
            &backend,
            OBSERVER,
            &local.origin,
            &format!("SELECT * FROM {ident}")
        )
        .rows,
        original.rows
    );
    query(&backend, CONSOLE, &local.origin, "ROLLBACK");
    assert_eq!(
        query(
            &backend,
            CONSOLE,
            &local.origin,
            &format!("SELECT * FROM {ident}")
        )
        .rows,
        original.rows
    );
    let schema_after: Vec<(String, String, String)> = left
        .prepare("SELECT type,name,sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY type,name")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        schema_after, schema_before,
        "comparison must not apply any generated DDL"
    );

    // Picker descriptors are intentionally stale: the next request reads current DDL.
    right
        .execute_batch(&format!("ALTER TABLE {ident} ADD COLUMN fresh_column TEXT"))
        .unwrap();
    let refreshed = compare(
        &backend,
        object(&local.origin, "main", name, ObjectKind::Table),
        object(&local.target, "main", name, ObjectKind::Table),
    )
    .unwrap();
    assert!(refreshed.target.sql.contains("fresh_column"));
    assert!(
        !diff.target.sql.contains("fresh_column"),
        "completed snapshots remain immutable"
    );
    right
        .execute_batch(&format!(
            "DROP TABLE {ident}; CREATE VIEW {ident} AS SELECT 1 AS id;"
        ))
        .unwrap();
    assert!(
        compare(
            &backend,
            object(&local.origin, "main", name, ObjectKind::Table),
            object(&local.target, "main", name, ObjectKind::Table)
        )
        .is_err(),
        "a stale table descriptor must not accept a view"
    );
    left.execute_batch("CREATE VIEW visible_orders AS SELECT id,note FROM other_table LEFT JOIN (SELECT 'a' AS note) ON 1=1").unwrap();
    right
        .execute_batch("CREATE VIEW visible_orders AS SELECT 2 AS id,'b' AS note")
        .unwrap();
    let views = compare(
        &backend,
        object(&local.origin, "main", "visible_orders", ObjectKind::View),
        object(&local.target, "main", "visible_orders", ObjectKind::View),
    )
    .unwrap();
    assert_complete(&views);
    assert!(views.origin.sql.to_uppercase().contains("CREATE VIEW"));

    // The base table has gone, but an unrelated same-named trigger still exists.
    left.execute_batch(&format!("DROP TABLE {ident}")).unwrap();
    right
        .execute_batch("CREATE TABLE valid_target(id INTEGER)")
        .unwrap();
    let missing_base = compare(
        &backend,
        object(&local.origin, "main", name, ObjectKind::Table),
        object(&local.target, "main", "valid_target", ObjectKind::Table),
    )
    .unwrap_err();
    assert!(
        missing_base.starts_with("Origin:"),
        "the unrelated trigger must not satisfy a missing table: {missing_base}"
    );
}

#[test]
fn sqlite_missing_files_invalid_paths_stale_identity_and_cancel_never_create_databases() {
    let local = LocalDatabases::new();
    for ds in [&local.origin, &local.target] {
        rusqlite::Connection::open(&ds.file)
            .unwrap()
            .execute_batch("CREATE TABLE item(id INTEGER)")
            .unwrap();
    }
    let backend = Backend::new(egui::Context::default());
    let target = || object(&local.target, "main", "item", ObjectKind::Table);
    let mut missing = local.origin.clone();
    missing.file = local
        .directory
        .join("must-not-be-created.sqlite")
        .to_string_lossy()
        .into_owned();
    assert!(
        compare(
            &backend,
            object(&missing, "main", "item", ObjectKind::Table),
            target()
        )
        .is_err()
    );
    assert!(!std::path::Path::new(&missing.file).exists());
    for schema in ["temp", "attached"] {
        assert!(
            compare(
                &backend,
                object(&local.origin, schema, "item", ObjectKind::Table),
                target()
            )
            .is_err()
        );
    }
    let mut memory = local.origin.clone();
    memory.file = ":memory:".into();
    assert!(
        compare(
            &backend,
            object(&memory, "main", "item", ObjectKind::Table),
            target()
        )
        .is_err()
    );
    let mut stale = object(&local.origin, "main", "item", ObjectKind::Table);
    stale.ds.file = missing.file.clone();
    let error = compare(&backend, stale, target()).unwrap_err();
    assert!(
        error.contains("data source definition has changed"),
        "{error}"
    );
    assert!(!std::path::Path::new(&missing.file).exists());
    let mut wrong_path = object(&local.origin, "main", "item", ObjectKind::Table);
    wrong_path.node.name = "not_item".into();
    assert!(compare(&backend, wrong_path, target()).is_err());
    assert!(
        compare(
            &backend,
            object(&local.origin, "main", "item", ObjectKind::Table),
            object(&local.target, "main", "item", ObjectKind::View)
        )
        .is_err()
    );
    let token = TOKEN.fetch_add(1, Ordering::Relaxed);
    backend.compare_structure(StructureCompareRequest {
        request_id: token,
        origin: object(&missing, "main", "item", ObjectKind::Table),
        target: target(),
        cancelled: Arc::new(AtomicBool::new(true)),
    });
    assert!(
        done(&backend, token)
            .unwrap_err()
            .to_lowercase()
            .contains("cancel")
    );
    assert!(!std::path::Path::new(&missing.file).exists());
}

fn server(driver: DriverKind) -> Option<DataSource> {
    let port = if driver == DriverKind::Postgres {
        55432
    } else {
        53306
    };
    if let Err(error) = std::net::TcpStream::connect(("127.0.0.1", port)) {
        eprintln!("skipping {driver:?} DDL Diff integration: TCP probe failed: {error}");
        return None;
    }
    let mut ds = DataSource::new(driver);
    ds.host = "127.0.0.1".into();
    ds.port = port;
    ds.user = "dbeast".into();
    ds.password = "dbeast".into();
    ds.database = "dbeast".into();
    Some(ds)
}

fn exercise_server(ds: DataSource) {
    let backend = Backend::new(egui::Context::default());
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let name = format!("ddl_{} \"Ż", &suffix[..12]);
    let view_name = format!("ddl_view_{}", &suffix[..12]);
    let (origin_schema, target_schema) = if ds.driver == DriverKind::Postgres {
        (
            format!("ddl_left_{}", &suffix[..12]),
            format!("ddl_right_{}", &suffix[..12]),
        )
    } else {
        ("dbeast".into(), "shop".into())
    };
    if ds.driver == DriverKind::Postgres {
        for schema in [&origin_schema, &target_schema] {
            query(
                &backend,
                OBSERVER,
                &ds,
                &format!("CREATE SCHEMA {}", sqlgen::ident(ds.driver, schema)),
            );
        }
    }
    let origin = sqlgen::qualified(ds.driver, Some(&origin_schema), &name);
    let target = sqlgen::qualified(ds.driver, Some(&target_schema), &name);
    let origin_view = sqlgen::qualified(ds.driver, Some(&origin_schema), &view_name);
    let target_view = sqlgen::qualified(ds.driver, Some(&target_schema), &view_name);
    let engine = if ds.driver == DriverKind::MySql {
        " ENGINE=InnoDB"
    } else {
        ""
    };
    query(
        &backend,
        OBSERVER,
        &ds,
        &format!("CREATE TABLE {origin}(id INTEGER PRIMARY KEY,note VARCHAR(100)){engine}"),
    );
    query(
        &backend,
        TARGET,
        &ds,
        &format!(
            "CREATE TABLE {target}(id INTEGER PRIMARY KEY,note VARCHAR(100),quantity INTEGER DEFAULT 0){engine}"
        ),
    );
    query(
        &backend,
        OBSERVER,
        &ds,
        &format!("INSERT INTO {origin} VALUES(1,'committed')"),
    );
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let original = query(&backend, CONSOLE, &ds, &format!("SELECT * FROM {origin}"));
        let current_context = original.context.clone();
        query(&backend, CONSOLE, &ds, "BEGIN");
        query(
            &backend,
            CONSOLE,
            &ds,
            &format!("UPDATE {origin} SET note='pending' WHERE id=1"),
        );
        let diff = compare(
            &backend,
            object(&ds, &origin_schema, &name, ObjectKind::Table),
            object(&ds, &target_schema, &name, ObjectKind::Table),
        )
        .unwrap();
        assert_complete(&diff);
        assert!(!diff.origin.sql.contains("quantity") && diff.target.sql.contains("quantity"));
        assert_eq!(diff.origin.identity.path.last(), Some(&name));
        let pending = query(&backend, CONSOLE, &ds, &format!("SELECT * FROM {origin}"));
        assert_eq!(pending.rows[0][1], Value::Text("pending".into()));
        assert_eq!(pending.context, current_context);
        assert_eq!(
            pending.source.as_ref().unwrap().session_epoch,
            original.source.as_ref().unwrap().session_epoch
        );
        assert_eq!(
            query(&backend, OBSERVER, &ds, &format!("SELECT * FROM {origin}")).rows,
            original.rows
        );
        query(&backend, CONSOLE, &ds, "ROLLBACK");
        assert_eq!(
            query(&backend, CONSOLE, &ds, &format!("SELECT * FROM {origin}")).rows,
            original.rows
        );
        query(
            &backend,
            TARGET,
            &ds,
            &format!("ALTER TABLE {target} ADD COLUMN fresh_column INTEGER"),
        );
        let refreshed = compare(
            &backend,
            object(&ds, &origin_schema, &name, ObjectKind::Table),
            object(&ds, &target_schema, &name, ObjectKind::Table),
        )
        .unwrap();
        assert!(refreshed.target.sql.contains("fresh_column"));
        query(
            &backend,
            OBSERVER,
            &ds,
            &format!("CREATE VIEW {origin_view} AS SELECT id FROM {origin}"),
        );
        query(
            &backend,
            TARGET,
            &ds,
            &format!("CREATE VIEW {target_view} AS SELECT id,note FROM {target}"),
        );
        let views = compare(
            &backend,
            object(&ds, &origin_schema, &view_name, ObjectKind::View),
            object(&ds, &target_schema, &view_name, ObjectKind::View),
        )
        .unwrap();
        assert_complete(&views);
        assert!(
            compare(
                &backend,
                object(&ds, &origin_schema, &view_name, ObjectKind::Table),
                object(&ds, &target_schema, &name, ObjectKind::Table)
            )
            .is_err(),
            "fresh catalog kind must override a stale table descriptor"
        );
        if ds.driver == DriverKind::Postgres {
            let mut wrong_database = object(&ds, &origin_schema, &name, ObjectKind::Table);
            wrong_database.node.path[0] = "postgres".into();
            wrong_database.identity.path = wrong_database.node.path.clone();
            assert!(
                compare(
                    &backend,
                    wrong_database,
                    object(&ds, &target_schema, &name, ObjectKind::Table)
                )
                .is_err()
            );
        }
    }));
    // Always release the console transaction before cleanup, including failed assertions.
    query(&backend, CONSOLE, &ds, "ROLLBACK");
    for view in [&origin_view, &target_view] {
        query(
            &backend,
            OBSERVER,
            &ds,
            &format!("DROP VIEW IF EXISTS {view}"),
        );
    }
    for table in [&origin, &target] {
        query(
            &backend,
            OBSERVER,
            &ds,
            &format!("DROP TABLE IF EXISTS {table}"),
        );
    }
    if ds.driver == DriverKind::Postgres {
        for schema in [&origin_schema, &target_schema] {
            query(
                &backend,
                OBSERVER,
                &ds,
                &format!("DROP SCHEMA {}", sqlgen::ident(ds.driver, schema)),
            );
        }
    }
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

#[test]
fn postgres_quoted_paths_fresh_kinds_and_console_transaction_are_isolated() {
    if let Some(ds) = server(DriverKind::Postgres) {
        exercise_server(ds);
    }
}

#[test]
fn mysql_cross_database_definitions_do_not_change_console_use_or_transaction() {
    if let Some(ds) = server(DriverKind::MySql) {
        exercise_server(ds);
    }
}

#[test]
fn postgres_same_source_id_keeps_two_physical_database_definitions_distinct() {
    let Some(origin) = server(DriverKind::Postgres) else {
        return;
    };
    let target = origin.with_database("dbeast_archive");
    assert_eq!(origin.id, target.id);
    assert_ne!(origin.identity(), target.identity());
    let backend = Backend::new(egui::Context::default());
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let name = format!("ddl_physical_{} \"Ż", &suffix[..12]);
    let table = sqlgen::qualified(DriverKind::Postgres, Some("public"), &name);
    // Separate console IDs are essential: each represents a different physical
    // database, even though both descriptors share the same saved source ID.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        query(
            &backend,
            OBSERVER,
            &origin,
            &format!("CREATE TABLE {table}(id INTEGER PRIMARY KEY,from_primary TEXT)"),
        );
        query(
            &backend,
            TARGET,
            &target,
            &format!("CREATE TABLE {table}(id INTEGER PRIMARY KEY,from_archive INTEGER)"),
        );
        let diff = compare(
            &backend,
            object(&origin, "public", &name, ObjectKind::Table),
            object(&target, "public", &name, ObjectKind::Table),
        )
        .unwrap();
        assert_complete(&diff);
        assert_eq!(diff.origin.identity.ds_id, diff.target.identity.ds_id);
        assert_eq!(diff.origin.identity.source.database, "dbeast");
        assert_eq!(diff.target.identity.source.database, "dbeast_archive");
        assert_eq!(diff.origin.identity.path, ["dbeast", "public", &name]);
        assert_eq!(
            diff.target.identity.path,
            ["dbeast_archive", "public", &name]
        );
        assert!(
            diff.origin.sql.contains("from_primary") && !diff.origin.sql.contains("from_archive")
        );
        assert!(
            diff.target.sql.contains("from_archive") && !diff.target.sql.contains("from_primary")
        );
        let generated_name = format!("public.{}", sqlgen::ident(DriverKind::Postgres, &name));
        assert!(
            diff.origin.sql.contains(&generated_name) && diff.target.sql.contains(&generated_name),
            "identical qualified table names must still resolve on different physical connections"
        );
    }));
    // Cleanup both databases even when setup or an assertion fails.
    query(
        &backend,
        OBSERVER,
        &origin,
        &format!("DROP TABLE IF EXISTS {table}"),
    );
    query(
        &backend,
        TARGET,
        &target,
        &format!("DROP TABLE IF EXISTS {table}"),
    );
    if let Err(error) = outcome {
        std::panic::resume_unwind(error);
    }
}

fn benchmark_pair(ds: &DataSource) {
    let backend = Backend::new(egui::Context::default());
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let names = [
        format!("ddl_bench_left_{}", &suffix[..12]),
        format!("ddl_bench_right_{}", &suffix[..12]),
    ];
    let schema = match ds.driver {
        DriverKind::Postgres => "public",
        DriverKind::MySql => "dbeast",
        DriverKind::Sqlite => "main",
    };
    let tables = names
        .each_ref()
        .map(|name| sqlgen::qualified(ds.driver, Some(schema), name));
    let columns = (0..100)
        .map(|index| format!("column_{index:03} TEXT"))
        .collect::<Vec<_>>()
        .join(",");
    for (index, table) in tables.iter().enumerate() {
        let extra = if index == 1 {
            ",extra_column INTEGER"
        } else {
            ""
        };
        query(
            &backend,
            OBSERVER,
            ds,
            &format!("CREATE TABLE {table}(id INTEGER PRIMARY KEY,{columns}{extra})"),
        );
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let sample = || {
            let request = (
                object(ds, schema, &names[0], ObjectKind::Table),
                object(ds, schema, &names[1], ObjectKind::Table),
            );
            let started = Instant::now();
            let diff = compare(&backend, request.0, request.1).unwrap();
            let elapsed = started.elapsed();
            assert_complete(&diff);
            (elapsed, diff.origin.sql.len() + diff.target.sql.len())
        };
        let mut first = Duration::ZERO;
        for index in 0..3 {
            let (elapsed, _) = sample();
            if index == 0 {
                first = elapsed;
            }
        }
        let mut samples = Vec::with_capacity(7);
        let mut bytes = 0;
        for _ in 0..7 {
            let (elapsed, size) = sample();
            samples.push(elapsed);
            bytes = size;
        }
        samples.sort_unstable();
        println!(
            "DDL_DIFF_BACKEND driver={:?} columns=101/102 bytes={} warmup=3 samples=7 first_ms={:.3} median_ms={:.3} max_ms={:.3}; wall time includes two fresh dedicated connections, catalog reads, worker alignment and event delivery; fixture setup, assertions and rendering excluded",
            ds.driver,
            bytes,
            first.as_secs_f64() * 1000.0,
            samples[3].as_secs_f64() * 1000.0,
            samples[6].as_secs_f64() * 1000.0
        );
    }));
    for table in &tables {
        query(&backend, OBSERVER, ds, &format!("DROP TABLE {table}"));
    }
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

/// Each request creates two independent metadata connections. Warmup primes the
/// runtime/server/filesystem, not a connection or definition cache. No UI runs.
#[test]
#[ignore = "Run with --release --ignored --nocapture to measure read-only DDL refresh"]
#[allow(clippy::assertions_on_constants)]
fn ddl_refresh_backend_end_to_end() {
    assert!(!cfg!(debug_assertions), "run this benchmark with --release");
    let local = LocalDatabases::new();
    benchmark_pair(&local.origin);
    for driver in [DriverKind::Postgres, DriverKind::MySql] {
        if let Some(ds) = server(driver) {
            benchmark_pair(&ds);
        }
    }
}
