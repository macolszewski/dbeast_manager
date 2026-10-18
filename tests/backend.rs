//! Transaction handling, verified grid edits and export through the backend,
//! against the Docker PostgreSQL.

use dbeast::backend::{Backend, Event, MutationRequest, ResultMetadataRequest, RunRequest};
use dbeast::export::{ExportOptions, TableName};
use dbeast::model::{
    DataSource, DriverKind, ExecutionContext, MutationFailure, MutationReport, Page, QueryResult,
    ResolvedContext, ResultEditMetadata, SchemaSelection, TableEditMetadata, Value,
};
use dbeast::sqlgen::CellEdit;
use std::collections::HashMap;
use std::time::{Duration, Instant};

fn pg() -> Option<DataSource> {
    if let Err(error) = std::net::TcpStream::connect("127.0.0.1:55432") {
        eprintln!("skipping PostgreSQL backend test: TCP probe failed: {error}");
        return None;
    }
    let mut ds = DataSource::new(DriverKind::Postgres);
    ds.port = 55432;
    ds.user = "dbeast".into();
    ds.password = "dbeast".into();
    ds.database = "dbeast".into();
    Some(ds)
}

type Statements = Vec<(String, Result<dbeast::model::QueryResult, String>)>;

fn run(backend: &Backend, req: RunRequest) -> (Statements, bool) {
    let console_id = req.console_id;
    backend.run(req);
    collect_run(backend, console_id)
}

/// Waits for the only outstanding run in this console.
fn collect_run(backend: &Backend, console_id: u64) -> (Statements, bool) {
    let mut done = Vec::new();
    loop {
        match backend
            .rx
            .recv_timeout(Duration::from_secs(10))
            .expect("backend event")
        {
            Event::StatementDone {
                console_id: c,
                sql,
                result,
                ..
            } if c == console_id => done.push((sql, result)),
            Event::RunFinished {
                console_id: c,
                in_tx,
            } if c == console_id => return (done, in_tx),
            _ => {}
        }
    }
}

fn req(
    console_id: u64,
    ds: &DataSource,
    statements: &[&str],
    manual_tx: bool,
) -> RunRequest {
    RunRequest {
        console_id,
        ds: ds.clone(),
        statements: statements.iter().map(|s| s.to_string()).collect(),
        page: Page::first(500),
        manual_tx,
        context: ExecutionContext::default(),
        expected_context: None,
    }
}

fn count(backend: &Backend, ds: &DataSource, where_: &str) -> Value {
    let sql = format!("select count(*) from shop.customers where {where_}");
    let (done, ..) = run(backend, req(99, ds, &[&sql], false));
    done[0].1.as_ref().unwrap().rows[0][0].clone()
}

#[test]
fn manual_transaction_rolls_back() {
    let Some(ds) = pg() else { return };
    let backend = Backend::new(egui::Context::default());
    let (done, in_tx) = run(
        &backend,
        req(
            1,
            &ds,
            &["insert into shop.customers (email) values ('tx@example.com')"], true,
        ),
    );
    assert_eq!(done[0].0, "BEGIN");
    assert!(in_tx, "manual mode leaves the transaction open");
    // Another session does not see the uncommitted row.
    assert_eq!(
        count(&backend, &ds, "email = 'tx@example.com'"),
        Value::Int(0)
    );
    backend.end_transaction(1, ds.clone(), false);
    // Tasks submitted separately may reach the session mutex out of order.
    // Wait for ROLLBACK before starting a new run, as the UI does.
    let (done, in_tx) = collect_run(&backend, 1);
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].0, "ROLLBACK");
    assert!(done[0].1.is_ok(), "ROLLBACK failed: {:?}", done[0].1);
    assert!(!in_tx);
    let (done, in_tx) = run(&backend, req(1, &ds, &["select 1"], false));
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].0, "select 1");
    assert!(done[0].1.is_ok(), "SELECT failed: {:?}", done[0].1);
    assert!(!in_tx);
    assert_eq!(
        count(&backend, &ds, "email = 'tx@example.com'"),
        Value::Int(0)
    );
}

#[test]
fn export_whole_result_to_file() {
    let Some(ds) = pg() else { return };
    let backend = Backend::new(egui::Context::default());
    let path = std::env::temp_dir().join(format!("dbeast_export_{}.csv", std::process::id()));
    backend.export(
        3,
        ds,
        "select id, email from shop.customers order by id".into(),
        ExportOptions::default(),
        TableName::new("customers"),
        path.clone(),
        None,
    );
    let summary = task_outcome(&backend, 3).unwrap();
    assert!(summary.starts_with("1200 rows exported"), "export is not limited to one page: {summary}");
    let csv = std::fs::read_to_string(&path).unwrap();
    assert!(csv.starts_with("id,email\n1,user1@example.com\n"));
    std::fs::remove_file(path).ok();
}

/// Waits for the outcome of a background transfer.
fn task_outcome(backend: &Backend, id: u64) -> Result<String, String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if let Event::Task { id: task, event: dbeast::backend::TaskEvent::Finished(result) } =
            backend.rx.recv_timeout(left).expect("transfer outcome")
            && task == id
        {
            return result;
        }
    }
}

fn named_context(name: &str) -> ExecutionContext {
    ExecutionContext {
        database: None,
        schema: SchemaSelection::Named(name.into()),
    }
}

fn contextual_request(
    id: u64,
    ds: &DataSource,
    context: &ExecutionContext,
    sql: &str,
) -> RunRequest {
    let mut request = req(id, ds, &[sql], false);
    request.context = context.clone();
    request
}

fn query_context(
    backend: &Backend,
    id: u64,
    ds: &DataSource,
    context: &ExecutionContext,
    sql: &str,
) -> QueryResult {
    let (mut done, _) = run(backend, contextual_request(id, ds, context, sql));
    assert_eq!(
        done.len(),
        1,
        "context changes must not become user SQL statements"
    );
    assert_eq!(done[0].0, sql);
    done.remove(0).1.unwrap()
}

fn change_context(
    backend: &Backend,
    id: u64,
    ds: &DataSource,
    requested: &ExecutionContext,
    revision: u64,
) -> Result<ResolvedContext, String> {
    backend.change_schema(id, ds.clone(), requested.clone(), revision);
    loop {
        match backend
            .rx
            .recv_timeout(Duration::from_secs(15))
            .expect("schema change event")
        {
            Event::SchemaChanged {
                console_id,
                ds_id,
                revision: received,
                requested: received_context,
                result,
            } if console_id == id => {
                assert_eq!(ds_id, ds.id);
                assert_eq!(received, revision);
                assert_eq!(received_context, *requested);
                return result;
            }
            Event::StatementDone { console_id, .. } if console_id == id => {
                panic!("schema selection must not add a user SQL result")
            }
            _ => {}
        }
    }
}

fn pg_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn load_edit_metadata(
    backend: &Backend,
    console_id: u64,
    ds: &DataSource,
    data: &QueryResult,
) -> Result<TableEditMetadata, String> {
    backend.load_result_metadata(ResultMetadataRequest {
        console_id,
        result_id: 8001,
        request_id: 8002,
        ds: ds.clone(),
        source_identity: ds.identity(),
        expected_context: data.context.clone().expect("captured namespace"),
        source: data.source.clone().expect("captured column origins"),
    });
    loop {
        if let Event::ResultMetadata {
            console_id: received,
            result_id,
            request_id,
            result,
        } = backend
            .rx
            .recv_timeout(Duration::from_secs(15))
            .expect("result metadata event")
            && received == console_id
        {
            assert_eq!(result_id, 8001);
            assert_eq!(request_id, 8002);
            return result;
        }
    }
}

fn editable_query(
    backend: &Backend,
    console_id: u64,
    ds: &DataSource,
    sql: &str,
) -> (QueryResult, ResultEditMetadata) {
    let data = query_context(backend, console_id, ds, &ExecutionContext::default(), sql);
    let table = load_edit_metadata(backend, console_id, ds, &data).unwrap();
    let metadata = dbeast::result_edit::resolve(Some(sql), ds.driver, &data, &table).unwrap();
    (data, metadata)
}

fn mutation_request(
    console_id: u64,
    ds: &DataSource,
    data: &QueryResult,
    metadata: &ResultEditMetadata,
    changes: &[((usize, usize), CellEdit)],
    manual_tx: bool,
) -> MutationRequest {
    let changes: HashMap<_, _> = changes.iter().cloned().collect();
    let updates = dbeast::sqlgen::bound_updates(ds.driver, metadata, data, &changes).unwrap();
    MutationRequest {
        console_id,
        result_id: 8001,
        request_id: 8003,
        ds: ds.clone(),
        source_identity: ds.identity(),
        expected_context: data.context.clone().unwrap(),
        session_epoch: data.source.as_ref().unwrap().session_epoch,
        metadata: metadata.clone(),
        mutations: updates,
        manual_tx,
    }
}

fn apply_updates(
    backend: &Backend,
    request: MutationRequest,
) -> (Result<MutationReport, MutationFailure>, bool) {
    let console_id = request.console_id;
    backend.apply_updates(request);
    loop {
        if let Event::UpdatesDone {
            console_id: received,
            result_id,
            request_id,
            in_tx,
            result,
        } = backend
            .rx
            .recv_timeout(Duration::from_secs(15))
            .expect("updates event")
            && received == console_id
        {
            assert_eq!(result_id, 8001);
            assert_eq!(request_id, 8003);
            return (result, in_tx.expect("the batch reached its session"));
        }
    }
}

