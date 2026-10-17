//! EXPLAIN is planned on the console's session without executing its statement.

use dbeast::backend::{
    Backend, Event, ExplainCompletion, ExplainExecution, ExplainRequest, RunRequest,
};
use dbeast::drivers::Connection;
use dbeast::model::{DataSource, DriverKind, ExecutionContext, Page, QueryResult, Value};
use dbeast::query_plan::{ExplainMode, QueryPlan};
use dbeast::sqlgen;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const SOURCE: u64 = 71;
const OBSERVER: u64 = 72;
static REQUEST: AtomicU64 = AtomicU64::new(1);

struct LocalDatabase {
    directory: PathBuf,
    source: DataSource,
}

impl LocalDatabase {
    fn new() -> Self {
        let directory =
            std::env::temp_dir().join(format!("dbeast_explain_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let mut source = DataSource::new(DriverKind::Sqlite);
        source.file = directory.join("plan.sqlite").to_string_lossy().into_owned();
        rusqlite::Connection::open(&source.file)
            .unwrap()
            .execute_batch("PRAGMA journal_mode=WAL")
            .unwrap();
        Self { directory, source }
    }
}

impl Drop for LocalDatabase {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn server(driver: DriverKind) -> Option<DataSource> {
    let port = if driver == DriverKind::Postgres {
        55432
    } else {
        53306
    };
    if let Err(error) = std::net::TcpStream::connect(("127.0.0.1", port)) {
        eprintln!("skipping {driver:?} Explain integration: TCP probe failed: {error}");
        return None;
    }
    let mut source = DataSource::new(driver);
    source.host = "127.0.0.1".into();
    source.port = port;
    source.user = "dbeast".into();
    source.password = "dbeast".into();
    source.database = "dbeast".into();
    Some(source)
}

fn receive(backend: &Backend, deadline: Instant) -> Event {
    backend
        .rx
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .expect("backend event before the Explain test deadline")
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
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut result = None;
    loop {
        match receive(backend, deadline) {
            Event::StatementDone {
                console_id: id,
                result: data,
                ..
            } if id == console_id => {
                assert!(result.is_none(), "one statement was requested");
                result = Some(data);
            }
            Event::RunFinished { console_id: id, .. } if id == console_id => {
                return result.expect("statement result").unwrap();
            }
            _ => {}
        }
    }
}

fn request(ds: &DataSource, sql: &str) -> ExplainRequest {
    analyse_request(ds, sql, ExplainMode::Plan, false)
}

fn analyse_request(
    ds: &DataSource,
    sql: &str,
    mode: ExplainMode,
    manual_tx: bool,
) -> ExplainRequest {
    ExplainRequest {
        console_id: SOURCE,
        request_id: REQUEST.fetch_add(1, Ordering::Relaxed),
        ds: ds.clone(),
        sql: sql.into(),
        bound_query: None,
        context: ExecutionContext::default(),
        mode,
        manual_tx,
    }
}

fn explain_done(backend: &Backend, token: u64) -> (Result<QueryPlan, String>, bool) {
    let (result, completion) = completion_done(backend, token);
    assert_eq!(completion.execution, ExplainExecution::NotStarted);
    assert!(!completion.session_closed);
    // Statements refused before taking the session report no observed state.
    assert!(completion.in_tx.is_some() || result.is_err());
    (result, completion.in_tx.unwrap_or(false))
}

fn completion_done(
    backend: &Backend,
    token: u64,
) -> (Result<QueryPlan, String>, ExplainCompletion) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match receive(backend, deadline) {
            Event::ExplainDone {
                console_id,
                request_id,
                completion,
                result,
            } => {
                assert_eq!(console_id, SOURCE);
                assert_eq!(request_id, token);
                return (result, completion);
            }
            Event::StatementDone { console_id, .. } | Event::RunFinished { console_id, .. }
                if console_id == SOURCE =>
            {
                panic!("Explain must not emit ordinary result-grid events");
            }
            _ => {}
        }
    }
}

fn explain(backend: &Backend, ds: &DataSource, sql: &str) -> (Result<QueryPlan, String>, bool) {
    let req = request(ds, sql);
    let token = req.request_id;
    backend.explain(req);
    explain_done(backend, token)
}

fn assert_tree(plan: &QueryPlan) {
    assert!(!plan.raw.is_empty());
    assert!(!plan.nodes.is_empty());
    let diagram = plan.diagram.as_ref().expect("native plans have a diagram");
    assert_eq!(diagram.len(), plan.nodes.len());
    for (index, node) in plan.nodes.iter().enumerate() {
        assert!(node.subtree_end > index && node.subtree_end <= plan.nodes.len());
        assert!(!node.operation.is_empty());
        let center = diagram.node_rect(index).unwrap().center();
        assert_eq!(diagram.hit_test(center), Some(index));
        assert!(
            plan.nodes[index + 1..node.subtree_end]
                .iter()
                .all(|child| child.depth > node.depth)
        );
        assert!(
            plan.raw_lines
                .iter()
                .all(|line| plan.raw.get(line.clone()).is_some())
        );
    }
}

fn session_id(backend: &Backend, ds: &DataSource) -> Option<Value> {
    let sql = match ds.driver {
        DriverKind::Postgres => "SELECT pg_backend_pid()",
        DriverKind::MySql => "SELECT CONNECTION_ID()",
        DriverKind::Sqlite => return None,
    };
    Some(query(backend, SOURCE, ds, sql).rows[0][0].clone())
}

fn exercise_explain(ds: DataSource) {
    let backend = Backend::new(egui::Context::default());
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let name = format!("plan_{} Ż", &suffix[..12]);
    let orders_name = format!("plan_orders_{}", &suffix[..12]);
    let schema = match ds.driver {
        DriverKind::Postgres => "public",
        DriverKind::MySql => "dbeast",
        DriverKind::Sqlite => "main",
    };
    let table = sqlgen::qualified(ds.driver, Some(schema), &name);
    let orders = sqlgen::qualified(ds.driver, Some(schema), &orders_name);
    let index = sqlgen::ident(ds.driver, &format!("plan_idx_{}", &suffix[..12]));
    let engine = if ds.driver == DriverKind::MySql {
        " ENGINE=InnoDB"
    } else {
        ""
    };
    query(
        &backend,
        OBSERVER,
        &ds,
        &format!("CREATE TABLE {table}(id INTEGER PRIMARY KEY,name VARCHAR(80)){engine}"),
    );
    query(
        &backend,
        OBSERVER,
        &ds,
        &format!(
            "CREATE TABLE {orders}(id INTEGER PRIMARY KEY,customer_id INTEGER,amount INTEGER){engine}"
        ),
    );
    // SQLite's CREATE INDEX table name cannot have a schema qualifier.
    let index_table = if ds.driver == DriverKind::Sqlite {
        sqlgen::ident(ds.driver, &orders_name)
    } else {
        orders.clone()
    };
    query(
        &backend,
        OBSERVER,
        &ds,
        &format!("CREATE INDEX {index} ON {index_table}(customer_id)"),
    );
    query(
        &backend,
        OBSERVER,
        &ds,
        &format!("INSERT INTO {table} VALUES(1,'before'),(2,'other')"),
    );
    query(
        &backend,
        OBSERVER,
        &ds,
        &format!("INSERT INTO {orders} VALUES(1,1,10),(2,1,20),(3,2,30)"),
    );
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let original = query(
            &backend,
            SOURCE,
            &ds,
            &format!("SELECT * FROM {table} ORDER BY id"),
        );
        let epoch = original.source.as_ref().unwrap().session_epoch;
        let pid = session_id(&backend, &ds);
        let select = format!(
            "SELECT o.id,c.name,'a;b Żółw' AS marker FROM {orders} o JOIN {table} c ON c.id=o.customer_id WHERE o.customer_id=1 ORDER BY o.id"
        );
        let (plan, in_tx) = explain(&backend, &ds, &select);
        let plan = plan.unwrap();
        assert!(!in_tx);
        assert_eq!(plan.sql, select);
        assert_tree(&plan);
        assert_eq!(plan.context, original.context);
        if ds.driver == DriverKind::Sqlite {
            assert!(
                plan.nodes.iter().all(|node| node.total_cost.is_empty()
                    && node.startup_cost.is_empty()
                    && node.rows.is_empty()),
                "SQLite does not report estimated metrics"
            );
        }
        assert_eq!(session_id(&backend, &ds), pid);
        for sql in [
            format!(
                "WITH chosen AS (SELECT id FROM {orders} WHERE customer_id=1) SELECT * FROM chosen"
            ),
            format!("INSERT INTO {table}(id,name) VALUES(99,'must not insert')"),
            format!("UPDATE {table} SET name='must not update' WHERE id=1"),
            format!("DELETE FROM {table} WHERE id=2"),
        ] {
            let (plan, in_tx) = explain(&backend, &ds, &sql);
            assert!(!in_tx);
            assert!(!plan.unwrap().raw.is_empty());
            assert_eq!(
                query(
                    &backend,
                    SOURCE,
                    &ds,
                    &format!("SELECT * FROM {table} ORDER BY id")
                )
                .rows,
                original.rows,
                "Explain must not execute DML"
            );
        }
        if ds.driver != DriverKind::MySql {
            assert!(explain(&backend, &ds, "VALUES(1),(2)").0.is_ok());
        }
        for sql in [
            format!("SELECT 1; DELETE FROM {table}"),
            format!("EXPLAIN ANALYZE DELETE FROM {table}"),
            "BEGIN".into(),
            format!("DROP TABLE {table}"),
            "".into(),
        ] {
            assert!(
                explain(&backend, &ds, &sql).0.is_err(),
                "must refuse {sql:?}"
            );
        }
        if ds.driver == DriverKind::MySql {
            // MySQL's -- comment needs whitespace; our script splitter historically
            // accepts --2. Prepared execution must independently reject the batch.
            assert!(
                explain(&backend, &ds, &format!("SELECT 1--2; DELETE FROM {table}"))
                    .0
                    .is_err()
            );
        }
        assert_eq!(
            query(
                &backend,
                SOURCE,
                &ds,
                &format!("SELECT * FROM {table} ORDER BY id")
            )
            .rows,
            original.rows
        );
        assert_eq!(session_id(&backend, &ds), pid);

        query(&backend, SOURCE, &ds, "BEGIN");
        query(
            &backend,
            SOURCE,
            &ds,
            &format!("UPDATE {table} SET name='earlier uncommitted' WHERE id=1"),
        );
        for sql in [
            &select,
            "SELECT missing_column_for_plan FROM definitely_missing_plan_table",
        ] {
            let (result, in_tx) = explain(&backend, &ds, sql);
            assert!(in_tx);
            if sql == select {
                assert!(result.is_ok());
            } else {
                assert!(result.is_err());
            }
            assert_eq!(
                query(
                    &backend,
                    SOURCE,
                    &ds,
                    &format!("SELECT name FROM {table} WHERE id=1")
                )
                .rows[0][0],
                Value::Text("earlier uncommitted".into())
            );
            assert_eq!(
                query(
                    &backend,
                    OBSERVER,
                    &ds,
                    &format!("SELECT name FROM {table} WHERE id=1")
                )
                .rows[0][0],
                Value::Text("before".into()),
                "Explain cannot commit the user's transaction"
            );
            assert_eq!(
                query(&backend, SOURCE, &ds, "SELECT 1").rows[0][0],
                Value::Int(1)
            );
        }
        query(&backend, SOURCE, &ds, "ROLLBACK");
        assert_eq!(
            query(
                &backend,
                SOURCE,
                &ds,
                &format!("SELECT * FROM {table} ORDER BY id")
            )
            .rows,
            original.rows
        );
        assert_eq!(
            query(&backend, SOURCE, &ds, &format!("SELECT * FROM {table}"))
                .source
                .unwrap()
                .session_epoch,
            epoch
        );

        // The connection-only temporary relation proves Explain does not open a
        // new session, even when it shares a name with a permanent table.
        let temp = sqlgen::ident(ds.driver, &name);
        let temp_keyword = if ds.driver == DriverKind::MySql {
            "TEMPORARY"
        } else {
            "TEMP"
        };
        query(
            &backend,
            SOURCE,
            &ds,
            &format!("CREATE {temp_keyword} TABLE {temp}(temp_only INTEGER)"),
        );
        query(
            &backend,
            SOURCE,
            &ds,
            &format!("INSERT INTO {temp} VALUES(41)"),
        );
        assert!(
            explain(&backend, &ds, &format!("SELECT temp_only FROM {temp}"))
                .0
                .is_ok()
        );
        assert_eq!(
            query(
                &backend,
                SOURCE,
                &ds,
                &format!("SELECT temp_only FROM {temp}")
            )
            .rows,
            [vec![Value::Int(41)]]
        );
        if ds.driver == DriverKind::MySql {
            query(&backend, SOURCE, &ds, "SET end_markers_in_json=ON");
            let (plan, _) = explain(
                &backend,
                &ds,
                &format!("SELECT id FROM {orders} WHERE customer_id=1"),
            );
            let plan = plan.unwrap();
            assert!(!plan.raw.is_empty());
            assert!(
                plan.warning.is_some(),
                "nonstandard MySQL JSON remains available as Raw"
            );
            assert_eq!(
                query(
                    &backend,
                    SOURCE,
                    &ds,
                    "SELECT @@session.end_markers_in_json"
                )
                .rows[0][0],
                Value::Int(1)
            );
            query(&backend, SOURCE, &ds, "SET end_markers_in_json=OFF");
        }
        if ds.driver == DriverKind::Postgres {
            query(&backend, SOURCE, &ds, "SET search_path TO pg_temp");
            let actual = query(
                &backend,
                SOURCE,
                &ds,
                &format!("SELECT temp_only FROM {temp}"),
            );
            let (plan, _) = explain(&backend, &ds, &format!("SELECT temp_only FROM {temp}"));
            assert_eq!(
                plan.unwrap().context,
                actual.context,
                "Explain retains a manual session search_path"
            );
            assert_eq!(
                query(&backend, SOURCE, &ds, "SHOW search_path").rows[0][0],
                Value::Text("pg_temp".into())
            );
        } else if ds.driver == DriverKind::MySql {
            query(&backend, SOURCE, &ds, "USE shop");
            let actual = query(&backend, SOURCE, &ds, &format!("SELECT id FROM {orders}"));
            let (plan, _) = explain(&backend, &ds, &format!("SELECT id FROM {orders}"));
            assert_eq!(
                plan.unwrap().context,
                actual.context,
                "Explain retains a manual session database"
            );
            assert_eq!(
                query(&backend, SOURCE, &ds, "SELECT DATABASE()").rows[0][0],
                Value::Text("shop".into())
            );
        }
    }));
    backend.close_session(&ds.id, SOURCE);
    // Use the independent console, unaffected by temporary shadows or a failed TX.
    for target in [&orders, &table] {
        query(
            &backend,
            OBSERVER,
            &ds,
            &format!("DROP TABLE IF EXISTS {target}"),
        );
    }
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn sqlite_explain_preserves_the_console_and_does_not_execute_dml() {
    let local = LocalDatabase::new();
    exercise_explain(local.source.clone());
}