fn exercise_verified_updates(ds: DataSource) {
    let backend = Backend::new(egui::Context::default());
    let table = format!("dbeast_edit_{}", uuid::Uuid::new_v4().simple());
    let schema = match ds.driver {
        DriverKind::Postgres => "public",
        DriverKind::MySql => "dbeast",
        DriverKind::Sqlite => "main",
    };
    let qualified = dbeast::sqlgen::qualified(ds.driver, Some(schema), &table);
    let default = ExecutionContext::default();
    let sql = format!(
        "SELECT id AS selected_id,tenant AS selected_tenant,name AS display_name,note,amount,amount * 2 AS doubled FROM {qualified} ORDER BY tenant,id"
    );
    let text = |value: &str| CellEdit::Text(value.into());
    let test = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let engine = if ds.driver == DriverKind::MySql {
            " ENGINE=InnoDB"
        } else {
            ""
        };
        query_context(
            &backend,
            79,
            &ds,
            &default,
            &format!(
                "CREATE TABLE {qualified} (tenant INTEGER NOT NULL,id INTEGER NOT NULL,name TEXT NOT NULL,note TEXT,amount INTEGER NOT NULL DEFAULT 0 CHECK(amount >= 0),PRIMARY KEY(tenant,id)){engine}"
            ),
        );
        query_context(
            &backend,
            79,
            &ds,
            &default,
            &format!(
                "INSERT INTO {qualified}(tenant,id,name,note,amount) VALUES (7,1,'Ada','first',10),(7,2,'Olek',NULL,20),(8,1,'Mira','sentinel',30)"
            ),
        );
        let (data, metadata) = editable_query(&backend, 71, &ds, &sql);
        assert_eq!(
            metadata.primary_key,
            [1, 0],
            "map physical composite key order to aliased output"
        );
        assert_eq!(metadata.physical_columns[2].as_deref(), Some("name"));
        assert!(!metadata.writable[5], "computed expression stays read-only");
        let payload = "O'Neil\\folder\nŻółw; -- SELECT 'x'";
        let request = mutation_request(
            71,
            &ds,
            &data,
            &metadata,
            &[((0, 2), text(payload)), ((0, 3), CellEdit::Null)],
            false,
        );
        assert!(
            request
                .mutations
                .iter()
                .all(|update| !update.sql.contains(payload)
                    && !update.sql.contains("selected_id")
                    && !update.sql.contains("display_name"))
        );
        let (result, in_tx) = apply_updates(&backend, request);
        assert_eq!(result.unwrap().changed_rows, 1);
        assert!(!in_tx);
        let stored = query_context(&backend, 79, &ds, &default, &sql);
        assert_eq!(stored.rows[0][2], Value::Text(payload.into()));
        assert_eq!(stored.rows[0][3], Value::Null);
        assert_eq!(
            stored.rows[2][2],
            Value::Text("Mira".into()),
            "other tenant with same id untouched"
        );
        let (data, metadata) = editable_query(&backend, 71, &ds, &sql);
        let defaults = HashMap::from([((0, 4), CellEdit::Default)]);
        if ds.driver == DriverKind::Sqlite {
            assert!(dbeast::sqlgen::bound_updates(ds.driver, &metadata, &data, &defaults).is_err());
        } else {
            let (result, _) = apply_updates(
                &backend,
                mutation_request(
                    71,
                    &ds,
                    &data,
                    &metadata,
                    &[((0, 4), CellEdit::Default)],
                    false,
                ),
            );
            assert_eq!(result.unwrap().changed_rows, 1);
            assert_eq!(
                query_context(&backend, 79, &ds, &default, &sql).rows[0][4],
                Value::Int(0)
            );
        }

        // Editing the projected key uses its original value in WHERE.
        let (data, metadata) = editable_query(&backend, 71, &ds, &sql);
        let (result, _) = apply_updates(
            &backend,
            mutation_request(71, &ds, &data, &metadata, &[((0, 0), text("4"))], false),
        );
        assert_eq!(result.unwrap().changed_rows, 1);
        let stored = query_context(&backend, 79, &ds, &default, &sql);
        assert_eq!(stored.rows[0][0], Value::Int(2));
        assert_eq!(stored.rows[1][0], Value::Int(4));
        assert_eq!(stored.rows[1][2], Value::Text(payload.into()));
        assert_eq!(stored.rows[2][0], Value::Int(1));

        let (data, metadata) = editable_query(&backend, 71, &ds, &sql);
        let bad = [((0, 2), text("must roll back")), ((1, 4), text("-1"))];
        let (result, in_tx) = apply_updates(
            &backend,
            mutation_request(71, &ds, &data, &metadata, &bad, false),
        );
        let failure = result.unwrap_err();
        assert_eq!(failure.source_row, Some(1));
        assert!(!failure.outcome_unknown);
        assert!(!in_tx, "failed Auto batch closes its own transaction");
        assert_eq!(
            query_context(&backend, 79, &ds, &default, &sql).rows,
            data.rows
        );

        // A savepoint must preserve earlier work in a user's open transaction.
        let (_, in_tx) = run(
            &backend,
            req(
                71,
                &ds,
                &[&format!(
                    "UPDATE {qualified} SET note='before batch' WHERE tenant=8 AND id=1"
                )], true,
        ),
        );
        assert!(in_tx);
        let (data, metadata) = editable_query(&backend, 71, &ds, &sql);
        let (result, in_tx) = apply_updates(
            &backend,
            mutation_request(71, &ds, &data, &metadata, &bad, true),
        );
        assert!(result.is_err());
        assert!(in_tx, "failed batch retains the user's transaction");
        let own = query_context(&backend, 71, &ds, &default, &sql);
        assert_eq!(
            own.rows, data.rows,
            "earlier transaction work survives, batch does not"
        );
        assert_eq!(own.rows[2][3], Value::Text("before batch".into()));
        assert_eq!(
            query_context(&backend, 79, &ds, &default, &sql).rows[2][3],
            Value::Text("sentinel".into())
        );
        let (result, in_tx) = apply_updates(
            &backend,
            mutation_request(
                71,
                &ds,
                &data,
                &metadata,
                &[((0, 3), text("manual staged"))],
                true,
            ),
        );
        assert_eq!(result.unwrap().changed_rows, 1);
        assert!(
            in_tx,
            "successful Manual Submit must not commit the user's transaction"
        );
        assert_eq!(
            query_context(&backend, 79, &ds, &default, &sql).rows[0][3],
            Value::Null
        );
        query_context(&backend, 71, &ds, &default, "ROLLBACK");
        assert_eq!(
            query_context(&backend, 79, &ds, &default, &sql).rows[2][3],
            Value::Text("sentinel".into())
        );

        if ds.driver != DriverKind::Sqlite {
            // The verified table remains protected for the user's whole Manual
            // transaction. The DDL session has a server-side bounded wait.
            let timeout_name = match ds.driver {
                DriverKind::Postgres => "SHOW lock_timeout",
                DriverKind::MySql => "SELECT @@SESSION.lock_wait_timeout",
                DriverKind::Sqlite => unreachable!(),
            };
            let previous_timeout =
                query_context(&backend, 79, &ds, &default, timeout_name).rows[0][0].clone();
            let set_timeout = match ds.driver {
                DriverKind::Postgres => "SET lock_timeout='150ms'",
                DriverKind::MySql => "SET SESSION lock_wait_timeout=1",
                DriverKind::Sqlite => unreachable!(),
            };
            query_context(&backend, 79, &ds, &default, set_timeout);
            let (data, metadata) = editable_query(&backend, 71, &ds, &sql);
            let (result, in_tx) = apply_updates(
                &backend,
                mutation_request(
                    71,
                    &ds,
                    &data,
                    &metadata,
                    &[((0, 3), text("held through commit"))],
                    true,
                ),
            );
            assert_eq!(result.unwrap().changed_rows, 1);
            assert!(in_tx);
            let alter = format!("ALTER TABLE {qualified} ADD COLUMN lock_probe INTEGER");
            let (attempt, _) = run(&backend, req(79, &ds, &[&alter], false));
            assert_eq!(attempt.len(), 1);
            let error = attempt[0]
                .1
                .as_ref()
                .expect_err("DDL must wait for the Manual transaction's table lock");
            match ds.driver {
                DriverKind::Postgres => assert!(
                    error.contains("55P03") || error.contains("lock timeout"),
                    "{error}"
                ),
                DriverKind::MySql => assert!(
                    error.contains("1205") || error.contains("Lock wait timeout"),
                    "{error}"
                ),
                DriverKind::Sqlite => unreachable!(),
            }
            query_context(&backend, 71, &ds, &default, "COMMIT");
            query_context(&backend, 79, &ds, &default, &alter);
            assert_eq!(
                query_context(&backend, 79, &ds, &default, &sql).rows[0][3],
                Value::Text("held through commit".into())
            );
            let reset_timeout = match ds.driver {
                DriverKind::Postgres => format!(
                    "SET lock_timeout={}",
                    dbeast::sqlgen::literal(ds.driver, &previous_timeout)
                ),
                DriverKind::MySql => format!(
                    "SET SESSION lock_wait_timeout={}",
                    dbeast::sqlgen::literal(ds.driver, &previous_timeout)
                ),
                DriverKind::Sqlite => unreachable!(),
            };
            query_context(&backend, 79, &ds, &default, &reset_timeout);
        }

        // Optimistic comparison protects the edited value, not only the key.
        let (data, metadata) = editable_query(&backend, 71, &ds, &sql);
        query_context(
            &backend,
            79,
            &ds,
            &default,
            &format!("UPDATE {qualified} SET name='concurrent' WHERE tenant=7 AND id=2"),
        );
        let (result, in_tx) = apply_updates(
            &backend,
            mutation_request(
                71,
                &ds,
                &data,
                &metadata,
                &[((0, 2), text("stale overwrite"))],
                false,
            ),
        );
        let failure = result.unwrap_err();
        assert_eq!(failure.source_row, Some(0));
        assert!(!failure.outcome_unknown && !in_tx);
        assert_eq!(
            query_context(&backend, 79, &ds, &default, &sql).rows[0][2],
            Value::Text("concurrent".into())
        );

        let (data, metadata) = editable_query(&backend, 71, &ds, &sql);
        let changes = [((0, 2), text("forbidden"))];
        let mut wrong_context = mutation_request(71, &ds, &data, &metadata, &changes, false);
        wrong_context.expected_context.database = Some("different_context".into());
        assert!(apply_updates(&backend, wrong_context).0.is_err());
        match ds.driver {
            DriverKind::Postgres => {
                query_context(&backend, 71, &ds, &default, "SET search_path TO shop");
                assert!(
                    apply_updates(
                        &backend,
                        mutation_request(71, &ds, &data, &metadata, &changes, false)
                    )
                    .0
                    .is_err()
                );
                assert_eq!(
                    query_context(&backend, 71, &ds, &default, "SELECT current_schema()").rows[0]
                        [0],
                    Value::Text("shop".into())
                );
                change_context(&backend, 71, &ds, &default, 9001).unwrap();
            }
            DriverKind::MySql => {
                query_context(&backend, 71, &ds, &default, "USE shop");
                assert!(
                    apply_updates(
                        &backend,
                        mutation_request(71, &ds, &data, &metadata, &changes, false)
                    )
                    .0
                    .is_err()
                );
                assert_eq!(
                    query_context(&backend, 71, &ds, &default, "SELECT DATABASE()").rows[0][0],
                    Value::Text("shop".into())
                );
                query_context(&backend, 71, &ds, &default, "USE dbeast");
            }
            DriverKind::Sqlite => {}
        }
        let mut wrong_source = mutation_request(71, &ds, &data, &metadata, &changes, false);
        let mut other_source = ds.clone();
        other_source.user.push_str("_changed");
        other_source.file.push_str("_changed");
        wrong_source.source_identity = other_source.identity();
        assert!(apply_updates(&backend, wrong_source).0.is_err());
        backend.close_session(&ds.id, 71);
        assert!(
            apply_updates(
                &backend,
                mutation_request(71, &ds, &data, &metadata, &changes, false)
            )
            .0
            .is_err(),
            "Apply never opens a missing session"
        );
        let fresh = query_context(&backend, 71, &ds, &default, &sql);
        assert_ne!(
            fresh.source.as_ref().unwrap().session_epoch,
            data.source.as_ref().unwrap().session_epoch
        );
        assert!(
            load_edit_metadata(&backend, 71, &ds, &data).is_err(),
            "metadata cannot reuse a previous connection epoch"
        );
        assert!(
            apply_updates(
                &backend,
                mutation_request(71, &ds, &data, &metadata, &changes, false)
            )
            .0
            .is_err(),
            "old capability cannot mutate a new session"
        );
        assert_eq!(
            query_context(&backend, 79, &ds, &default, &sql).rows[0][2],
            Value::Text("concurrent".into())
        );
        let (data, metadata) = editable_query(&backend, 71, &ds, &sql);
        query_context(
            &backend,
            79,
            &ds,
            &default,
            &format!("DELETE FROM {qualified} WHERE tenant=7 AND id=2"),
        );
        let failure = apply_updates(
            &backend,
            mutation_request(71, &ds, &data, &metadata, &changes, false),
        )
        .0
        .unwrap_err();
        assert_eq!(failure.source_row, Some(0));
        assert!(!failure.outcome_unknown);
        let remaining = query_context(&backend, 79, &ds, &default, &sql);
        assert_eq!(remaining.rows.len(), 2);
        assert_eq!(remaining.rows[1][2], Value::Text("Mira".into()));

        // A warm metadata cache must not authorize an UPDATE after external DDL.
        let (data, metadata) = editable_query(&backend, 71, &ds, &sql);
        match ds.driver {
            DriverKind::Postgres => {
                let constraint = dbeast::sqlgen::ident(ds.driver, &format!("{table}_pkey"));
                query_context(
                    &backend,
                    79,
                    &ds,
                    &default,
                    &format!("ALTER TABLE {qualified} DROP CONSTRAINT {constraint}"),
                );
            }
            DriverKind::MySql => {
                query_context(
                    &backend,
                    79,
                    &ds,
                    &default,
                    &format!("ALTER TABLE {qualified} DROP PRIMARY KEY"),
                );
            }
            DriverKind::Sqlite => {
                let backup =
                    dbeast::sqlgen::qualified(ds.driver, Some(schema), &format!("{table}_copy"));
                query_context(
                    &backend,
                    79,
                    &ds,
                    &default,
                    &format!("CREATE TABLE {backup} AS SELECT * FROM {qualified}"),
                );
                query_context(
                    &backend,
                    79,
                    &ds,
                    &default,
                    &format!("DROP TABLE {qualified}"),
                );
                query_context(
                    &backend,
                    79,
                    &ds,
                    &default,
                    &format!(
                        "ALTER TABLE {backup} RENAME TO {}",
                        dbeast::sqlgen::ident(ds.driver, &table)
                    ),
                );
            }
        }
        assert!(
            apply_updates(
                &backend,
                mutation_request(71, &ds, &data, &metadata, &changes, false)
            )
            .0
            .is_err(),
            "Submit must revalidate the physical primary key after DDL"
        );
        assert_eq!(
            query_context(&backend, 79, &ds, &default, &sql).rows,
            data.rows
        );
    }));
    backend.close_session(&ds.id, 71);
    let (cleanup, _) = run(
        &backend,
        req(
            79,
            &ds,
            &[&format!("DROP TABLE IF EXISTS {qualified}")], false,
        ),
    );
    assert!(
        cleanup.iter().all(|(_, result)| result.is_ok()),
        "owned fixture cleanup failed"
    );
    if let Err(panic) = test {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn verified_console_updates_are_atomic_and_session_bound() {
    let directory = std::env::temp_dir().join(format!("dbeast_mutations_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let mut sqlite = DataSource::new(DriverKind::Sqlite);
    sqlite.file = directory
        .join("mutations.sqlite")
        .to_string_lossy()
        .into_owned();
    rusqlite::Connection::open(&sqlite.file)
        .unwrap()
        .execute_batch("PRAGMA journal_mode=WAL")
        .unwrap();
    exercise_verified_updates(sqlite);
    std::fs::remove_dir_all(directory).unwrap();
    if let Some(postgres) = pg() {
        exercise_verified_updates(postgres);
    }
    match std::net::TcpStream::connect("127.0.0.1:53306") {
        Ok(_) => {
            let mut mysql = DataSource::new(DriverKind::MySql);
            mysql.host = "127.0.0.1".into();
            mysql.port = 53306;
            mysql.database = "dbeast".into();
            mysql.user = "dbeast".into();
            mysql.password = "dbeast".into();
            exercise_verified_updates(mysql);
        }
        Err(error) => eprintln!("skipping MySQL console mutations: TCP probe failed: {error}"),
    }
}

#[allow(clippy::too_many_arguments)]
fn table_mutation_request(
    console_id: u64,
    ds: &DataSource,
    data: &QueryResult,
    metadata: &ResultEditMetadata,
    changed: &[((usize, usize), CellEdit)],
    deleted: &[usize],
    inserted: &[Vec<CellEdit>],
    manual_tx: bool,
) -> MutationRequest {
    let changed: HashMap<_, _> = changed.iter().cloned().collect();
    let deleted: std::collections::HashSet<_> = deleted.iter().copied().collect();
    let mutations = dbeast::sqlgen::bound_mutations(
        ds.driver,
        metadata,
        data,
        &dbeast::sqlgen::PendingEdits {
            changed: &changed,
            deleted: &deleted,
            inserted,
        },
    )
    .unwrap();
    MutationRequest {
        console_id,
        result_id: 8001,
        request_id: 8003,
        ds: ds.clone(),
        source_identity: ds.identity(),
        expected_context: data.context.clone().unwrap(),
        session_epoch: data.source.as_ref().unwrap().session_epoch,
        metadata: metadata.clone(),
        mutations,
        manual_tx,
    }
}

/// The table tab path: no console grammar, the result must come from the table.
fn table_query(
    backend: &Backend,
    console_id: u64,
    ds: &DataSource,
    sql: &str,
) -> (QueryResult, Result<ResultEditMetadata, String>, TableEditMetadata) {
    let data = query_context(backend, console_id, ds, &ExecutionContext::default(), sql);
    let table = load_edit_metadata(backend, console_id, ds, &data).unwrap();
    let metadata = dbeast::result_edit::resolve(None, ds.driver, &data, &table);
    (data, metadata, table)
}

fn scalar(backend: &Backend, ds: &DataSource, sql: &str) -> Value {
    let (done, _) = run(backend, req(98, ds, &[sql], false));
    done[0].1.as_ref().unwrap().rows[0][0].clone()
}

fn exercise_table_edits(ds: DataSource) {
    let backend = Backend::new(egui::Context::default());
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let schema = match ds.driver {
        DriverKind::Postgres => "public",
        DriverKind::MySql => "dbeast",
        DriverKind::Sqlite => "main",
    };
    let keyed = dbeast::sqlgen::qualified(ds.driver, Some(schema), &format!("dbeast_keyed_{suffix}"));
    let keyless = dbeast::sqlgen::qualified(ds.driver, Some(schema), &format!("dbeast_keyless_{suffix}"));
    let default = ExecutionContext::default();
    let text = |value: &str| CellEdit::Text(value.into());
    let engine = if ds.driver == DriverKind::MySql { " ENGINE=InnoDB" } else { "" };
    let test = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        for sql in [
            format!("CREATE TABLE {keyed} (id INTEGER NOT NULL PRIMARY KEY, name VARCHAR(40) NOT NULL, amount INTEGER NOT NULL DEFAULT 0 CHECK (amount >= 0)){engine}"),
            format!("INSERT INTO {keyed} (id, name, amount) VALUES (1, 'one', 1), (2, 'two', 2), (3, 'three', 3)"),
            format!("CREATE TABLE {keyless} (v VARCHAR(10)){engine}"),
            format!("INSERT INTO {keyless} (v) VALUES ('old'), ('old')"),
        ] {
            query_context(&backend, 81, &ds, &default, &sql);
        }

        // Without a primary key, duplicates cannot be told apart. PostgreSQL and
        // MySQL stay read-only; SQLite identifies rows by rowid instead.
        let (_, metadata, table) =
            table_query(&backend, 82, &ds, &format!("SELECT * FROM {keyless}"));
        if ds.driver == DriverKind::Sqlite {
            assert_eq!(table.primary_key, [dbeast::model::SQLITE_ROWID]);
            assert!(metadata.is_err(), "rowid must be selected before editing");
            let (data, metadata, _) =
                table_query(&backend, 82, &ds, &format!("SELECT rowid, * FROM {keyless}"));
            let metadata = metadata.unwrap();
            assert_eq!(metadata.primary_key, [0]);
            assert!(!metadata.writable[0], "rowid is never assigned by an edit");
            let request = table_mutation_request(82, &ds, &data, &metadata, &[((1, 1), text("new"))], &[0], &[], false);
            assert_eq!(apply_updates(&backend, request).0.unwrap().changed_rows, 2);
            assert_eq!(scalar(&backend, &ds, &format!("SELECT count(*) FROM {keyless}")), Value::Int(1));
            assert_eq!(scalar(&backend, &ds, &format!("SELECT v FROM {keyless}")), Value::Text("new".into()));
        } else {
            assert!(table.primary_key.is_empty());
            assert!(metadata.unwrap_err().contains("no primary key"));
        }

        // A table tab filter may contain a subquery; the console grammar would refuse it.
        let filtered = format!("SELECT * FROM {keyed} WHERE id IN (SELECT id FROM {keyed}) ORDER BY id");
        let (data, metadata, table) = table_query(&backend, 83, &ds, &filtered);
        let metadata = metadata.unwrap();
        assert!(dbeast::result_edit::resolve(Some(&filtered), ds.driver, &data, &table).is_err());
        let id = |n: i64| text(&n.to_string());
        let request = table_mutation_request(
            83, &ds, &data, &metadata,
            &[((1, 1), text("TWO"))],
            &[0],
            &[vec![id(10), text("ten"), CellEdit::Default]],
            false,
        );
        let (result, in_tx) = apply_updates(&backend, request);
        assert_eq!(result.unwrap().changed_rows, 3);
        assert!(!in_tx);
        assert_eq!(scalar(&backend, &ds, &format!("SELECT count(*) FROM {keyed} WHERE id = 1")), Value::Int(0));
        assert_eq!(scalar(&backend, &ds, &format!("SELECT name FROM {keyed} WHERE id = 2")), Value::Text("TWO".into()));
        assert_eq!(scalar(&backend, &ds, &format!("SELECT amount FROM {keyed} WHERE id = 10")), Value::Int(0));

        // Another client changes row 3 after it was shown: deleting it from the
        // stale view is a conflict, and the whole batch (including the insert) rolls back.
        let (data, metadata, _) = table_query(&backend, 84, &ds, &filtered);
        let metadata = metadata.unwrap();
        query_context(&backend, 85, &ds, &default, &format!("UPDATE {keyed} SET name = 'changed elsewhere' WHERE id = 3"));
        let row3 = data.rows.iter().position(|row| row[0] == Value::Int(3)).unwrap();
        let request = table_mutation_request(
            84, &ds, &data, &metadata, &[], &[row3], &[vec![id(20), text("twenty"), CellEdit::Default]], false,
        );
        let failure = apply_updates(&backend, request).0.unwrap_err();
        assert!(failure.message.contains("matched 0 rows"), "{}", failure.message);
        assert!(!failure.outcome_unknown);
        assert_eq!(scalar(&backend, &ds, &format!("SELECT count(*) FROM {keyed} WHERE id IN (3, 20)")), Value::Int(1));

        // A constraint error in the middle of a batch leaves nothing applied.
        let (data, metadata, _) = table_query(&backend, 84, &ds, &filtered);
        let metadata = metadata.unwrap();
        let row2 = data.rows.iter().position(|row| row[0] == Value::Int(2)).unwrap();
        let request = table_mutation_request(
            84, &ds, &data, &metadata,
            &[((row2, 1), text("partial")), ((row2, 2), text("-5"))],
            &[],
            &[vec![id(30), text("thirty"), CellEdit::Default]],
            false,
        );
        let failure = apply_updates(&backend, request).0.unwrap_err();
        assert_eq!(failure.source_row, Some(row2));
        assert_eq!(scalar(&backend, &ds, &format!("SELECT name FROM {keyed} WHERE id = 2")), Value::Text("TWO".into()));
        assert_eq!(scalar(&backend, &ds, &format!("SELECT count(*) FROM {keyed} WHERE id = 30")), Value::Int(0));

        // Inside a manual transaction a failed batch rolls back to its savepoint
        // and keeps the user's transaction and earlier work usable.
        let (_, in_tx) = run(&backend, req(86, &ds, &[&format!("UPDATE {keyed} SET name = 'in tx' WHERE id = 10")], true));
        assert!(in_tx);
        let (data, metadata, _) = table_query(&backend, 86, &ds, &filtered);
        let metadata = metadata.unwrap();
        let row10 = data.rows.iter().position(|row| row[0] == Value::Int(10)).unwrap();
        let request = table_mutation_request(86, &ds, &data, &metadata, &[((row10, 2), text("-1"))], &[], &[], true);
        let (result, in_tx) = apply_updates(&backend, request);
        assert!(result.is_err());
        assert!(in_tx, "the user's transaction stays open");
        let request = table_mutation_request(86, &ds, &data, &metadata, &[((row10, 2), text("7"))], &[], &[], true);
        let (result, in_tx) = apply_updates(&backend, request);
        assert_eq!(result.unwrap().changed_rows, 1);
        assert!(in_tx);
        let (done, in_tx) = run(&backend, req(86, &ds, &["COMMIT"], false));
        assert!(done[0].1.is_ok() && !in_tx);
        assert_eq!(scalar(&backend, &ds, &format!("SELECT name FROM {keyed} WHERE id = 10")), Value::Text("in tx".into()));
        assert_eq!(scalar(&backend, &ds, &format!("SELECT amount FROM {keyed} WHERE id = 10")), Value::Int(7));
    }));
    for table in [&keyed, &keyless] {
        let _ = run(&backend, req(81, &ds, &[&format!("DROP TABLE IF EXISTS {table}")], false));
    }
    if let Err(panic) = test {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn table_edits_are_verified_parameterized_and_atomic() {
    let directory = std::env::temp_dir().join(format!("dbeast_table_edits_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let mut sqlite = DataSource::new(DriverKind::Sqlite);
    sqlite.file = directory.join("edits.sqlite").to_string_lossy().into_owned();
    rusqlite::Connection::open(&sqlite.file)
        .unwrap()
        .execute_batch("PRAGMA journal_mode=WAL")
        .unwrap();
    exercise_table_edits(sqlite);
    std::fs::remove_dir_all(directory).unwrap();
    if let Some(postgres) = pg() {
        exercise_table_edits(postgres);
    }
    match std::net::TcpStream::connect("127.0.0.1:53306") {
        Ok(_) => {
            let mut mysql = DataSource::new(DriverKind::MySql);
            mysql.host = "127.0.0.1".into();
            mysql.port = 53306;
            mysql.database = "dbeast".into();
            mysql.user = "dbeast".into();
            mysql.password = "dbeast".into();
            exercise_table_edits(mysql);
        }
        Err(error) => eprintln!("skipping MySQL table edits: TCP probe failed: {error}"),
    }
}

fn report_backend_timings(driver: DriverKind, phase: &str, mut samples: Vec<Duration>) {
    samples.sort_unstable();
    let median = samples[samples.len() / 2].as_secs_f64() * 1000.0;
    let p95 = samples[(samples.len() * 95).div_ceil(100) - 1].as_secs_f64() * 1000.0;
    println!(
        "console_updates driver={} phase={phase} samples={} median_ms={median:.3} p95_ms={p95:.3}",
        driver.label(),
        samples.len()
    );
}

fn benchmark_console_updates(ds: DataSource) {
    const SAMPLES: usize = 7;
    const ROWS: usize = 100;
    let backend = Backend::new(egui::Context::default());
    let table = format!("dbeast_bench_{}", uuid::Uuid::new_v4().simple());
    let schema = match ds.driver {
        DriverKind::Postgres => "public",
        DriverKind::MySql => "dbeast",
        DriverKind::Sqlite => "main",
    };
    let qualified = dbeast::sqlgen::qualified(ds.driver, Some(schema), &table);
    let sql = format!("SELECT id AS selected_id,name AS display_name FROM {qualified} ORDER BY id");
    let default = ExecutionContext::default();
    let test = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let engine = if ds.driver == DriverKind::MySql {
            " ENGINE=InnoDB"
        } else {
            ""
        };
        query_context(
            &backend,
            9200,
            &ds,
            &default,
            &format!(
                "CREATE TABLE {qualified} (id INTEGER PRIMARY KEY,name TEXT NOT NULL){engine}"
            ),
        );
        let rows = (1..=ROWS)
            .map(|id| format!("({id},'before')"))
            .collect::<Vec<_>>()
            .join(",");
        query_context(
            &backend,
            9200,
            &ds,
            &default,
            &format!("INSERT INTO {qualified} VALUES {rows}"),
        );

        // Opening the connection and fetching rows are deliberately outside these
        // timings. A fresh console has no table metadata cached for its epoch.
        let mut cold = Vec::with_capacity(SAMPLES);
        for sample in 0..SAMPLES {
            let console_id = 9000 + sample as u64;
            let data = query_context(&backend, console_id, &ds, &default, &sql);
            let started = Instant::now();
            let metadata = load_edit_metadata(&backend, console_id, &ds, &data).unwrap();
            cold.push(started.elapsed());
            assert_eq!(metadata.primary_key, ["id"]);
            backend.close_session(&ds.id, console_id);
        }
        report_backend_timings(ds.driver, "metadata_cold_context_and_catalog", cold);

        let data = query_context(&backend, 9100, &ds, &default, &sql);
        load_edit_metadata(&backend, 9100, &ds, &data).unwrap();
        let mut warm = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let started = Instant::now();
            let metadata = load_edit_metadata(&backend, 9100, &ds, &data).unwrap();
            warm.push(started.elapsed());
            assert_eq!(metadata.primary_key, ["id"]);
        }
        report_backend_timings(ds.driver, "metadata_warm_context_and_cache", warm);

        for count in [1, ROWS] {
            let mut samples = Vec::with_capacity(SAMPLES);
            for sample in 0..SAMPLES {
                // Use the exact current result and metadata for every mutation.
                // Fetching, metadata delivery and SQL compilation are not timed.
                let (data, metadata) = editable_query(&backend, 9100, &ds, &sql);
                let values: Vec<_> = (0..count)
                    .map(|row| format!("batch_{count}_{sample}_row_{row}"))
                    .collect();
                let changes: Vec<_> = values
                    .iter()
                    .enumerate()
                    .map(|(row, value)| ((row, 1), CellEdit::Text(value.clone())))
                    .collect();
                let request = mutation_request(9100, &ds, &data, &metadata, &changes, false);
                let started = Instant::now();
                let (report, in_tx) = apply_updates(&backend, request);
                samples.push(started.elapsed());
                assert_eq!(report.unwrap().changed_rows, count);
                assert!(!in_tx);
                let verified = query_context(&backend, 9200, &ds, &default, &sql);
                for (row, value) in values.into_iter().enumerate() {
                    assert_eq!(verified.rows[row][1], Value::Text(value));
                }
            }
            report_backend_timings(
                ds.driver,
                &format!("submit_{count}_rows_context_catalog_savepoint_commit"),
                samples,
            );
        }
    }));
    for console_id in 9000..9000 + SAMPLES as u64 {
        backend.close_session(&ds.id, console_id);
    }
    backend.close_session(&ds.id, 9100);
    let (done, _) = run(
        &backend,
        req(
            9200,
            &ds,
            &[&format!("DROP TABLE IF EXISTS {qualified}")], false,
        ),
    );
    assert!(
        done.iter().all(|(_, result)| result.is_ok()),
        "benchmark fixture cleanup failed"
    );
    if let Err(panic) = test {
        std::panic::resume_unwind(panic);
    }
}

/// Wall-clock backend request latency, including channel/runtime scheduling and
/// database work. Not UI-frame or GPU time. Setup, connection opening, SELECT,
/// metadata for Submit, SQL compilation and verification are outside each sample.
/// Submit includes context guard, fresh catalog validation, UPDATEs and commit.
#[test]
#[ignore = "manual release measurement of metadata caching and 1/100-row Submit latency"]
#[allow(clippy::assertions_on_constants)]
fn console_update_metadata_and_submit_latency() {
    assert!(!cfg!(debug_assertions), "run this benchmark with --release");
    println!(
        "console_updates metric=backend_wall_clock no_ui=true no_gpu=true cold_cache=per_connection_epoch warm_cache=same_connection samples=7"
    );
    let directory =
        std::env::temp_dir().join(format!("dbeast_mutation_bench_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let mut sqlite = DataSource::new(DriverKind::Sqlite);
    sqlite.file = directory
        .join("benchmark.sqlite")
        .to_string_lossy()
        .into_owned();
    benchmark_console_updates(sqlite);
    std::fs::remove_dir_all(directory).unwrap();
    if let Some(postgres) = pg() {
        benchmark_console_updates(postgres);
    }
    match std::net::TcpStream::connect("127.0.0.1:53306") {
        Ok(_) => {
            let mut mysql = DataSource::new(DriverKind::MySql);
            mysql.host = "127.0.0.1".into();
            mysql.port = 53306;
            mysql.database = "dbeast".into();
            mysql.user = "dbeast".into();
            mysql.password = "dbeast".into();
            benchmark_console_updates(mysql);
        }
        Err(error) => eprintln!("skipping MySQL mutation benchmark: TCP probe failed: {error}"),
    }
}

#[test]
fn postgres_console_context_isolation_and_stale_results() {
    let Some(ds) = pg() else { return };
    let backend = Backend::new(egui::Context::default());
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let schema_a = format!("dbeast_ctx_{}_a", &suffix[..12]);
    let schema_b = format!("dbeast_ctx_{}_b", &suffix[..12]);
    let quoted = format!("dbeast_ctx_{} Żółw.,'\";--", &suffix[..12]);
    let context_a = named_context(&schema_a);
    let context_b = named_context(&schema_b);
    let default = ExecutionContext::default();
    let export_path = std::env::temp_dir().join(format!("dbeast_context_{suffix}.csv"));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        for (schema, column, marker) in [
            (&schema_a, "only_a", "A"),
            (&schema_b, "only_b", "B"),
            (&quoted, "quoted", "Q"),
        ] {
            let schema = pg_ident(schema);
            query_context(
                &backend,
                590,
                &ds,
                &default,
                &format!("CREATE SCHEMA {schema}"),
            );
            query_context(
                &backend,
                590,
                &ds,
                &default,
                &format!("CREATE TABLE {schema}.same_name (id integer PRIMARY KEY, {column} text)"),
            );
            query_context(
                &backend,
                590,
                &ds,
                &default,
                &format!(
                    "INSERT INTO {schema}.same_name VALUES (1, '{marker}1'), (2, '{marker}2'), (3, '{marker}3')"
                ),
            );
        }
        let initial =
            query_context(&backend, 51, &ds, &default, "SHOW search_path").rows[0][0].clone();
        let pid = query_context(&backend, 51, &ds, &default, "SELECT pg_backend_pid()").rows[0][0]
            .clone();
        change_context(&backend, 51, &ds, &context_a, 1).unwrap();
        change_context(&backend, 52, &ds, &context_b, 1).unwrap();
        let sql = "SELECT * FROM same_name ORDER BY id";
        let a = query_context(&backend, 51, &ds, &context_a, sql);
        let b = query_context(&backend, 52, &ds, &context_b, sql);
        assert_eq!(a.columns[1].name, "only_a");
        assert_eq!(b.columns[1].name, "only_b");
        assert_eq!(a.rows[0][1], Value::Text("A1".into()));
        assert_eq!(b.rows[0][1], Value::Text("B1".into()));
        let snapshot_a = a.context.clone().expect("actual result context");
        assert_eq!(snapshot_a.database.as_deref(), Some("dbeast"));
        assert!(snapshot_a.schemas.contains(&schema_a));
        assert!(snapshot_a.temporary_schema.is_none());

        // A restored result can replay on a fresh session after its saved selection
        // is initialized. An already-open session with another context is guarded below.
        let mut restored = contextual_request(53, &ds, &context_a, sql);
        restored.page = Page { skip: 1, limit: 1 };
        restored.expected_context = Some(snapshot_a.clone());
        let (restored, _) = run(&backend, restored);
        assert_eq!(restored.len(), 1);
        let restored = restored[0].1.as_ref().unwrap();
        assert_eq!(restored.rows[0][1], Value::Text("A2".into()));
        assert_eq!(restored.context.as_ref(), Some(&snapshot_a));
        query_context(
            &backend,
            51,
            &ds,
            &context_a,
            "CREATE TEMP TABLE context_temp AS SELECT 'kept'::text AS note",
        );

        let quoted_context = named_context(&quoted);
        change_context(&backend, 51, &ds, &quoted_context, 2).unwrap();
        assert_eq!(
            query_context(&backend, 51, &ds, &quoted_context, sql).rows[0][1],
            Value::Text("Q1".into())
        );
        for invalid in ["".to_owned(), format!("dbeast_missing_{suffix}")] {
            assert!(change_context(&backend, 51, &ds, &named_context(&invalid), 3).is_err());
            assert_eq!(
                query_context(&backend, 51, &ds, &quoted_context, sql).rows[0][1],
                Value::Text("Q1".into())
            );
        }
        assert_eq!(
            query_context(
                &backend,
                51,
                &ds,
                &quoted_context,
                "SELECT pg_backend_pid()"
            )
            .rows[0][0],
            pid
        );
        assert_eq!(
            query_context(
                &backend,
                51,
                &ds,
                &quoted_context,
                "SELECT note FROM context_temp"
            )
            .rows[0][0],
            Value::Text("kept".into())
        );
        change_context(&backend, 51, &ds, &default, 4).unwrap();
        assert_eq!(
            query_context(&backend, 51, &ds, &default, "SHOW search_path").rows[0][0],
            initial
        );

        // The backend guard is required even if the UI had stale transaction state.
        change_context(&backend, 51, &ds, &context_a, 5).unwrap();
        let (_, in_tx) = run(&backend, contextual_request(51, &ds, &context_a, "BEGIN"));
        assert!(in_tx);
        assert!(change_context(&backend, 51, &ds, &context_b, 6).is_err());
        let (failed, in_tx) = run(
            &backend,
            contextual_request(51, &ds, &context_a, "SELECT 1 / 0"),
        );
        assert!(in_tx && failed[0].1.is_err());
        let invalid = named_context("schema_that_does_not_exist");
        let (rollback, in_tx) = run(&backend, contextual_request(51, &ds, &invalid, "ROLLBACK"));
        assert!(!in_tx, "rollback must bypass a failed context choice");
        assert!(rollback[0].1.is_ok());
        assert_eq!(
            query_context(&backend, 52, &ds, &context_b, sql).rows[0][1],
            Value::Text("B1".into())
        );

        change_context(&backend, 51, &ds, &context_b, 7).unwrap();
        let mut replay = contextual_request(51, &ds, &context_b, sql);
        replay.page = Page { skip: 1, limit: 1 };
        replay.expected_context = Some(snapshot_a.clone());
        let (refused, _) = run(&backend, replay);
        assert!(
            refused.iter().any(|(_, result)| result.is_err()),
            "old A result must not replay in B"
        );
        assert_eq!(
            query_context(&backend, 51, &ds, &context_b, sql).rows[0][1],
            Value::Text("B1".into())
        );
        backend.export(
            51,
            ds.clone(),
            sql.into(),
            ExportOptions::default(),
            TableName::new("same_name"),
            export_path.clone(),
            Some(snapshot_a.clone()),
        );
        let summary = task_outcome(&backend, 51).unwrap();
        assert!(summary.starts_with("3 rows exported"), "{summary}");
        assert_eq!(
            std::fs::read_to_string(&export_path).unwrap(),
            "id,only_a\n1,A1\n2,A2\n3,A3\n"
        );
        assert_eq!(
            query_context(
                &backend,
                51,
                &ds,
                &context_b,
                "SELECT note FROM context_temp"
            )
            .rows[0][0],
            Value::Text("kept".into())
        );

        // Ordinary runs must not undo a manual SET; replay still checks actual state.
        change_context(&backend, 51, &ds, &context_a, 8).unwrap();
        query_context(
            &backend,
            51,
            &ds,
            &context_a,
            &format!("SET search_path TO {}", pg_ident(&schema_b)),
        );
        let actual_b = query_context(&backend, 51, &ds, &context_a, sql);
        assert_eq!(actual_b.rows[0][1], Value::Text("B1".into()));
        assert!(actual_b.context.unwrap().schemas.contains(&schema_b));
        let mut replay = contextual_request(51, &ds, &context_a, sql);
        replay.expected_context = Some(snapshot_a);
        assert!(
            run(&backend, replay)
                .0
                .iter()
                .any(|(_, result)| result.is_err())
        );
        assert_eq!(
            query_context(&backend, 51, &ds, &context_a, sql).rows[0][1],
            Value::Text("B1".into())
        );

        // A separate export connection must not silently resolve a temporary table
        // to the permanent table with the same name in the captured search path.
        query_context(
            &backend,
            51,
            &ds,
            &context_a,
            "CREATE TEMP TABLE same_name (id integer, marker text)",
        );
        query_context(
            &backend,
            51,
            &ds,
            &context_a,
            "INSERT INTO same_name VALUES (1, 'temporary')",
        );
        let temporary = query_context(&backend, 51, &ds, &context_a, sql);
        assert_eq!(temporary.rows[0][1], Value::Text("temporary".into()));
        let temporary = temporary.context.unwrap();
        assert!(temporary.temporary_schema.is_some());
        backend.export(
            51,
            ds.clone(),
            sql.into(),
            ExportOptions::default(),
            TableName::new("same_name"),
            export_path.clone(),
            Some(temporary),
        );
        let error = task_outcome(&backend, 51).expect_err("temporary context must refuse separate export");
        assert!(error.to_lowercase().contains("tempor"), "{error}");
        assert_eq!(
            std::fs::read_to_string(&export_path).unwrap(),
            "id,only_a\n1,A1\n2,A2\n3,A3\n",
            "refused export must not replace an existing file"
        );
    }));
    backend.close_session(&ds.id, 51);
    backend.close_session(&ds.id, 52);
    backend.close_session(&ds.id, 53);
    for schema in [&schema_a, &schema_b, &quoted] {
        let (done, _) = run(
            &backend,
            req(
                590,
                &ds,
                &[&format!(
                    "DROP SCHEMA IF EXISTS {} CASCADE",
                    pg_ident(schema)
                )], false,
        ),
        );
        assert!(
            done.iter().all(|(_, result)| result.is_ok()),
            "fixture cleanup failed"
        );
    }
    std::fs::remove_file(export_path).ok();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn mysql_database_context_is_scoped_to_each_console() {
    if let Err(error) = std::net::TcpStream::connect("127.0.0.1:53306") {
        eprintln!("skipping MySQL backend context test: TCP probe failed: {error}");
        return;
    }
    let mut ds = DataSource::new(DriverKind::MySql);
    ds.host = "127.0.0.1".into();
    ds.port = 53306;
    ds.user = "dbeast".into();
    ds.password = "dbeast".into();
    ds.database = "dbeast".into();
    let backend = Backend::new(egui::Context::default());
    let table = format!("dbeast_context_{}", uuid::Uuid::new_v4().simple());
    let default = ExecutionContext::default();
    let shop = named_context("shop");
    let sql = format!("SELECT * FROM `{table}` ORDER BY id");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        for (database, column, marker) in [("dbeast", "only_a", "A"), ("shop", "only_b", "B")] {
            query_context(
                &backend,
                690,
                &ds,
                &default,
                &format!(
                    "CREATE TABLE `{database}`.`{table}` (id integer PRIMARY KEY, {column} varchar(16))"
                ),
            );
            query_context(
                &backend,
                690,
                &ds,
                &default,
                &format!(
                    "INSERT INTO `{database}`.`{table}` VALUES (1, '{marker}1'), (2, '{marker}2')"
                ),
            );
        }
        let a = query_context(&backend, 61, &ds, &default, &sql);
        let snapshot_a = a.context.unwrap();
        assert_eq!(a.rows[0][1], Value::Text("A1".into()));
        let connection =
            query_context(&backend, 61, &ds, &default, "SELECT CONNECTION_ID()").rows[0][0].clone();
        query_context(
            &backend,
            61,
            &ds,
            &default,
            "CREATE TEMPORARY TABLE context_temp (note varchar(16))",
        );
        query_context(
            &backend,
            61,
            &ds,
            &default,
            "INSERT INTO context_temp VALUES ('kept')",
        );
        change_context(&backend, 61, &ds, &shop, 1).unwrap();
        assert_eq!(
            query_context(&backend, 61, &ds, &shop, &sql).rows[0][1],
            Value::Text("B1".into())
        );
        assert_eq!(
            query_context(&backend, 62, &ds, &default, &sql).rows[0][1],
            Value::Text("A1".into())
        );
        assert_eq!(
            query_context(&backend, 61, &ds, &shop, "SELECT DATABASE()").rows[0][0],
            Value::Text("shop".into())
        );
        assert_eq!(
            query_context(&backend, 61, &ds, &shop, "SELECT CONNECTION_ID()").rows[0][0],
            connection
        );
        assert_eq!(
            query_context(
                &backend,
                61,
                &ds,
                &shop,
                "SELECT note FROM dbeast.context_temp"
            )
            .rows[0][0],
            Value::Text("kept".into())
        );
        let missing = named_context(&format!("missing_{}", uuid::Uuid::new_v4().simple()));
        assert!(change_context(&backend, 61, &ds, &missing, 2).is_err());
        assert_eq!(
            query_context(&backend, 61, &ds, &shop, &sql).rows[0][1],
            Value::Text("B1".into())
        );
        change_context(&backend, 61, &ds, &default, 3).unwrap();
        assert_eq!(
            query_context(&backend, 61, &ds, &default, "SELECT DATABASE()").rows[0][0],
            Value::Text("dbeast".into())
        );

        let (_, in_tx) = run(
            &backend,
            contextual_request(61, &ds, &default, "START TRANSACTION"),
        );
        assert!(in_tx);
        assert!(change_context(&backend, 61, &ds, &shop, 4).is_err());
        let (_, in_tx) = run(&backend, contextual_request(61, &ds, &missing, "ROLLBACK"));
        assert!(!in_tx);
        query_context(&backend, 61, &ds, &default, "USE `shop`");
        let actual_shop = query_context(&backend, 61, &ds, &default, &sql);
        assert_eq!(
            actual_shop.context.as_ref().unwrap().database.as_deref(),
            Some("shop")
        );
        assert_eq!(actual_shop.rows[0][1], Value::Text("B1".into()));
        let mut replay = contextual_request(61, &ds, &default, &sql);
        replay.expected_context = Some(snapshot_a);
        replay.page = Page { skip: 1, limit: 1 };
        assert!(
            run(&backend, replay)
                .0
                .iter()
                .any(|(_, result)| result.is_err())
        );
        assert_eq!(
            query_context(&backend, 61, &ds, &default, "SELECT DATABASE()").rows[0][0],
            Value::Text("shop".into())
        );
        assert_eq!(
            query_context(&backend, 62, &ds, &default, "SELECT DATABASE()").rows[0][0],
            Value::Text("dbeast".into())
        );
    }));
    backend.close_session(&ds.id, 61);
    backend.close_session(&ds.id, 62);
    for database in ["dbeast", "shop"] {
        let (done, _) = run(
            &backend,
            req(
                690,
                &ds,
                &[&format!("DROP TABLE IF EXISTS `{database}`.`{table}`")], false,
        ),
        );
        assert!(
            done.iter().all(|(_, result)| result.is_ok()),
            "fixture cleanup failed"
        );
    }
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