#[test]
fn sqlite_explain_supports_attached_namespaces_and_read_only_sources() {
    let local = LocalDatabase::new();
    let backend = Backend::new(egui::Context::default());
    let attached = local.directory.join("attached.sqlite");
    query(
        &backend,
        SOURCE,
        &local.source,
        "CREATE TABLE documents(id INTEGER PRIMARY KEY)",
    );
    query(
        &backend,
        SOURCE,
        &local.source,
        &format!(
            "ATTACH '{}' AS extra",
            attached.to_string_lossy().replace('\'', "''")
        ),
    );
    query(
        &backend,
        SOURCE,
        &local.source,
        "CREATE TABLE extra.documents(payload TEXT)",
    );
    query(
        &backend,
        SOURCE,
        &local.source,
        "INSERT INTO extra.documents VALUES('attached Żółw')",
    );
    let namespaces = query(&backend, SOURCE, &local.source, "PRAGMA database_list");
    let sql = "SELECT payload FROM extra.documents";
    let (plan, in_tx) = explain(&backend, &local.source, sql);
    assert!(!in_tx);
    assert_tree(&plan.unwrap());
    assert_eq!(
        query(&backend, SOURCE, &local.source, "PRAGMA database_list").rows,
        namespaces.rows
    );
    assert_eq!(
        query(&backend, SOURCE, &local.source, sql).rows[0][0],
        Value::Text("attached Żółw".into())
    );
    backend.close_session(&local.source.id, SOURCE);
    let mut read_only = local.source.clone();
    read_only.read_only = true;
    let (plan, in_tx) = explain(&backend, &read_only, "SELECT id FROM documents");
    assert!(!in_tx);
    assert_tree(&plan.unwrap());
}