// A loopback peer accepts TCP but never answers PostgreSQL startup. This also
// covers Stop before the driver has produced a database cancellation handle.
fn unresponsive_peer() -> (DataSource, std::thread::JoinHandle<()>) {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut ds = DataSource::new(DriverKind::Postgres);
    ds.host = "127.0.0.1".into();
    ds.port = listener.local_addr().unwrap().port();
    ds.use_tls = false;
    let worker = std::thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "backend did not connect");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("{error}"),
            }
        };
        socket.set_nonblocking(false).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut bytes = [0; 1024];
        loop {
            match socket.read(&mut bytes) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => break,
                Err(error) => panic!("connection was not closed: {error}"),
            }
        }
    });
    (ds, worker)
}

#[test]
fn stop_and_disconnect_release_unresponsive_connection_setup() {
    for action in 0..3 {
        let (ds, peer) = unresponsive_peer();
        let backend = Backend::new(egui::Context::default());
        backend.run(req(1, &ds, &["select 1"], false));
        // Allow the worker to enter startup, where no CancelHandle exists yet.
        std::thread::sleep(Duration::from_millis(100));
        let started = Instant::now();
        match action {
            0 => backend.cancel(1),
            1 => backend.disconnect(&ds.id),
            _ => backend.close_session(&ds.id, 1),
        }
        let (done, in_tx) = collect_run(&backend, 1);
        eprintln!("recovery latency: {:.1} ms", started.elapsed().as_secs_f64() * 1000.0);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(!in_tx);
        assert!(
            done.last()
                .unwrap()
                .1
                .as_ref()
                .unwrap_err()
                .contains("interrupted"),
            "{:?}",
            done
        );
        peer.join().unwrap();
        if action == 0 {
            let (done, ..) = run(&backend, req(1, &ds, &["select 2"], false));
            assert!(
                done[0]
                    .1
                    .as_ref()
                    .unwrap_err()
                    .contains("Disconnect and reconnect")
            );
        }
    }
}