#[test]
fn postgres_explain_preserves_the_console_and_does_not_execute_dml() {
    if let Some(ds) = server(DriverKind::Postgres) {
        exercise_explain(ds);
    }
}

#[test]
fn mysql_explain_preserves_the_console_and_does_not_execute_dml() {
    if let Some(ds) = server(DriverKind::MySql) {
        exercise_explain(ds);
    }
}

#[test]
fn postgres_cancel_explain_keeps_prior_transaction_work_and_connection() {
    let Some(ds) = server(DriverKind::Postgres) else {
        return;
    };
    let backend = Backend::new(egui::Context::default());
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let locked = sqlgen::qualified(
        ds.driver,
        Some("public"),
        &format!("explain_lock_{}", &suffix[..12]),
    );
    let marker = sqlgen::qualified(
        ds.driver,
        Some("public"),
        &format!("explain_marker_{}", &suffix[..12]),
    );
    let mut observer = runtime.block_on(Connection::open(&ds)).unwrap();
    for sql in [
        format!("CREATE TABLE {locked}(id INTEGER)"),
        format!("CREATE TABLE {marker}(id INTEGER PRIMARY KEY,note TEXT)"),
        format!("INSERT INTO {marker} VALUES(1,'before')"),
    ] {
        runtime.block_on(observer.execute(&sql, Page::ALL)).unwrap();
    }
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let pid = query(&backend, SOURCE, &ds, "SELECT pg_backend_pid()").rows[0][0].to_string();
        query(&backend, SOURCE, &ds, "BEGIN");
        query(
            &backend,
            SOURCE,
            &ds,
            &format!("UPDATE {marker} SET note='pending' WHERE id=1"),
        );
        // A server-side bound prevents a broken cancellation path hanging cleanup.
        query(&backend, SOURCE, &ds, "SET LOCAL lock_timeout='8s'");
        runtime
            .block_on(observer.execute("BEGIN", Page::ALL))
            .unwrap();
        runtime
            .block_on(observer.execute(
                &format!("LOCK TABLE {locked} IN ACCESS EXCLUSIVE MODE"),
                Page::ALL,
            ))
            .unwrap();
        let req = request(&ds, &format!("SELECT * FROM {locked}"));
        let token = req.request_id;
        backend.explain(req);
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            runtime
                .block_on(observer.execute("SELECT pg_stat_clear_snapshot()", Page::ALL))
                .unwrap();
            let state=runtime.block_on(observer.execute(&format!("SELECT count(*) FROM pg_stat_activity WHERE pid={pid} AND wait_event_type='Lock' AND query ILIKE '%EXPLAIN%'"),Page::ALL)).unwrap();
            if state.rows[0][0] == Value::Int(1) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Explain must reach the deterministic lock wait before cancellation"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        backend.cancel(SOURCE);
        let (result, in_tx) = explain_done(&backend, token);
        let error = result.unwrap_err();
        assert!(
            error.contains("57014") || error.to_ascii_lowercase().contains("cancel"),
            "query cancellation, not a lock timeout: {error}"
        );
        assert!(in_tx);
        runtime
            .block_on(observer.execute("ROLLBACK", Page::ALL))
            .unwrap();
        assert_eq!(
            query(&backend, SOURCE, &ds, "SELECT pg_backend_pid()").rows[0][0].to_string(),
            pid
        );
        assert_eq!(
            query(&backend, SOURCE, &ds, &format!("SELECT note FROM {marker}")).rows[0][0],
            Value::Text("pending".into())
        );
        assert_eq!(
            runtime
                .block_on(observer.execute(&format!("SELECT note FROM {marker}"), Page::ALL))
                .unwrap()
                .rows[0][0],
            Value::Text("before".into())
        );
        assert!(
            explain(&backend, &ds, &format!("SELECT * FROM {locked}"))
                .0
                .is_ok(),
            "a removed cancellation handle cannot cancel the next request"
        );
        query(&backend, SOURCE, &ds, "ROLLBACK");
    }));
    // Both rollback and source disconnect happen even when a wait assertion fails.
    let _ = runtime.block_on(observer.execute("ROLLBACK", Page::ALL));
    backend.close_session(&ds.id, SOURCE);
    for table in [&locked, &marker] {
        runtime
            .block_on(observer.execute(&format!("DROP TABLE IF EXISTS {table}"), Page::ALL))
            .unwrap();
    }
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

fn analyse(
    backend: &Backend,
    ds: &DataSource,
    sql: &str,
    manual_tx: bool,
) -> (Result<QueryPlan, String>, ExplainCompletion) {
    let req = analyse_request(ds, sql, ExplainMode::Analyse, manual_tx);
    let token = req.request_id;
    backend.explain(req);
    completion_done(backend, token)
}

#[test]
fn postgres_explain_analyse_executes_once_and_respects_transactions() {
    let Some(ds) = server(DriverKind::Postgres) else {
        return;
    };
    let backend = Backend::new(egui::Context::default());
    let suffix = &uuid::Uuid::new_v4().simple().to_string()[..12];
    let counter = format!("public.analyse_counter_{suffix}");
    let bump = format!("public.analyse_bump_{suffix}");
    let read = |backend: &Backend| {
        query(backend, OBSERVER, &ds, &format!("SELECT n FROM {counter}")).rows[0][0].clone()
    };
    query(
        &backend,
        OBSERVER,
        &ds,
        &format!("CREATE TABLE {counter}(id INTEGER PRIMARY KEY, n INTEGER)"),
    );
    query(
        &backend,
        OBSERVER,
        &ds,
        &format!("INSERT INTO {counter} VALUES(1, 0)"),
    );
    query(
        &backend,
        OBSERVER,
        &ds,
        &format!(
            "CREATE FUNCTION {bump}() RETURNS integer LANGUAGE sql AS 'UPDATE {counter} SET n = n + 100 RETURNING n'"
        ),
    );
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let pid = session_id(&backend, &ds);
        // Ordinary Explain Plan still never executes the statement.
        let (plan, in_tx) = explain(&backend, &ds, &format!("UPDATE {counter} SET n = n + 1"));
        assert!(plan.unwrap().analysis.is_none());
        assert!(!in_tx);
        assert_eq!(read(&backend), Value::Int(0));

        // Auto: executed exactly once and committed by the server.
        let (plan, completion) = analyse(
            &backend,
            &ds,
            &format!("UPDATE {counter} SET n = n + 1"),
            false,
        );
        let plan = plan.unwrap();
        assert_eq!(completion.execution, ExplainExecution::Succeeded);
        assert_eq!(completion.in_tx, Some(false));
        assert!(!completion.session_closed);
        assert_eq!(completion.ds_id, ds.id);
        assert!(completion.post_context.is_some());
        assert_eq!(plan.mode, ExplainMode::Analyse);
        assert_tree(&plan);
        let analysis = plan.analysis.as_ref().unwrap();
        assert_eq!(analysis.nodes.len(), plan.nodes.len());
        assert!(
            analysis
                .nodes
                .iter()
                .any(|node| node.total_ms.is_some() && node.loops.is_some())
        );
        assert!(analysis.summary.execution_time_ms.is_some());
        assert!(analysis.summary.planning_time_ms.is_some());
        assert_eq!(read(&backend), Value::Int(1));
        assert_eq!(session_id(&backend, &ds), pid);

        // Manual: the change stays in the opened transaction until Rollback.
        let (plan, completion) = analyse(
            &backend,
            &ds,
            &format!("UPDATE {counter} SET n = n + 10"),
            true,
        );
        assert!(plan.is_ok());
        assert_eq!(completion.in_tx, Some(true));
        assert_eq!(read(&backend), Value::Int(1));
        query(&backend, SOURCE, &ds, "ROLLBACK");
        assert_eq!(read(&backend), Value::Int(1));
        let (_, completion) = analyse(
            &backend,
            &ds,
            &format!("UPDATE {counter} SET n = n + 10"),
            true,
        );
        assert_eq!(completion.in_tx, Some(true));
        query(&backend, SOURCE, &ds, "COMMIT");
        assert_eq!(read(&backend), Value::Int(11));

        // A server error rolls back only Analyse work, never earlier work.
        query(&backend, SOURCE, &ds, "BEGIN");
        query(
            &backend,
            SOURCE,
            &ds,
            &format!("UPDATE {counter} SET n = 500"),
        );
        let (result, completion) = analyse(
            &backend,
            &ds,
            &format!("UPDATE {counter} SET n = n / 0"),
            false,
        );
        assert!(result.unwrap_err().contains("division by zero"));
        assert_eq!(completion.execution, ExplainExecution::Failed);
        assert_eq!(completion.in_tx, Some(true));
        assert!(!completion.session_closed);
        assert_eq!(
            query(&backend, SOURCE, &ds, &format!("SELECT n FROM {counter}")).rows[0][0],
            Value::Int(500)
        );
        query(&backend, SOURCE, &ds, "ROLLBACK");
        assert_eq!(read(&backend), Value::Int(11));

        // CTEs and functions with side effects run on the same session.
        let (_, completion) = analyse(
            &backend,
            &ds,
            &format!(
                "WITH changed AS (UPDATE {counter} SET n = n + 1 RETURNING n) SELECT * FROM changed"
            ),
            false,
        );
        assert_eq!(completion.execution, ExplainExecution::Succeeded);
        assert_eq!(read(&backend), Value::Int(12));
        assert!(
            analyse(&backend, &ds, &format!("SELECT {bump}()"), false)
                .0
                .is_ok()
        );
        assert_eq!(read(&backend), Value::Int(112));
        query(
            &backend,
            SOURCE,
            &ds,
            "CREATE TEMP TABLE analyse_temp(v INTEGER)",
        );
        assert!(
            analyse(&backend, &ds, "INSERT INTO analyse_temp VALUES(7)", false)
                .0
                .is_ok()
        );
        assert_eq!(
            query(&backend, SOURCE, &ds, "SELECT v FROM analyse_temp").rows,
            [vec![Value::Int(7)]]
        );
        assert_eq!(session_id(&backend, &ds), pid);

        // The UI context comes from a fresh read after execution, not the plan snapshot.
        let (plan, completion) = analyse(
            &backend,
            &ds,
            "SELECT set_config('search_path', 'pg_catalog', false)",
            false,
        );
        let before = plan.unwrap().context.unwrap().search_path.unwrap();
        let after = completion.post_context.unwrap().search_path.unwrap();
        assert_ne!(before, after);
        assert_eq!(after, "pg_catalog");
        query(&backend, SOURCE, &ds, "SET search_path TO DEFAULT");

        // Stop during execution: confirmed cancel keeps the session and earlier work.
        query(&backend, SOURCE, &ds, "BEGIN");
        query(
            &backend,
            SOURCE,
            &ds,
            &format!("UPDATE {counter} SET n = 900"),
        );
        let req = analyse_request(&ds, "SELECT pg_sleep(30)", ExplainMode::Analyse, false);
        let token = req.request_id;
        backend.explain(req);
        std::thread::sleep(Duration::from_millis(400));
        let stopped = Instant::now();
        backend.cancel(SOURCE);
        let (result, completion) = completion_done(&backend, token);
        assert!(stopped.elapsed() < Duration::from_secs(5));
        let error = result.unwrap_err();
        assert!(
            error.contains("57014") || error.to_ascii_lowercase().contains("cancel"),
            "{error}"
        );
        assert_eq!(completion.execution, ExplainExecution::Failed);
        assert!(!completion.session_closed);
        assert_eq!(completion.in_tx, Some(true));
        assert_eq!(
            query(&backend, SOURCE, &ds, &format!("SELECT n FROM {counter}")).rows[0][0],
            Value::Int(900)
        );
        query(&backend, SOURCE, &ds, "ROLLBACK");
        // No late cancellation reaches the next statement.
        assert_eq!(
            query(&backend, SOURCE, &ds, "SELECT pg_sleep(0.3), 1").rows[0][1],
            Value::Int(1)
        );
        assert_eq!(session_id(&backend, &ds), pid);

        // A read-only source refuses changes on the server.
        let mut read_only = ds.clone();
        read_only.read_only = true;
        backend.close_session(&ds.id, SOURCE);
        let (result, completion) = analyse(
            &backend,
            &read_only,
            &format!("UPDATE {counter} SET n = 0"),
            false,
        );
        assert!(result.is_err());
        assert_eq!(completion.execution, ExplainExecution::Failed);
        assert_eq!(read(&backend), Value::Int(112));
    }));
    backend.close_session(&ds.id, SOURCE);
    query(
        &backend,
        OBSERVER,
        &ds,
        &format!("DROP FUNCTION IF EXISTS {bump}()"),
    );
    query(
        &backend,
        OBSERVER,
        &ds,
        &format!("DROP TABLE IF EXISTS {counter}"),
    );
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn mysql_explain_analyse_executes_iterators_and_respects_transactions() {
    let Some(ds) = server(DriverKind::MySql) else {
        return;
    };
    let backend = Backend::new(egui::Context::default());
    let suffix = &uuid::Uuid::new_v4().simple().to_string()[..12];
    let counter = format!("analyse_counter_{suffix}");
    let bump = format!("analyse_bump_{suffix}");
    // The local fixture's ordinary user lacks CREATE FUNCTION with binary
    // logging. Only fixture setup/cleanup uses root; execution uses dbeast.
    let mut admin = ds.clone();
    admin.id = uuid::Uuid::new_v4().to_string();
    admin.user = "root".into();
    let update = |value: &str| {
        format!("UPDATE {counter} t JOIN (SELECT 1 AS id) u ON t.id=u.id SET t.n={value}")
    };
    let read = |backend: &Backend, session| {
        query(
            backend,
            session,
            &ds,
            &format!("SELECT n FROM {counter} WHERE id=1"),
        )
        .rows[0][0]
            .clone()
    };
    query(
        &backend,
        OBSERVER,
        &ds,
        &format!(
            "CREATE TABLE {counter}(id INTEGER PRIMARY KEY, n INTEGER NOT NULL) ENGINE=InnoDB"
        ),
    );
    query(
        &backend,
        OBSERVER,
        &ds,
        &format!("INSERT INTO {counter} VALUES(1,0),(2,0)"),
    );
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        query(
            &backend,
            OBSERVER,
            &admin,
            &format!(
                "CREATE FUNCTION {bump}(seed INT, fail BOOLEAN) RETURNS INT DETERMINISTIC MODIFIES SQL DATA SQL SECURITY INVOKER BEGIN UPDATE {counter} SET n=n+1 WHERE id=1; IF fail THEN SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT='fixture failure'; END IF; RETURN 1; END"
            ),
        );
        let id = query(&backend, SOURCE, &ds, "SELECT CONNECTION_ID()").rows[0][0].clone();
        query(
            &backend,
            SOURCE,
            &ds,
            "CREATE TEMPORARY TABLE analyse_temp(n INT)",
        );
        query(&backend, SOURCE, &ds, "INSERT INTO analyse_temp VALUES(7)");
        let select = format!("SELECT * FROM analyse_temp WHERE {bump}(n,FALSE)>0");
        assert!(explain(&backend, &ds, &update("t.n+1")).0.is_ok());
        assert_eq!(read(&backend, OBSERVER), Value::Int(0));
        // MySQL 8.4 instruments DML iterators but omits the write stage.
        // Never emulate it by issuing the user's DML a second time.
        let (plan, completion) = analyse(&backend, &ds, &update("t.n+1"), false);
        let plan = plan.unwrap();
        assert_eq!(completion.execution, ExplainExecution::Succeeded);
        assert_eq!(completion.in_tx, Some(false));
        assert!(!completion.session_closed);
        assert_tree(&plan);
        assert!(
            plan.analysis
                .as_ref()
                .unwrap()
                .nodes
                .iter()
                .any(|n| n.total_ms.is_some())
        );
        assert!(
            plan.analysis
                .as_ref()
                .unwrap()
                .summary
                .execution_time_ms
                .is_none()
        );
        assert_eq!(read(&backend, OBSERVER), Value::Int(0));
        // A predicate function does execute once and can change persistent data.
        assert!(analyse(&backend, &ds, &select, false).0.is_ok());
        assert_eq!(read(&backend, OBSERVER), Value::Int(1));
        let (plan, completion) = analyse(&backend, &ds, &select, true);
        assert!(plan.is_ok(), "{plan:?}");
        assert_eq!(completion.in_tx, Some(true));
        assert_eq!(read(&backend, SOURCE), Value::Int(2));
        assert_eq!(read(&backend, OBSERVER), Value::Int(1));
        query(&backend, SOURCE, &ds, "ROLLBACK");
        assert_eq!(read(&backend, OBSERVER), Value::Int(1));
        assert!(analyse(&backend, &ds, &select, true).0.is_ok());
        query(&backend, SOURCE, &ds, "COMMIT");
        assert_eq!(read(&backend, OBSERVER), Value::Int(2));
        query(&backend, SOURCE, &ds, "BEGIN");
        query(&backend, SOURCE, &ds, &update("500"));
        let (result, completion) = analyse(&backend, &ds, &select.replace("FALSE", "TRUE"), false);
        assert!(result.unwrap_err().contains("fixture failure"));
        assert_eq!(completion.execution, ExplainExecution::Failed);
        assert_eq!(completion.in_tx, Some(true));
        assert!(!completion.session_closed);
        assert_eq!(read(&backend, SOURCE), Value::Int(500));
        let (result, completion) = analyse(
            &backend,
            &ds,
            &format!("INSERT INTO {counter} VALUES(3,0)"),
            false,
        );
        assert!(result.is_err());
        assert_eq!(completion.execution, ExplainExecution::NotStarted);
        assert_eq!(
            query(
                &backend,
                SOURCE,
                &ds,
                &format!("SELECT count(*) FROM {counter}")
            )
            .rows[0][0],
            Value::Int(2)
        );
        let (result, _) = analyse(
            &backend,
            &ds,
            &format!("UPDATE {counter} SET n=1000"),
            false,
        );
        assert!(
            result.is_ok(),
            "MySQL 8.4 accepts this single-table UPDATE: {result:?}"
        );
        assert_eq!(read(&backend, SOURCE), Value::Int(500));
        query(&backend, SOURCE, &ds, "ROLLBACK");
        assert_eq!(
            query(&backend, SOURCE, &ds, "SELECT CONNECTION_ID()").rows[0][0],
            id
        );
        for sql in [
            "TABLE analyse_temp",
            "WITH c AS (SELECT * FROM analyse_temp) SELECT * FROM c",
            "SELECT * FROM analyse_temp WHERE n=8 AND SLEEP(1)=0",
        ] {
            let (plan, completion) = analyse(&backend, &ds, sql, false);
            let plan = plan.unwrap();
            assert_eq!(plan.warning, None, "{}", plan.raw);
            assert_eq!(completion.execution, ExplainExecution::Succeeded);
            assert_eq!(plan.context, completion.post_context);
        }
        assert!(
            analyse(
                &backend,
                &ds,
                &format!("DELETE t FROM {counter} t JOIN (SELECT 2 AS id) u ON t.id=u.id"),
                false
            )
            .0
            .is_ok()
        );
        assert_eq!(
            query(
                &backend,
                SOURCE,
                &ds,
                &format!("SELECT count(*) FROM {counter}")
            )
            .rows[0][0],
            Value::Int(2)
        );
        // MySQL may return a partial TREE with protocol success after KILL.
        // The client cannot call that a complete execution or reuse the session.
        query(&backend, SOURCE, &ds, "BEGIN");
        query(&backend, SOURCE, &ds, &update("900"));
        let req = analyse_request(
            &ds,
            &format!("SELECT * FROM {counter} WHERE SLEEP(30)=0"),
            ExplainMode::Analyse,
            false,
        );
        let request_id = req.request_id;
        backend.explain(req);
        std::thread::sleep(Duration::from_millis(400));
        let stopped = Instant::now();
        backend.cancel(SOURCE);
        let (result, completion) = completion_done(&backend, request_id);
        assert!(stopped.elapsed() < Duration::from_secs(5));
        assert!(result.unwrap_err().contains("outcome is unknown"));
        assert_eq!(completion.execution, ExplainExecution::Unknown);
        assert!(completion.session_closed);
        assert_eq!(completion.in_tx, Some(false));
        assert!(completion.post_context.is_none());
        let (result, completion) = analyse(&backend, &ds, "SELECT 1", false);
        assert!(result.unwrap_err().contains("Disconnect"));
        assert_eq!(completion.execution, ExplainExecution::NotStarted);
        backend.disconnect(&ds.id);
        // No retry or late KILL reaches the fresh session after explicit reset.
        assert_eq!(
            query(&backend, SOURCE, &ds, "SELECT SLEEP(0.1), 1").rows[0][1],
            Value::Int(1)
        );
        assert_eq!(read(&backend, OBSERVER), Value::Int(2));
        let mut read_only = ds.clone();
        read_only.read_only = true;
        backend.close_session(&ds.id, SOURCE);
        let select = format!("SELECT * FROM (SELECT 1 AS n) x WHERE {bump}(n,FALSE)>0");
        let (result, completion) = analyse(&backend, &read_only, &select, false);
        let error = result.unwrap_err();
        assert!(error.contains("READ ONLY"), "{error}");
        assert!(matches!(
            completion.execution,
            ExplainExecution::Failed | ExplainExecution::NotStarted
        ));
        assert_eq!(read(&backend, OBSERVER), Value::Int(2));
    }));
    backend.close_session(&ds.id, SOURCE);
    query(
        &backend,
        OBSERVER,
        &admin,
        &format!("DROP FUNCTION IF EXISTS {bump}"),
    );
    query(
        &backend,
        OBSERVER,
        &ds,
        &format!("DROP TABLE IF EXISTS {counter}"),
    );
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn analyse_is_refused_before_touching_sqlite_sessions() {
    let local = LocalDatabase::new();
    let backend = Backend::new(egui::Context::default());
    query(&backend, SOURCE, &local.source, "CREATE TABLE t(n INTEGER)");
    let (result, completion) = analyse(&backend, &local.source, "INSERT INTO t VALUES(1)", false);
    assert!(result.unwrap_err().contains("PostgreSQL"));
    assert_eq!(completion.execution, ExplainExecution::NotStarted);
    assert_eq!(completion.in_tx, None);
    assert!(
        query(&backend, SOURCE, &local.source, "SELECT * FROM t")
            .rows
            .is_empty()
    );
}