#[test]
fn stop_is_registered_before_worker_starts() {
    let backend = Backend::new(egui::Context::default());
    let mut ds = DataSource::new(DriverKind::Postgres);
    ds.host = "127.0.0.1".into();
    ds.port = 1;
    backend.run(req(1, &ds, &["select 1"], false));
    backend.cancel(1);
    let (done, _) = collect_run(&backend, 1);
    assert!(done[0].1.as_ref().unwrap_err().contains("interrupted"));
}

#[test]
fn stop_and_disconnect_release_a_blackholed_result_page() {
    use std::io::{Read, Write};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let Some(original) = pg() else { return };
    for disconnect in [false, true] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut ds = original.clone();
        ds.id = uuid::Uuid::new_v4().to_string();
        ds.host = "127.0.0.1".into();
        ds.port = listener.local_addr().unwrap().port();
        ds.use_tls = false;
        let blackhole = Arc::new(AtomicBool::new(false));
        let cut = blackhole.clone();
        let (observed, received) = std::sync::mpsc::channel();
        let peer = std::thread::spawn(move || {
            let (mut client, _) = listener.accept().unwrap();
            let mut server = std::net::TcpStream::connect("127.0.0.1:55432").unwrap();
            client.set_nonblocking(true).unwrap();
            server.set_nonblocking(true).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut buffer = [0; 65536];
            loop {
                assert!(Instant::now() < deadline, "proxy connection leaked");
                for incoming in [true, false] {
                    let (from, to) = if incoming {
                        (&mut client, &mut server)
                    } else {
                        (&mut server, &mut client)
                    };
                    match from.read(&mut buffer) {
                        Ok(0) => return,
                        Ok(n) => {
                            if cut.load(Ordering::Acquire) {
                                if incoming {
                                    let _ = observed.send(());
                                }
                            } else {
                                to.write_all(&buffer[..n]).unwrap();
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => return,
                        Err(error) => panic!("proxy: {error}"),
                    }
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        let backend = Backend::new(egui::Context::default());
        let (first, ..) = run(
            &backend,
            req(1, &ds, &["select generate_series(1, 1000)"], false),
        );
        assert_eq!(first[0].1.as_ref().unwrap().rows.len(), 500);
        blackhole.store(true, Ordering::Release);
        let mut next = req(1, &ds, &["select generate_series(1, 1000)"], false);
        next.page.skip = 500;
        backend.run(next);
        received.recv_timeout(Duration::from_secs(2)).unwrap();
        let started = Instant::now();
        if disconnect {
            backend.disconnect(&ds.id);
        } else {
            backend.cancel(1);
        }
        let (done, in_tx) = collect_run(&backend, 1);
        eprintln!("recovery latency: {:.1} ms", started.elapsed().as_secs_f64() * 1000.0);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(!in_tx);
        assert!(
            done[0]
                .1
                .as_ref()
                .unwrap_err()
                .contains("outcome is unknown")
        );
        peer.join().unwrap();
        // Another source remains usable and the disconnected source can be
        // explicitly reopened. No interrupted statement is replayed.
        let (other, ..) = run(&backend, req(2, &original, &["select 42"], false));
        assert!(other[0].1.is_ok());
        backend.disconnect(&ds.id);
        ds.port = original.port;
        let (reopened, ..) = run(&backend, req(1, &ds, &["select 1"], false));
        assert!(reopened[0].1.is_ok());
    }
}

#[test]
fn disconnect_releases_unresponsive_explorer_connection() {
    let (ds, peer) = unresponsive_peer();
    let backend = Backend::new(egui::Context::default());
    backend.connect(ds.clone(), 1);
    std::thread::sleep(Duration::from_millis(100));
    backend.disconnect(&ds.id);
    peer.join().unwrap();
    assert!(
        backend.rx.try_recv().is_err(),
        "disconnected metadata must not publish late results"
    );
}

fn mysql() -> Option<DataSource> {
    if let Err(error) = std::net::TcpStream::connect("127.0.0.1:53306") {
        eprintln!("skipping MySQL transaction test: TCP probe failed: {error}");
        return None;
    }
    let mut ds = DataSource::new(DriverKind::MySql);
    ds.host = "127.0.0.1".into();
    ds.port = 53306;
    ds.user = "dbeast".into();
    ds.password = "dbeast".into();
    ds.database = "dbeast".into();
    Some(ds)
}

/// Like [`run`], also collecting the console's Output notices.
fn run_noticed(backend: &Backend, req: RunRequest) -> (Statements, Vec<String>, bool) {
    let console_id = req.console_id;
    backend.run(req);
    let (mut done, mut notices) = (Vec::new(), Vec::new());
    loop {
        match backend.rx.recv_timeout(Duration::from_secs(10)).expect("backend event") {
            Event::StatementDone { console_id: c, sql, result, .. } if c == console_id => done.push((sql, result)),
            Event::Notice { console_id: c, text } if c == console_id => notices.push(text),
            Event::RunFinished { console_id: c, in_tx } if c == console_id => return (done, notices, in_tx),
            _ => {}
        }
    }
}

/// MySQL DDL commits implicitly. The console must follow the server's real
/// transaction state, so Manual mode never runs a later change in autocommit
/// and Rollback really undoes it.
#[test]
fn mysql_implicit_commit_keeps_manual_transactions_real() {
    let Some(ds) = mysql() else { return };
    let backend = Backend::new(egui::Context::default());
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let (data, ddl_a, ddl_b) = (
        format!("dbeast_tx_{suffix}"),
        format!("dbeast_tx_a_{suffix}"),
        format!("dbeast_tx_b_{suffix}"),
    );
    // Read through another console, which has its own connection.
    let committed = || scalar(&backend, &ds, &format!("SELECT v FROM {data} WHERE id = 1"));
    let test = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run(&backend, req(700, &ds, &[
            &format!("CREATE TABLE {data} (id integer PRIMARY KEY, v integer NOT NULL) ENGINE=InnoDB"),
            &format!("INSERT INTO {data} VALUES (1, 0)"),
        ], false));

        // A: DDL alone ends the transaction it was given; the next run opens a new one.
        let (done, notices, in_tx) = run_noticed(&backend, req(701, &ds, &[&format!("CREATE TABLE {ddl_a} (id integer) ENGINE=InnoDB")], true));
        assert!(done.iter().all(|(_, result)| result.is_ok()), "{done:?}");
        assert!(!in_tx, "the DDL committed the transaction");
        assert!(notices.is_empty(), "an empty transaction is not reported: {notices:?}");
        let (done, _, in_tx) = run_noticed(&backend, req(701, &ds, &[&format!("UPDATE {data} SET v = 1 WHERE id = 1")], true));
        assert_eq!(done[0].0, "START TRANSACTION");
        assert!(in_tx);
        assert_eq!(committed(), Value::Int(0));
        let (_, in_tx) = run(&backend, req(701, &ds, &["ROLLBACK"], false));
        assert!(!in_tx);
        assert_eq!(committed(), Value::Int(0), "Rollback undid the UPDATE");

        // B: DDL and a change in one script; the change gets a new transaction.
        let (done, notices, in_tx) = run_noticed(&backend, req(702, &ds, &[
            &format!("ALTER TABLE {ddl_a} ADD COLUMN c integer"),
            &format!("UPDATE {data} SET v = 2 WHERE id = 1"),
        ], true));
        assert!(done.iter().all(|(_, result)| result.is_ok()), "{done:?}");
        assert!(in_tx);
        assert!(notices.iter().any(|n| n.starts_with("START TRANSACTION: the remaining statements run in a new transaction")), "{notices:?}");
        assert_eq!(committed(), Value::Int(0));
        let (_, in_tx) = run(&backend, req(702, &ds, &["ROLLBACK"], false));
        assert!(!in_tx);
        assert_eq!(committed(), Value::Int(0), "Rollback undid the UPDATE");

        // C: DDL commits an UPDATE made earlier in the app's transaction; the
        // console reports it and shows no open transaction.
        let (_, _, in_tx) = run_noticed(&backend, req(703, &ds, &[&format!("UPDATE {data} SET v = 3 WHERE id = 1")], true));
        assert!(in_tx);
        assert_eq!(committed(), Value::Int(0));
        let (done, notices, in_tx) = run_noticed(&backend, req(703, &ds, &[&format!("CREATE TABLE {ddl_b} (id integer) ENGINE=InnoDB")], true));
        assert!(done.len() == 1 && done[0].1.is_ok(), "{done:?}");
        assert!(!in_tx, "the server has no open transaction");
        assert!(notices.iter().any(|n| n.starts_with("Transaction committed implicitly by this statement")), "{notices:?}");
        assert_eq!(committed(), Value::Int(3));

        // Work, DDL and more work in one script: only the last change rolls back.
        let (_, notices, in_tx) = run_noticed(&backend, req(703, &ds, &[
            &format!("UPDATE {data} SET v = 4 WHERE id = 1"),
            &format!("DROP TABLE {ddl_b}"),
            &format!("UPDATE {data} SET v = 5 WHERE id = 1"),
        ], true));
        assert!(in_tx);
        assert_eq!(notices.len(), 2, "{notices:?}");
        assert_eq!(committed(), Value::Int(4));
        let (_, in_tx) = run(&backend, req(703, &ds, &["ROLLBACK"], false));
        assert!(!in_tx);
        assert_eq!(committed(), Value::Int(4));
    }));
    for table in [&data, &ddl_a, &ddl_b] {
        let _ = run(&backend, req(700, &ds, &[&format!("DROP TABLE IF EXISTS {table}")], false));
    }
    if let Err(panic) = test {
        std::panic::resume_unwind(panic);
    }
}

fn table_page(
    backend: &Backend,
    ds: &DataSource,
    table: &dbeast::backend::TablePage,
    order: &str,
    page: Page,
) -> QueryResult {
    let mut sql = dbeast::drivers::select_all(ds.driver, table.schema.as_deref(), &table.table);
    if !order.is_empty() {
        sql.push_str(&format!(" ORDER BY {order}"));
    }
    let mut request = req(40, ds, &[&sql], false);
    request.page = page;
    let table = dbeast::backend::TablePage { ordered: !order.is_empty(), ..table.clone() };
    backend.run_table_page(request, table);
    let (mut done, _) = collect_run(backend, 40);
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].0, sql, "the result keeps the unpaged SQL for export and reloads");
    done.remove(0).1.unwrap()
}