#[test]
#[ignore = "manual release measurement against local MySQL fixture"]
fn benchmark_mysql_analyse_execution() {
    let Some(ds) = server(DriverKind::MySql) else {
        return;
    };
    let backend = Backend::new(egui::Context::default());
    query(
        &backend,
        SOURCE,
        &ds,
        "CREATE TEMPORARY TABLE analyse_perf(n INT)",
    );
    query(
        &backend,
        SOURCE,
        &ds,
        "INSERT INTO analyse_perf WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM seq WHERE n<1000) SELECT n FROM seq",
    );
    let sql = "SELECT SUM(n) FROM analyse_perf";
    for _ in 0..3 {
        query(&backend, SOURCE, &ds, sql);
        analyse(&backend, &ds, sql, false).0.unwrap();
    }
    let mut baseline = Vec::new();
    let mut measured = Vec::new();
    for _ in 0..11 {
        let start = Instant::now();
        query(&backend, SOURCE, &ds, sql);
        baseline.push(start.elapsed());
        let start = Instant::now();
        let (plan, completion) = analyse(&backend, &ds, sql, false);
        assert_eq!(completion.execution, ExplainExecution::Succeeded);
        assert!(plan.unwrap().warning.is_none());
        measured.push(start.elapsed());
    }
    baseline.sort();
    measured.sort();
    eprintln!(
        "MySQL local 1000-row SUM: baseline {:.3} ms; Analyse backend + parser + events {:.3} ms (median 11, warm session)",
        baseline[5].as_secs_f64() * 1000.0,
        measured[5].as_secs_f64() * 1000.0
    );
}

#[test]
fn mysql_analyse_stop_during_connection_setup_does_not_wait_for_handshake() {
    use std::net::{Shutdown, TcpListener};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut ds = DataSource::new(DriverKind::MySql);
    ds.host = "127.0.0.1".into();
    ds.port = listener.local_addr().unwrap().port();
    ds.user = "fixture".into();
    let backend = Backend::new(egui::Context::default());
    let req = analyse_request(&ds, "SELECT 1", ExplainMode::Analyse, false);
    let id = req.request_id;
    backend.explain(req);
    let (socket, _) = listener.accept().unwrap();
    let start = Instant::now();
    backend.cancel(SOURCE);
    let (result, completion) = completion_done(&backend, id);
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(result.unwrap_err().contains("not executed"));
    assert_eq!(completion.execution, ExplainExecution::NotStarted);
    assert!(completion.session_closed);
    let _ = socket.shutdown(Shutdown::Both);
}

#[test]
fn mysql_analyse_post_execution_blackhole_is_bounded_and_keeps_known_outcome() {
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener, TcpStream};
    let Some(mut ds) = server(DriverKind::MySql) else {
        return;
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    ds.port = listener.local_addr().unwrap().port();
    // Forward the native fixture protocol, then withhold the first context read
    // after COM_STMT_EXECUTE. No SQL is changed and no user config is used.
    let (blocked_tx, blocked_rx) = std::sync::mpsc::channel();
    let proxy = std::thread::spawn(move || {
        let (mut client, _) = listener.accept().unwrap();
        let mut upstream = TcpStream::connect("127.0.0.1:53306").unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        upstream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        let mut from_server = upstream.try_clone().unwrap();
        let mut to_client = client.try_clone().unwrap();
        let downstream = std::thread::spawn(move || {
            let _ = std::io::copy(&mut from_server, &mut to_client);
        });
        let mut executed = false;
        let mut blackhole = false;
        loop {
            let mut header = [0; 4];
            if client.read_exact(&mut header).is_err() {
                break;
            }
            let length =
                usize::from(header[0]) | usize::from(header[1]) << 8 | usize::from(header[2]) << 16;
            let mut payload = vec![0; length];
            if client.read_exact(&mut payload).is_err() {
                break;
            }
            if executed && payload.first() == Some(&0x03) {
                blackhole = true;
                let _ = blocked_tx.send(());
            }
            executed |= payload.first() == Some(&0x17) && header[3] == 0;
            if !blackhole
                && (upstream.write_all(&header).is_err() || upstream.write_all(&payload).is_err())
            {
                break;
            }
        }
        let _ = upstream.shutdown(Shutdown::Both);
        let _ = client.shutdown(Shutdown::Both);
        downstream.join().unwrap();
    });
    let backend = Backend::new(egui::Context::default());
    let req = analyse_request(&ds, "SELECT 1", ExplainMode::Analyse, false);
    let id = req.request_id;
    backend.explain(req);
    blocked_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let start = Instant::now();
    let (result, completion) = completion_done(&backend, id);
    assert!(start.elapsed() < Duration::from_secs(7));
    assert!(result.unwrap_err().contains("recovery limit"));
    assert_eq!(completion.execution, ExplainExecution::Succeeded);
    assert!(completion.session_closed);
    assert!(completion.post_context.is_none());
    drop(backend);
    proxy.join().unwrap();
}

#[test]
fn unbound_placeholder_count_is_rejected_before_execute_without_closing_session() {
    for driver in [DriverKind::Postgres, DriverKind::MySql] {
        let Some(ds) = server(driver) else { continue };
        let backend = Backend::new(egui::Context::default());
        let sql = if driver == DriverKind::Postgres { "SELECT $1::text" } else { "SELECT ?" };
        let request = analyse_request(&ds, sql, ExplainMode::Analyse, false);
        let token = request.request_id;
        backend.explain(request);
        let (result, completion) = completion_done(&backend, token);
        assert!(result.unwrap_err().contains("parameter count mismatch"));
        assert_eq!(completion.execution, ExplainExecution::NotStarted);
        assert!(!completion.session_closed);
        assert_eq!(query(&backend, SOURCE, &ds, "SELECT 42").rows[0][0], Value::Int(42));
    }
}