fn ids(result: &QueryResult) -> Vec<i64> {
    result
        .rows
        .iter()
        .map(|row| match row[0] {
            Value::Int(id) => id,
            ref other => panic!("unexpected id {other:?}"),
        })
        .collect()
}

/// Table editor pages come from the server (LIMIT/OFFSET) in primary key
/// order, although the rows were stored in reverse order.
#[test]
fn table_pages_are_read_from_the_server_in_key_order() {
    let name = format!("dbeast_pages_{}", std::process::id());
    let dir = std::env::temp_dir().join(format!("dbeast_pages_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("pages.sqlite");
    // INT (not INTEGER) PRIMARY KEY is no rowid alias, so storage order stays reversed.
    rusqlite::Connection::open(&file)
        .unwrap()
        .execute_batch(&format!(
            "CREATE TABLE {name} (id INT PRIMARY KEY, note TEXT);
             WITH RECURSIVE s(g) AS (SELECT 1200 UNION ALL SELECT g - 1 FROM s WHERE g > 1)
             INSERT INTO {name} SELECT g, 'n' || g FROM s;"
        ))
        .unwrap();
    let mut sqlite = DataSource::new(DriverKind::Sqlite);
    sqlite.file = file.to_string_lossy().into_owned();
    let mut sources = vec![(sqlite, None, Vec::new(), Vec::new())];
    if let Some(ds) = pg() {
        sources.push((
            ds,
            Some("public".to_string()),
            vec![
                format!("create table public.{name} (id int primary key, note text)"),
                format!("insert into public.{name} select g, 'n' || g from generate_series(1200, 1, -1) g"),
            ],
            vec![format!("drop table public.{name}")],
        ));
    }
    if let Some(ds) = mysql() {
        sources.push((
            ds,
            Some("shop".to_string()),
            vec![
                format!("create table shop.{name} (id int primary key, note varchar(20))"),
                "set session cte_max_recursion_depth = 2000".into(),
                format!(
                    "insert into shop.{name} with recursive s(g) as (select 1200 union all select g - 1 from s where g > 1) select g, concat('n', g) from s"
                ),
            ],
            vec![format!("drop table shop.{name}")],
        ));
    }
    let backend = Backend::new(egui::Context::default());
    for (ds, schema, setup, teardown) in sources {
        let label = ds.driver.label();
        for sql in &setup {
            let (done, _) = run(&backend, req(41, &ds, &[sql], false));
            done[0].1.as_ref().unwrap_or_else(|error| panic!("{label}: {error}"));
        }
        let table = dbeast::backend::TablePage { schema, table: name.clone(), ordered: false };
        let first = table_page(&backend, &ds, &table, "", Page::first(500));
        assert_eq!(ids(&first), (1..=500).collect::<Vec<_>>(), "{label}");
        assert!(first.truncated, "{label}");
        let second = table_page(&backend, &ds, &table, "", Page { skip: 500, limit: 500 });
        assert_eq!(ids(&second), (501..=1000).collect::<Vec<_>>(), "{label}");
        assert!(second.truncated, "{label}");
        // The same page twice is the same rows.
        let again = table_page(&backend, &ds, &table, "", Page { skip: 500, limit: 500 });
        assert_eq!(again.rows, second.rows, "{label}");
        let exact = table_page(&backend, &ds, &table, "", Page { skip: 700, limit: 500 });
        assert_eq!((exact.rows.len(), exact.truncated), (500, false), "{label}");
        let last = table_page(&backend, &ds, &table, "", Page { skip: 1000, limit: 500 });
        assert_eq!(ids(&last), (1001..=1200).collect::<Vec<_>>(), "{label}");
        assert!(!last.truncated, "{label}");
        let past = table_page(&backend, &ds, &table, "", Page { skip: 1500, limit: 500 });
        assert_eq!((past.rows.len(), past.truncated), (0, false), "{label}");
        // The user's ORDER BY wins over the key.
        let user = table_page(&backend, &ds, &table, "note DESC", Page::first(3));
        assert_eq!(ids(&user), vec![999, 998, 997], "{label}");
        for sql in &teardown {
            let (done, _) = run(&backend, req(41, &ds, &[sql], false));
            assert!(done[0].1.is_ok(), "{label}: {:?}", done[0].1);
        }
    }
    std::fs::remove_dir_all(dir).ok();
}

/// A 100-row Submit whose 51st change conflicts with another session: the
/// failure names that row, and no change of the batch remains, also those
/// sent after the conflict. Then 1-row and 100-row batches succeed.
fn exercise_batch_conflict(ds: DataSource) {
    const ROWS: usize = 100;
    let backend = Backend::new(egui::Context::default());
    let table = format!("dbeast_batch_{}", uuid::Uuid::new_v4().simple());
    let schema = match ds.driver {
        DriverKind::Postgres => "public",
        DriverKind::MySql => "dbeast",
        DriverKind::Sqlite => "main",
    };
    let qualified = dbeast::sqlgen::qualified(ds.driver, Some(schema), &table);
    let sql = format!("SELECT id, name FROM {qualified} ORDER BY id");
    let default = ExecutionContext::default();
    let test = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let engine = if ds.driver == DriverKind::MySql { " ENGINE=InnoDB" } else { "" };
        query_context(&backend, 9300, &ds, &default, &format!("CREATE TABLE {qualified} (id INTEGER PRIMARY KEY, name TEXT NOT NULL){engine}"));
        let rows = (1..=ROWS).map(|id| format!("({id},'before')")).collect::<Vec<_>>().join(",");
        query_context(&backend, 9300, &ds, &default, &format!("INSERT INTO {qualified} VALUES {rows}"));
        let changes = |label: &str, count: usize| -> Vec<((usize, usize), CellEdit)> {
            (0..count).map(|row| ((row, 1), CellEdit::Text(format!("{label}_{row}")))).collect()
        };

        let (data, metadata) = editable_query(&backend, 9301, &ds, &sql);
        query_context(&backend, 9300, &ds, &default, &format!("UPDATE {qualified} SET name = 'elsewhere' WHERE id = 51"));
        let (result, in_tx) = apply_updates(&backend, mutation_request(9301, &ds, &data, &metadata, &changes("lost", ROWS), false));
        let failure = result.unwrap_err();
        assert_eq!(failure.source_row, Some(50), "{}", failure.message);
        assert!(failure.message.contains("matched 0 rows"), "{}", failure.message);
        assert!(!failure.outcome_unknown);
        assert!(!in_tx);
        let stored = query_context(&backend, 9300, &ds, &default, &sql);
        for (row, values) in stored.rows.iter().enumerate() {
            let expected = if row == 50 { "elsewhere" } else { "before" };
            assert_eq!(values[1], Value::Text(expected.into()), "row {row}");
        }

        for count in [1, ROWS] {
            let (data, metadata) = editable_query(&backend, 9301, &ds, &sql);
            let label = format!("saved{count}");
            let (result, in_tx) = apply_updates(&backend, mutation_request(9301, &ds, &data, &metadata, &changes(&label, count), false));
            assert_eq!(result.unwrap().changed_rows, count);
            assert!(!in_tx);
            let stored = query_context(&backend, 9300, &ds, &default, &sql);
            for row in 0..count {
                assert_eq!(stored.rows[row][1], Value::Text(format!("{label}_{row}")));
            }
        }
    }));
    backend.close_session(&ds.id, 9301);
    run(&backend, req(9300, &ds, &[&format!("DROP TABLE IF EXISTS {qualified}")], false));
    if let Err(panic) = test {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn sqlite_batch_conflict_rolls_back_the_whole_submit() {
    let directory = std::env::temp_dir().join(format!("dbeast_batch_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let mut sqlite = DataSource::new(DriverKind::Sqlite);
    sqlite.file = directory.join("batch.sqlite").to_string_lossy().into_owned();
    exercise_batch_conflict(sqlite);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn postgres_batch_conflict_rolls_back_the_whole_submit() {
    if let Some(ds) = pg() {
        exercise_batch_conflict(ds);
    }
}

#[test]
fn mysql_batch_conflict_rolls_back_the_whole_submit() {
    if let Some(ds) = mysql() {
        exercise_batch_conflict(ds);
    }
}

fn updates_outcome(backend: &Backend, console_id: u64) -> (Result<MutationReport, MutationFailure>, Option<bool>) {
    loop {
        if let Event::UpdatesDone { console_id: received, in_tx, result, .. } =
            backend.rx.recv_timeout(Duration::from_secs(15)).expect("updates event")
            && received == console_id
        {
            return (result, in_tx);
        }
    }
}

/// Stop or Disconnect while a Submit waits for a row lock held by another
/// session: the waiting UPDATE is cancelled, nothing of the batch remains,
/// the session is closed and the console can Submit again.
#[test]
fn stop_and_disconnect_end_a_submit_waiting_for_a_row_lock() {
    let Some(ds) = pg() else { return };
    let default = ExecutionContext::default();
    let table = format!("dbeast_stop_{}", uuid::Uuid::new_v4().simple());
    let sql = format!("SELECT id, name FROM {table} ORDER BY id");
    let backend = Backend::new(egui::Context::default());
    query_context(&backend, 9400, &ds, &default, &format!("CREATE TABLE {table} (id INTEGER PRIMARY KEY, name TEXT NOT NULL)"));
    query_context(&backend, 9400, &ds, &default, &format!("INSERT INTO {table} VALUES (1, 'a'), (2, 'b'), (3, 'c')"));
    let test = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        for disconnect in [false, true] {
            let (data, metadata) = editable_query(&backend, 9401, &ds, &sql);
            // Another console holds row 2 until it ends its transaction.
            let (_, in_tx) = run(&backend, req(9402, &ds, &[&format!("UPDATE {table} SET name = name WHERE id = 2")], true));
            assert!(in_tx);
            let changes: Vec<_> = (0..3).map(|row| ((row, 1), CellEdit::Text(format!("stopped_{row}")))).collect();
            backend.apply_updates(mutation_request(9401, &ds, &data, &metadata, &changes, false));
            std::thread::sleep(Duration::from_millis(300));
            let started = Instant::now();
            if disconnect {
                // Only the editing console's session is dropped by closing it.
                backend.close_session(&ds.id, 9401);
            } else {
                backend.cancel(9401);
            }
            let (result, in_tx) = updates_outcome(&backend, 9401);
            assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
            let failure = result.unwrap_err();
            assert!(!failure.outcome_unknown, "{}", failure.message);
            assert_eq!(in_tx, Some(false));
            run(&backend, req(9402, &ds, &["ROLLBACK"], false));
            let stored = query_context(&backend, 9400, &ds, &default, &sql);
            assert_eq!(stored.rows, data.rows, "no change of the stopped batch remains");
        }
        let (data, metadata) = editable_query(&backend, 9401, &ds, &sql);
        let (result, _) = apply_updates(&backend, mutation_request(9401, &ds, &data, &metadata, &[((0, 1), CellEdit::Text("after".into()))], false));
        assert_eq!(result.unwrap().changed_rows, 1);
    }));
    for console in [9401, 9402] {
        backend.close_session(&ds.id, console);
    }
    run(&backend, req(9400, &ds, &[&format!("DROP TABLE IF EXISTS {table}")], false));
    if let Err(panic) = test {
        std::panic::resume_unwind(panic);
    }
}

/// A Submit stopped on a connection that no longer answers is abandoned
/// within the recovery limit; it was not committed and the session is closed.
#[test]
fn stop_releases_a_submit_on_an_unresponsive_connection() {
    use std::io::{Read, Write};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let Some(original) = pg() else { return };
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut ds = original.clone();
    ds.id = uuid::Uuid::new_v4().to_string();
    ds.host = "127.0.0.1".into();
    ds.port = listener.local_addr().unwrap().port();
    ds.use_tls = false;
    let blackhole = Arc::new(AtomicBool::new(false));
    let cut = blackhole.clone();
    let peer = std::thread::spawn(move || {
        let (mut client, _) = listener.accept().unwrap();
        let mut server = std::net::TcpStream::connect("127.0.0.1:55432").unwrap();
        client.set_nonblocking(true).unwrap();
        server.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut buffer = [0; 65536];
        loop {
            assert!(Instant::now() < deadline, "proxy connection leaked");
            for incoming in [true, false] {
                let (from, to) = if incoming { (&mut client, &mut server) } else { (&mut server, &mut client) };
                match from.read(&mut buffer) {
                    Ok(0) => return,
                    Ok(n) if !cut.load(Ordering::Acquire) => to.write_all(&buffer[..n]).unwrap(),
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => return,
                    Err(error) => panic!("proxy: {error}"),
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    let default = ExecutionContext::default();
    let table = format!("dbeast_stop_{}", uuid::Uuid::new_v4().simple());
    let setup = Backend::new(egui::Context::default());
    query_context(&setup, 9500, &original, &default, &format!("CREATE TABLE {table} (id INTEGER PRIMARY KEY, name TEXT NOT NULL)"));
    query_context(&setup, 9500, &original, &default, &format!("INSERT INTO {table} VALUES (1, 'a')"));
    let backend = Backend::new(egui::Context::default());
    let sql = format!("SELECT id, name FROM {table} ORDER BY id");
    let (data, metadata) = editable_query(&backend, 9501, &ds, &sql);
    blackhole.store(true, Ordering::Release);
    backend.apply_updates(mutation_request(9501, &ds, &data, &metadata, &[((0, 1), CellEdit::Text("never".into()))], false));
    std::thread::sleep(Duration::from_millis(200));
    let started = Instant::now();
    backend.cancel(9501);
    let (result, in_tx) = updates_outcome(&backend, 9501);
    let elapsed = started.elapsed();
    let failure = result.unwrap_err();
    peer.join().unwrap();
    let stored = query_context(&setup, 9500, &original, &default, &sql);
    run(&setup, req(9500, &original, &[&format!("DROP TABLE IF EXISTS {table}")], false));
    assert!(elapsed < Duration::from_secs(7), "{elapsed:?}");
    assert!(failure.message.contains("did not respond"), "{}", failure.message);
    assert!(!failure.outcome_unknown);
    assert_eq!(in_tx, Some(false));
    assert_eq!(stored.rows, data.rows);
}
