//! Related Rows retain typed keys and session guards across independent consoles.

use dbeast::backend::{
    Backend, BoundRunRequest, Event, RelatedMetadataRequest, RelatedSourceGuard, RunRequest,
};
use dbeast::drivers::Connection;
use dbeast::export::{ExportOptions, TableName};
use dbeast::model::{
    BoundUpdate, CapturedRelatedQuery, DataSource, DriverKind, ExecutionContext, ForeignKey, Page,
    QueryResult, RelatedDirection, RelatedMetadata, Value,
};
use dbeast::{related, sqlgen};
use futures_util::FutureExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

const KEY: &str = "A'\\Żółw; --";
const SOURCE: u64 = 701;
const TARGET: u64 = 702;
type Statements = Vec<(String, Result<QueryResult, String>)>;

struct LocalDatabase {
    directory: PathBuf,
    source: DataSource,
}

impl LocalDatabase {
    fn new() -> Self {
        let directory =
            std::env::temp_dir().join(format!("dbeast_related_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let mut source = DataSource::new(DriverKind::Sqlite);
        source.file = directory
            .join("relations.sqlite")
            .to_string_lossy()
            .into_owned();
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
        eprintln!("skipping {driver:?} Related Rows: TCP probe failed: {error}");
        return None;
    }
    let mut ds = DataSource::new(driver);
    ds.host = "127.0.0.1".into();
    ds.port = port;
    ds.database = "dbeast".into();
    ds.user = "dbeast".into();
    ds.password = "dbeast".into();
    Some(ds)
}

fn receive(backend: &Backend, deadline: Instant) -> Event {
    backend
        .rx
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .expect("Related Rows backend event before deadline")
}

fn finish(backend: &Backend, id: u64) -> (Statements, bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut statements = Vec::new();
    loop {
        match receive(backend, deadline) {
            Event::StatementDone {
                console_id,
                sql,
                result,
                ..
            } if console_id == id => {
                statements.push((sql, result));
            }
            Event::RunFinished {
                console_id, in_tx, ..
            } if console_id == id => {
                return (statements, in_tx);
            }
            _ => {}
        }
    }
}

fn query(backend: &Backend, id: u64, ds: &DataSource, sql: &str) -> QueryResult {
    backend.run(RunRequest {
        console_id: id,
        ds: ds.clone(),
        statements: vec![sql.into()],
        page: Page::ALL,
        manual_tx: false,
        context: ExecutionContext::default(),
        expected_context: None,
    });
    let (mut statements, _) = finish(backend, id);
    assert_eq!(statements.len(), 1, "one requested statement: {sql}");
    assert_eq!(statements[0].0, sql);
    statements.remove(0).1.unwrap()
}

fn metadata_request(
    ds: &DataSource,
    data: &QueryResult,
    direction: RelatedDirection,
) -> RelatedMetadataRequest {
    RelatedMetadataRequest {
        console_id: SOURCE,
        result_id: 9101,
        request_id: 9102,
        ds: ds.clone(),
        source_identity: ds.identity(),
        expected_context: data.context.clone().unwrap(),
        source: data.source.clone().unwrap(),
        direction,
    }
}

fn metadata(backend: &Backend, request: RelatedMetadataRequest) -> Result<RelatedMetadata, String> {
    let expected = (
        request.console_id,
        request.result_id,
        request.request_id,
        request.direction,
    );
    backend.load_related_metadata(request);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Event::RelatedMetadata {
            console_id,
            result_id,
            request_id,
            direction,
            result,
        } = receive(backend, deadline)
        {
            assert_eq!((console_id, result_id, request_id, direction), expected);
            return result;
        }
    }
}

fn capture(
    ds: &DataSource,
    data: &QueryResult,
    row: usize,
    key: &ForeignKey,
    direction: RelatedDirection,
) -> CapturedRelatedQuery {
    related::capture(
        data,
        row,
        key,
        direction,
        ds.identity(),
        data.context.clone().unwrap(),
    )
    .unwrap()
}

fn bound(
    backend: &Backend,
    ds: &DataSource,
    captured: &CapturedRelatedQuery,
    page: Page,
    source: Option<&QueryResult>,
) -> Result<QueryResult, String> {
    backend.run_bound(BoundRunRequest {
        console_id: TARGET,
        ds: ds.clone(),
        query: Arc::new(captured.clone()),
        page,
        manual_tx: false,
        source_guard: source.map(|data| RelatedSourceGuard {
            console_id: SOURCE,
            session_epoch: data.source.as_ref().unwrap().session_epoch,
        }),
    });
    let (mut statements, in_tx) = finish(backend, TARGET);
    assert!(!in_tx, "navigation does not open a transaction");
    assert_eq!(statements.len(), 1, "bound execution reports one statement");
    assert!(
        !statements[0].0.contains(KEY),
        "bound parameter must not leak into logged SQL"
    );
    statements.remove(0).1
}

fn explain_related_at(console_id: u64, backend: &Backend, ds: &DataSource, captured: &CapturedRelatedQuery, mode: dbeast::query_plan::ExplainMode) -> (Result<dbeast::query_plan::QueryPlan, String>, dbeast::backend::ExplainCompletion) {
    backend.explain(dbeast::backend::ExplainRequest {
        console_id, request_id: 999, ds: ds.clone(),
        // Must never become executable: the captured structure is authoritative.
        sql: "DELETE FROM must_never_execute".into(),
        bound_query: Some(Arc::new(captured.clone())),
        context: ExecutionContext::default(), mode, manual_tx: false,
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Event::ExplainDone { request_id: 999, result, completion, .. } = receive(backend, deadline) {
            return (result, completion);
        }
    }
}

fn explain_related(backend: &Backend, ds: &DataSource, captured: &CapturedRelatedQuery, mode: dbeast::query_plan::ExplainMode) -> (Result<dbeast::query_plan::QueryPlan, String>, dbeast::backend::ExplainCompletion) {
    explain_related_at(TARGET, backend, ds, captured, mode)
}

fn check_related_plans(backend: &Backend, ds: &DataSource, captured: &CapturedRelatedQuery, expected_rows: &str) {
    use dbeast::query_plan::ExplainMode;
    use dbeast::backend::ExplainExecution;
    let generated = sqlgen::related_query(ds.driver, captured).unwrap();
    for mode in [ExplainMode::Plan, ExplainMode::Analyse] {
        let (result, completion) = explain_related(backend, ds, captured, mode);
        if mode == ExplainMode::Analyse && ds.driver == DriverKind::Sqlite {
            assert!(result.unwrap_err().contains("PostgreSQL"));
            assert_eq!(completion.execution, ExplainExecution::NotStarted);
            continue;
        }
        let plan = result.unwrap();
        assert_eq!(plan.sql, generated.sql);
        assert!(!plan.nodes.is_empty());
        if mode == ExplainMode::Analyse {
            assert_eq!(completion.execution, ExplainExecution::Succeeded);
            assert_eq!(plan.analysis.unwrap().nodes[0].rows.as_deref(), Some(expected_rows));
        }
    }
    if ds.driver == DriverKind::Postgres {
        let mut invalid_value = captured.clone();
        invalid_value.values[0] = Value::Text("client-side\0failure".into());
        let (result, completion) = explain_related(backend, ds, &invalid_value, ExplainMode::Analyse);
        assert!(result.unwrap_err().contains("cannot contain NUL"));
        assert_eq!(completion.execution, ExplainExecution::NotStarted);
        assert!(!completion.session_closed);
        // Local preflight failure leaves the same session healthy.
        explain_related(backend, ds, captured, ExplainMode::Analyse).0.unwrap();
    }
    let mut invalid = captured.clone();
    invalid.source_identity.host.push_str(".changed");
    let (result, completion) = explain_related(backend, ds, &invalid, ExplainMode::Plan);
    assert!(result.unwrap_err().contains("definition has changed"));
    assert_eq!(completion.in_tx, None);
    invalid = captured.clone();
    invalid.values.pop();
    assert!(explain_related(backend, ds, &invalid, ExplainMode::Plan).0.unwrap_err().contains("incomplete key"));
    invalid = captured.clone();
    invalid.context.schemas.push("changed_context".into());
    let (result, completion) = explain_related(backend, ds, &invalid, ExplainMode::Plan);
    assert!(result.unwrap_err().contains("schema or database has changed"));
    assert_eq!(completion.execution, ExplainExecution::NotStarted);
    assert!(!completion.session_closed);
}

fn texts(data: &QueryResult, column: &str) -> Vec<String> {
    let index = data.columns.iter().position(|c| c.name == column).unwrap();
    data.rows.iter().map(|row| row[index].to_string()).collect()
}

async fn seed_row(conn: &mut Connection, driver: DriverKind, table: &str, params: Vec<Value>) {
    let placeholders = (1..=params.len())
        .map(|i| {
            if driver == DriverKind::Postgres {
                format!("${i}")
            } else {
                "?".into()
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    let inserted = conn
        .execute_bound_update(&BoundUpdate {
            sql: format!("INSERT INTO {table} VALUES ({placeholders})"),
            params,
            source_row: 0,
        })
        .await
        .unwrap();
    assert_eq!(inserted, 1);
}

fn exercise_composite_navigation(ds: DataSource) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let backend = Backend::new(egui::Context::default());
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let parent_name = format!("rel_{}_parent Ż", &suffix[..10]);
    let child_name = format!("rel_{}_child Ż", &suffix[..10]);
    let schema = match ds.driver {
        DriverKind::Postgres => "public",
        DriverKind::MySql => "dbeast",
        DriverKind::Sqlite => "main",
    };
    let parent = sqlgen::qualified(ds.driver, Some(schema), &parent_name);
    let parent_reference = if ds.driver == DriverKind::Sqlite {
        sqlgen::ident(ds.driver, &parent_name)
    } else {
        parent.clone()
    };
    let child = sqlgen::qualified(ds.driver, Some(schema), &child_name);
    let key_name = format!("rel_{}_key", &suffix[..10]);
    let key_ident = sqlgen::ident(ds.driver, &key_name);
    let engine = if ds.driver == DriverKind::MySql {
        " ENGINE=InnoDB"
    } else {
        ""
    };
    runtime.block_on(async {
        let mut connection = Connection::open(&ds).await.unwrap();
        for sql in [
            format!("CREATE TABLE {parent} (tenant INTEGER NOT NULL, code VARCHAR(100) NOT NULL, label VARCHAR(100), PRIMARY KEY(tenant,code), UNIQUE(code,tenant)){engine}"),
            format!("CREATE TABLE {child} (id INTEGER PRIMARY KEY, tenant INTEGER NOT NULL, account_code VARCHAR(100), name VARCHAR(100), CONSTRAINT {key_ident} FOREIGN KEY(account_code,tenant) REFERENCES {parent_reference}(code,tenant)){engine}"),
        ] { connection.execute(&sql, Page::ALL).await.unwrap(); }
        for (tenant, code, label) in [(7, KEY, "North"), (8, KEY, "Other tenant"), (7, "B", "South")] {
            seed_row(&mut connection, ds.driver, &parent, vec![Value::Int(tenant), Value::Text(code.into()), Value::Text(label.into())]).await;
        }
        for (id, tenant, code, label) in [(101,7,Some(KEY),"Ada"), (102,7,Some(KEY),"Olek"), (103,8,Some(KEY),"Sentinel"), (104,7,None,"No account")] {
            seed_row(&mut connection, ds.driver, &child, vec![Value::Int(id), Value::Int(tenant), code.map(|s| Value::Text(s.into())).unwrap_or(Value::Null), Value::Text(label.into())]).await;
        }
    });
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // No child PK is projected: UPDATE authority is irrelevant to navigation.
        let sql = format!(
            "SELECT account_code AS account_ref,name,tenant AS tenant_ref FROM {child} ORDER BY id"
        );
        let data = query(&backend, SOURCE, &ds, &sql);
        let outgoing = metadata(
            &backend,
            metadata_request(&ds, &data, RelatedDirection::Referenced),
        )
        .unwrap();
        assert_eq!(outgoing.keys.len(), 1);
        let key = &outgoing.keys[0];
        assert_eq!(key.source.name, child_name);
        assert_eq!(key.target.name, parent_name);
        assert_eq!(
            key.columns
                .iter()
                .map(|c| (c.source_name.as_str(), c.target_name.as_str()))
                .collect::<Vec<_>>(),
            [("account_code", "code"), ("tenant", "tenant")]
        );
        assert_eq!(
            related::columns(&data, key, RelatedDirection::Referenced).unwrap(),
            [0, 2]
        );
        let captured = capture(&ds, &data, 0, key, RelatedDirection::Referenced);
        assert_eq!(captured.values, [Value::Text(KEY.into()), Value::Int(7)]);
        let generated = sqlgen::related_query(ds.driver, &captured).unwrap();
        assert!(!generated.sql.contains(KEY));
        assert_eq!(generated.params, captured.values);
        assert_eq!(
            texts(
                &bound(&backend, &ds, &captured, Page::ALL, Some(&data)).unwrap(),
                "label"
            ),
            ["North"]
        );
        check_related_plans(&backend, &ds, &captured, "1");
        // A restored console has no live session; canonical query and context suffice.
        explain_related_at(703, &backend, &ds, &captured, dbeast::query_plan::ExplainMode::Plan).0.unwrap();
        // Planning is a SQL snapshot operation, not FK catalog revalidation.
        let mut catalog_snapshot = captured.clone();
        catalog_snapshot.relation.catalog_id = "no-longer-in-catalog".into();
        explain_related(&backend, &ds, &catalog_snapshot, dbeast::query_plan::ExplainMode::Plan).0.unwrap();
        assert!(
            related::capture(
                &data,
                3,
                key,
                RelatedDirection::Referenced,
                ds.identity(),
                data.context.clone().unwrap()
            )
            .is_err(),
            "NULL FK has no referenced row"
        );

        let partial = query(
            &backend,
            SOURCE,
            &ds,
            &format!("SELECT account_code FROM {child} ORDER BY id"),
        );
        assert!(
            related::columns(&partial, key, RelatedDirection::Referenced).is_err(),
            "all composite fields are required"
        );
        let duplicate = query(
            &backend,
            SOURCE,
            &ds,
            &format!(
                "SELECT account_code,account_code AS duplicate,tenant FROM {child} ORDER BY id"
            ),
        );
        assert!(
            related::columns(&duplicate, key, RelatedDirection::Referenced).is_err(),
            "duplicate origin is not guessed"
        );

        // Referencing rows use the same FK order even though the PK is reversed.
        let parent_data = query(
            &backend,
            SOURCE,
            &ds,
            &format!(
                "SELECT label,code AS account_ref,tenant FROM {parent} WHERE tenant=7 AND label='North'"
            ),
        );
        let incoming = metadata(
            &backend,
            metadata_request(&ds, &parent_data, RelatedDirection::Referencing),
        )
        .unwrap();
        assert_eq!(incoming.keys.len(), 1);
        let incoming = capture(
            &ds,
            &parent_data,
            0,
            &incoming.keys[0],
            RelatedDirection::Referencing,
        );
        check_related_plans(&backend, &ds, &incoming, "2");
        let first = bound(&backend, &ds, &incoming, Page::first(1), Some(&parent_data)).unwrap();
        assert_eq!(first.rows.len(), 1);
        assert!(first.truncated);
        let second = bound(&backend, &ds, &incoming, Page { skip: 1, limit: 1 }, None).unwrap();
        assert_eq!(second.rows.len(), 1);
        assert!(!second.truncated);
        let mut names = [
            texts(&first, "name")[0].clone(),
            texts(&second, "name")[0].clone(),
        ];
        names.sort();
        assert_eq!(names, ["Ada", "Olek"]);
        let reload = bound(&backend, &ds, &incoming, Page { skip: 1, limit: 1 }, None).unwrap();
        assert_eq!(
            reload.rows, second.rows,
            "reload retains captured values and page"
        );

        let restored: CapturedRelatedQuery =
            serde_json::from_str(&serde_json::to_string(&incoming).unwrap()).unwrap();
        backend.close_session(&ds.id, TARGET);
        assert_eq!(
            bound(&backend, &ds, &restored, Page::ALL, None)
                .unwrap()
                .rows
                .len(),
            2,
            "restored bound query establishes a fresh independent session"
        );

        let export_path = std::env::temp_dir().join(format!("dbeast_related_{suffix}.csv"));
        backend.export_related(
            TARGET,
            ds.clone(),
            Arc::new(restored.clone()),
            ExportOptions::default(),
            TableName::new("related"),
            export_path.clone(),
        );
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Event::Task { event: dbeast::backend::TaskEvent::Finished(result), .. } = receive(&backend, deadline) {
                assert!(result.unwrap().starts_with("2 rows exported"));
                break;
            }
        }
        let csv = std::fs::read_to_string(&export_path).unwrap();
        std::fs::remove_file(export_path).unwrap();
        assert!(csv.contains("Ada") && csv.contains("Olek"));
        assert!(
            !csv.contains("Sentinel"),
            "whole-result export keeps the relation filter"
        );

        let mut wrong_identity = incoming.clone();
        let mut different_source = ds.clone();
        different_source.file.push_str("-different");
        different_source.host.push_str("-different");
        wrong_identity.source_identity = different_source.identity();
        assert!(
            bound(
                &backend,
                &ds,
                &wrong_identity,
                Page::ALL,
                Some(&parent_data)
            )
            .is_err()
        );
        let mut wrong_context = incoming.clone();
        wrong_context
            .context
            .schemas
            .push("unrelated_namespace".into());
        assert!(bound(&backend, &ds, &wrong_context, Page::ALL, Some(&parent_data)).is_err());
        let mut stale_metadata = metadata_request(&ds, &parent_data, RelatedDirection::Referencing);
        stale_metadata.source.session_epoch += 1;
        assert!(metadata(&backend, stale_metadata).is_err());

        query(&backend, SOURCE, &ds, "BEGIN");
        assert!(
            metadata(
                &backend,
                metadata_request(&ds, &parent_data, RelatedDirection::Referencing)
            )
            .is_err()
        );
        assert!(
            bound(&backend, &ds, &incoming, Page::ALL, Some(&parent_data)).is_err(),
            "a separate console cannot read the source transaction"
        );
        query(&backend, SOURCE, &ds, "ROLLBACK");
        assert_eq!(
            bound(&backend, &ds, &incoming, Page::ALL, Some(&parent_data))
                .unwrap()
                .rows
                .len(),
            2
        );

        if ds.driver != DriverKind::Sqlite {
            let switch = if ds.driver == DriverKind::Postgres {
                "SET search_path TO shop"
            } else {
                "USE shop"
            };
            query(&backend, SOURCE, &ds, switch);
            assert!(
                bound(&backend, &ds, &incoming, Page::ALL, Some(&parent_data)).is_err(),
                "manual namespace changes are not silently restored"
            );
            let current = if ds.driver == DriverKind::Postgres {
                "SELECT current_schema() AS current_name"
            } else {
                "SELECT DATABASE() AS current_name"
            };
            assert_eq!(
                texts(&query(&backend, SOURCE, &ds, current), "current_name"),
                ["shop"]
            );
            query(
                &backend,
                SOURCE,
                &ds,
                if ds.driver == DriverKind::Postgres {
                    "SET search_path TO public"
                } else {
                    "USE dbeast"
                },
            );
        }
        backend.close_session(&ds.id, SOURCE);
        assert!(
            metadata(
                &backend,
                metadata_request(&ds, &parent_data, RelatedDirection::Referencing)
            )
            .is_err(),
            "metadata must not recreate the source session"
        );
        assert!(bound(&backend, &ds, &incoming, Page::ALL, Some(&parent_data)).is_err());
        let fresh = query(&backend, SOURCE, &ds, &sql);
        assert_ne!(
            fresh.source.as_ref().unwrap().session_epoch,
            parent_data.source.as_ref().unwrap().session_epoch
        );
        assert!(
            bound(&backend, &ds, &incoming, Page::ALL, Some(&parent_data)).is_err(),
            "old epoch cannot borrow the new connection"
        );

        match ds.driver {
            DriverKind::Postgres => {
                query(
                    &backend,
                    SOURCE,
                    &ds,
                    "CREATE TEMP TABLE related_temp_parent(id INTEGER PRIMARY KEY)",
                );
                query(
                    &backend,
                    SOURCE,
                    &ds,
                    "CREATE TEMP TABLE related_temp_child(id INTEGER REFERENCES related_temp_parent(id))",
                );
                query(
                    &backend,
                    SOURCE,
                    &ds,
                    "INSERT INTO related_temp_parent VALUES(1)",
                );
                query(
                    &backend,
                    SOURCE,
                    &ds,
                    "INSERT INTO related_temp_child VALUES(1)",
                );
                let temporary = query(&backend, SOURCE, &ds, "SELECT * FROM related_temp_child");
                assert!(
                    metadata(
                        &backend,
                        metadata_request(&ds, &temporary, RelatedDirection::Referenced)
                    )
                    .is_err(),
                    "temporary relations cannot be navigated from a new session"
                );
            }
            DriverKind::MySql => {
                // The protocol reports the permanent table's name for its temp shadow too.
                query(
                    &backend,
                    SOURCE,
                    &ds,
                    &format!(
                        "CREATE TEMPORARY TABLE {child} (id INTEGER,tenant INTEGER,account_code VARCHAR(100),name VARCHAR(100))"
                    ),
                );
                query(
                    &backend,
                    SOURCE,
                    &ds,
                    &format!("INSERT INTO {child} VALUES(900,7,'B','temporary sentinel')"),
                );
                let temporary = query(&backend, SOURCE, &ds, &format!("SELECT * FROM {child}"));
                assert!(
                    metadata(
                        &backend,
                        metadata_request(&ds, &temporary, RelatedDirection::Referenced)
                    )
                    .is_err(),
                    "temp shadow must not borrow permanent FK metadata"
                );
                query(
                    &backend,
                    SOURCE,
                    &ds,
                    &format!("DROP TEMPORARY TABLE {child}"),
                );
            }
            DriverKind::Sqlite => {}
        }

        // Replayed/restored payloads are not authority after the FK is removed.
        let remove = match ds.driver {
            DriverKind::Postgres => format!("ALTER TABLE {child} DROP CONSTRAINT {key_ident}"),
            DriverKind::MySql => format!("ALTER TABLE {child} DROP FOREIGN KEY {key_ident}"),
            DriverKind::Sqlite => format!("DROP TABLE {child}"),
        };
        query(&backend, SOURCE, &ds, &remove);
        if ds.driver == DriverKind::Sqlite {
            query(
                &backend,
                SOURCE,
                &ds,
                &format!(
                    "CREATE TABLE {child} (id INTEGER PRIMARY KEY, tenant INTEGER, account_code VARCHAR(100), name VARCHAR(100))"
                ),
            );
        }
        assert!(
            bound(&backend, &ds, &captured, Page::ALL, None).is_err(),
            "missing FK cannot fall back to a raw SQL filter"
        );
    }));
    drop(backend);
    runtime.block_on(async {
        let mut connection = Connection::open(&ds).await.unwrap();
        for table in [&child, &parent] {
            connection
                .execute(&format!("DROP TABLE IF EXISTS {table}"), Page::ALL)
                .await
                .unwrap();
        }
    });
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn sqlite_composite_navigation_retains_values_pages_and_guards() {
    let local = LocalDatabase::new();
    exercise_composite_navigation(local.source.clone());
}

#[test]
fn postgres_composite_navigation_retains_values_pages_and_guards() {
    if let Some(ds) = server(DriverKind::Postgres) {
        exercise_composite_navigation(ds);
    }
}

#[test]
fn mysql_composite_navigation_retains_values_pages_and_guards() {
    if let Some(ds) = server(DriverKind::MySql) {
        exercise_composite_navigation(ds);
    }
}

#[test]
fn sqlite_incoming_preserves_parent_collation_affinity_and_implicit_primary_key() {
    let local = LocalDatabase::new();
    let db = rusqlite::Connection::open(&local.source.file).unwrap();
    db.execute_batch(
        "PRAGMA foreign_keys=ON;
         CREATE TABLE codes(code TEXT COLLATE NOCASE PRIMARY KEY,label TEXT);
         CREATE TABLE code_links(id INTEGER PRIMARY KEY,code TEXT COLLATE BINARY REFERENCES codes(code));
         INSERT INTO codes VALUES('AbC','case parent'); INSERT INTO code_links VALUES(1,'abc');
         CREATE TABLE numbered(id INTEGER PRIMARY KEY,label TEXT);
         CREATE TABLE number_links(id INTEGER PRIMARY KEY,parent_id TEXT REFERENCES numbered);
         INSERT INTO numbered VALUES(7,'numeric parent'); INSERT INTO number_links VALUES(2,'007');
         CREATE TABLE accounts(tenant INTEGER,code TEXT,PRIMARY KEY(tenant,code));
         CREATE TABLE bills(id INTEGER PRIMARY KEY,tenant INTEGER,code TEXT,FOREIGN KEY(tenant,code) REFERENCES accounts);
         INSERT INTO accounts VALUES(9,'implicit'); INSERT INTO bills VALUES(4,9,'implicit');
         PRAGMA foreign_keys=OFF;
         CREATE TABLE text_numbers(code TEXT PRIMARY KEY);
         CREATE TABLE integer_links(id INTEGER PRIMARY KEY,parent_code INTEGER REFERENCES text_numbers(code));
         INSERT INTO text_numbers VALUES('001'); INSERT INTO integer_links VALUES(3,1);"
    ).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap(),
        1
    );
    let backend = Backend::new(egui::Context::default());
    for (parent, child, expected) in [
        ("codes", "code_links", 1),
        ("numbered", "number_links", 1),
        ("accounts", "bills", 1),
        ("text_numbers", "integer_links", 0),
    ] {
        let data = query(
            &backend,
            SOURCE,
            &local.source,
            &format!("SELECT * FROM {parent}"),
        );
        let keys = metadata(
            &backend,
            metadata_request(&local.source, &data, RelatedDirection::Referencing),
        )
        .unwrap();
        assert_eq!(keys.keys.len(), 1);
        assert_eq!(keys.keys[0].source.name, child);
        let captured = capture(
            &local.source,
            &data,
            0,
            &keys.keys[0],
            RelatedDirection::Referencing,
        );
        let result = bound(&backend, &local.source, &captured, Page::ALL, Some(&data)).unwrap();
        assert_eq!(
            result.rows.len(),
            expected,
            "SQLite FK semantics for {parent} -> {child}"
        );
        if parent == "accounts" {
            assert_eq!(
                keys.keys[0]
                    .columns
                    .iter()
                    .map(|c| c.target_name.as_str())
                    .collect::<Vec<_>>(),
                ["tenant", "code"]
            );
        }
    }
    let invalid_child = query(
        &backend,
        SOURCE,
        &local.source,
        "SELECT * FROM integer_links",
    );
    let keys = metadata(
        &backend,
        metadata_request(&local.source, &invalid_child, RelatedDirection::Referenced),
    )
    .unwrap();
    let captured = capture(
        &local.source,
        &invalid_child,
        0,
        &keys.keys[0],
        RelatedDirection::Referenced,
    );
    assert!(
        bound(
            &backend,
            &local.source,
            &captured,
            Page::ALL,
            Some(&invalid_child)
        )
        .unwrap()
        .rows
        .is_empty()
    );
}

#[test]
fn sqlite_session_local_namespaces_cannot_escape_to_an_independent_console() {
    let local = LocalDatabase::new();
    let backend = Backend::new(egui::Context::default());
    for sql in [
        "CREATE TABLE main.parent(id INTEGER PRIMARY KEY)",
        "CREATE TABLE main.child(id INTEGER REFERENCES parent(id))",
        "INSERT INTO main.parent VALUES(1)",
        "INSERT INTO main.child VALUES(1)",
    ] {
        query(&backend, SOURCE, &local.source, sql);
    }
    for prefix in ["temp", "attached"] {
        if prefix == "attached" {
            let attached = local.directory.join("attached.sqlite");
            query(
                &backend,
                SOURCE,
                &local.source,
                &format!(
                    "ATTACH DATABASE {} AS attached",
                    sqlgen::text_literal(DriverKind::Sqlite, &attached.to_string_lossy())
                ),
            );
        }
        query(
            &backend,
            SOURCE,
            &local.source,
            &format!("CREATE TABLE {prefix}.parent(id INTEGER PRIMARY KEY)"),
        );
        query(
            &backend,
            SOURCE,
            &local.source,
            &format!("CREATE TABLE {prefix}.child(id INTEGER REFERENCES parent(id))"),
        );
        query(
            &backend,
            SOURCE,
            &local.source,
            &format!("INSERT INTO {prefix}.parent VALUES(1)"),
        );
        query(
            &backend,
            SOURCE,
            &local.source,
            &format!("INSERT INTO {prefix}.child VALUES(1)"),
        );
        let data = query(
            &backend,
            SOURCE,
            &local.source,
            &format!("SELECT * FROM {prefix}.child"),
        );
        assert!(
            metadata(
                &backend,
                metadata_request(&local.source, &data, RelatedDirection::Referenced)
            )
            .is_err(),
            "{prefix} cannot be replayed on a separate connection"
        );
        assert_eq!(
            query(
                &backend,
                SOURCE,
                &local.source,
                &format!("SELECT * FROM {prefix}.child")
            )
            .rows,
            data.rows,
            "refusal preserves the original session"
        );
        if prefix == "attached" {
            let main = query(&backend, SOURCE, &local.source, "SELECT * FROM main.child");
            assert!(
                metadata(
                    &backend,
                    metadata_request(&local.source, &main, RelatedDirection::Referenced)
                )
                .is_err(),
                "even main cannot carry a session's active attachments into a new console"
            );
        }
    }
    drop(backend);

    let mut memory = DataSource::new(DriverKind::Sqlite);
    memory.file = ":memory:".into();
    let backend = Backend::new(egui::Context::default());
    for sql in [
        "CREATE TABLE parent(id INTEGER PRIMARY KEY)",
        "CREATE TABLE child(id INTEGER REFERENCES parent(id))",
        "INSERT INTO parent VALUES(1)",
        "INSERT INTO child VALUES(1)",
    ] {
        query(&backend, SOURCE, &memory, sql);
    }
    let data = query(&backend, SOURCE, &memory, "SELECT * FROM child");
    assert!(
        metadata(
            &backend,
            metadata_request(&memory, &data, RelatedDirection::Referenced)
        )
        .is_err()
    );
    let restored: QueryResult =
        serde_json::from_str(&serde_json::to_string(&data).unwrap()).unwrap();
    assert!(restored.source.is_none());
    assert!(
        related::candidate(&restored).is_err(),
        "saved rows do not restore runtime authority"
    );
}

#[tokio::test]
async fn mysql_bound_rows_decode_typed_values_but_timestamp_keys_are_refused() {
    let Some(ds) = server(DriverKind::MySql) else {
        return;
    };
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let names: Vec<_> = [
        "datetime_parent",
        "datetime_child",
        "timestamp_parent",
        "timestamp_child",
    ]
    .map(|name| format!("related_{}_{name}", &suffix[..10]))
    .into();
    let tables: Vec<_> = names
        .iter()
        .map(|name| sqlgen::qualified(ds.driver, Some("dbeast"), name))
        .collect();
    let mut conn = Connection::open(&ds).await.unwrap();
    let outcome = std::panic::AssertUnwindSafe(async {
        for sql in [
            format!("CREATE TABLE {} (stamp DATETIME PRIMARY KEY,label VARCHAR(80),large_value BIGINT UNSIGNED,amount DECIMAL(20,4),born DATE,duration TIME) ENGINE=InnoDB", tables[0]),
            format!("CREATE TABLE {} (id INTEGER PRIMARY KEY,stamp DATETIME,FOREIGN KEY(stamp) REFERENCES {}(stamp)) ENGINE=InnoDB", tables[1],tables[0]),
            format!("CREATE TABLE {} (stamp TIMESTAMP PRIMARY KEY,label VARCHAR(80)) ENGINE=InnoDB", tables[2]),
            format!("CREATE TABLE {} (id INTEGER PRIMARY KEY,stamp TIMESTAMP,FOREIGN KEY(stamp) REFERENCES {}(stamp)) ENGINE=InnoDB", tables[3],tables[2]),
        ] { conn.execute(&sql, Page::ALL).await.unwrap(); }
        let stamp = Value::Text("2024-01-02 03:04:05".into());
        seed_row(&mut conn, ds.driver, &tables[0], vec![stamp.clone(),Value::Text("typed parent".into()),Value::Decimal(u64::MAX.to_string()),Value::Decimal("3.2500".into()),Value::Text("2024-01-02".into()),Value::Text("03:04:05".into())]).await;
        seed_row(&mut conn, ds.driver, &tables[1], vec![Value::Int(1),stamp.clone()]).await;
        seed_row(&mut conn, ds.driver, &tables[2], vec![stamp.clone(),Value::Text("timestamp parent".into())]).await;
        seed_row(&mut conn, ds.driver, &tables[3], vec![Value::Int(2),stamp]).await;
        let child = conn.execute_with_context(&format!("SELECT stamp AS aliased_stamp FROM {}",tables[1]),Page::ALL,None).await.unwrap();
        let keys = conn.foreign_keys(related::candidate(&child).unwrap(),RelatedDirection::Referenced).await.unwrap();
        assert_eq!(keys.keys.len(),1);
        let query = capture(&ds,&child,0,&keys.keys[0],RelatedDirection::Referenced);
        let generated = sqlgen::related_query(ds.driver,&query).unwrap();
        let mut independent = Connection::open(&ds).await.unwrap();
        let result = independent.execute_bound_query(&generated,Page::ALL,Some(&query.context)).await.unwrap();
        assert_eq!(result.rows,[vec![
            Value::Text("2024-01-02 03:04:05".into()),Value::Text("typed parent".into()),
            Value::Decimal(u64::MAX.to_string()),Value::Decimal("3.2500".into()),
            Value::Text("2024-01-02".into()),Value::Text("03:04:05".into()),
        ]],"prepared SELECT decodes MySQL's binary protocol without SQL-quoted text or unsigned rounding");

        conn.execute("SET time_zone='+02:00'",Page::ALL).await.unwrap();
        for (table,direction) in [(&tables[3],RelatedDirection::Referenced),(&tables[2],RelatedDirection::Referencing)] {
            let data = conn.execute_with_context(&format!("SELECT * FROM {table}"),Page::ALL,None).await.unwrap();
            let keys = conn.foreign_keys(related::candidate(&data).unwrap(),direction).await.unwrap();
            assert_eq!(keys.keys.len(),1);
            let error = related::capture(&data,0,&keys.keys[0],direction,ds.identity(),data.context.clone().unwrap()).unwrap_err();
            assert!(error.contains("TIMESTAMP"),"the session-time-zone limit must be explicit: {error}");
        }
    }).catch_unwind().await;
    for index in [3, 2, 1, 0] {
        conn.execute(
            &format!("DROP TABLE IF EXISTS {}", tables[index]),
            Page::ALL,
        )
        .await
        .unwrap();
    }
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

fn report_related_timings(driver: DriverKind, phase: &str, mut samples: Vec<Duration>) {
    assert_eq!(samples.len(), 7);
    samples.sort_unstable();
    println!(
        "related_rows driver={driver:?} phase={phase} samples=7 median_ms={:.3} p95_ms={:.3}",
        samples[3].as_secs_f64() * 1000.0,
        samples[6].as_secs_f64() * 1000.0,
    );
}

fn benchmark_related_requests(ds: DataSource) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let backend = Backend::new(egui::Context::default());
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let schema = match ds.driver {
        DriverKind::Postgres => "public",
        DriverKind::MySql => "dbeast",
        DriverKind::Sqlite => "main",
    };
    let parent_name = format!("related_bench_{}_parent", &suffix[..10]);
    let parent = sqlgen::qualified(ds.driver, Some(schema), &parent_name);
    let parent_reference = if ds.driver == DriverKind::Sqlite {
        sqlgen::ident(ds.driver, &parent_name)
    } else {
        parent.clone()
    };
    let child = sqlgen::qualified(
        ds.driver,
        Some(schema),
        &format!("related_bench_{}_child", &suffix[..10]),
    );
    let engine = if ds.driver == DriverKind::MySql {
        " ENGINE=InnoDB"
    } else {
        ""
    };
    runtime.block_on(async {
        let mut conn = Connection::open(&ds).await.unwrap();
        for sql in [
            format!("CREATE TABLE {parent}(id INTEGER PRIMARY KEY,label VARCHAR(80)){engine}"),
            format!("CREATE TABLE {child}(id INTEGER PRIMARY KEY,parent_id INTEGER,FOREIGN KEY(parent_id) REFERENCES {parent_reference}(id)){engine}"),
        ] { conn.execute(&sql, Page::ALL).await.unwrap(); }
        conn.execute("BEGIN", Page::ALL).await.unwrap();
        seed_row(&mut conn, ds.driver, &parent, vec![Value::Int(1),Value::Text("benchmark parent".into())]).await;
        for id in 1..=500 {
            seed_row(&mut conn, ds.driver, &child, vec![Value::Int(id),Value::Int(1)]).await;
        }
        conn.execute("COMMIT", Page::ALL).await.unwrap();
    });
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let child_sql = format!("SELECT parent_id FROM {child} WHERE id=1");
        let parent_sql = format!("SELECT id FROM {parent} WHERE id=1");
        // The unrecorded first iteration warms driver/runtime/OS paths. Every cold
        // sample still has a new connection epoch and no FK metadata cache entry.
        for (direction, sql, label) in [
            (RelatedDirection::Referenced, &child_sql, "outgoing"),
            (RelatedDirection::Referencing, &parent_sql, "incoming"),
        ] {
            let mut cold = Vec::with_capacity(7);
            for sample in 0..8 {
                backend.close_session(&ds.id, SOURCE);
                let data = query(&backend, SOURCE, &ds, sql);
                let request = metadata_request(&ds, &data, direction);
                let started = Instant::now();
                let result = metadata(&backend, request);
                let elapsed = started.elapsed();
                assert_eq!(result.unwrap().keys.len(), 1);
                if sample != 0 {
                    cold.push(elapsed);
                }
            }
            report_related_timings(ds.driver, &format!("metadata_cold_{label}"), cold);
            let data = query(&backend, SOURCE, &ds, sql);
            let mut warm = Vec::with_capacity(7);
            for sample in 0..8 {
                let request = metadata_request(&ds, &data, direction);
                let started = Instant::now();
                let result = metadata(&backend, request);
                let elapsed = started.elapsed();
                assert_eq!(result.unwrap().keys.len(), 1);
                if sample != 0 {
                    warm.push(elapsed);
                }
            }
            report_related_timings(ds.driver, &format!("metadata_warm_{label}"), warm);
        }

        let child_data = query(&backend, SOURCE, &ds, &child_sql);
        let outgoing = metadata(
            &backend,
            metadata_request(&ds, &child_data, RelatedDirection::Referenced),
        )
        .unwrap();
        let outgoing = capture(
            &ds,
            &child_data,
            0,
            &outgoing.keys[0],
            RelatedDirection::Referenced,
        );
        let parent_data = query(&backend, SOURCE, &ds, &parent_sql);
        let incoming = metadata(
            &backend,
            metadata_request(&ds, &parent_data, RelatedDirection::Referencing),
        )
        .unwrap();
        let incoming = capture(
            &ds,
            &parent_data,
            0,
            &incoming.keys[0],
            RelatedDirection::Referencing,
        );
        for (captured, source, expected_rows, label) in [
            (&outgoing, &child_data, 1, "bound_outgoing_1_row"),
            (&incoming, &parent_data, 500, "bound_incoming_500_rows"),
        ] {
            let mut samples = Vec::with_capacity(7);
            for sample in 0..8 {
                // A fresh destination session is opened before timing. Its first
                // bound SELECT still performs both source and destination guards,
                // fresh FK checks, parameter binding, execution and row decoding.
                backend.close_session(&ds.id, TARGET);
                query(&backend, TARGET, &ds, "SELECT 1");
                let started = Instant::now();
                let result = bound(&backend, &ds, captured, Page::first(500), Some(source));
                let elapsed = started.elapsed();
                assert_eq!(result.unwrap().rows.len(), expected_rows);
                if sample != 0 {
                    samples.push(elapsed);
                }
            }
            report_related_timings(ds.driver, label, samples);
        }
    }));
    drop(backend);
    runtime.block_on(async {
        let mut conn = Connection::open(&ds).await.unwrap();
        for table in [&child, &parent] {
            conn.execute(&format!("DROP TABLE IF EXISTS {table}"), Page::ALL)
                .await
                .unwrap();
        }
    });
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

fn benchmark_sqlite_reverse_catalog(empty_tables: usize) {
    let local = LocalDatabase::new();
    let db = rusqlite::Connection::open(&local.source.file).unwrap();
    let mut setup = String::from(
        "BEGIN; CREATE TABLE parent(id INTEGER PRIMARY KEY); CREATE TABLE child(id INTEGER,parent_id INTEGER REFERENCES parent(id)); INSERT INTO parent VALUES(1);",
    );
    for index in 0..empty_tables {
        setup.push_str(&format!("CREATE TABLE unused_{index}(id INTEGER);"));
    }
    setup.push_str("COMMIT;");
    db.execute_batch(&setup).unwrap();
    drop(db);
    let backend = Backend::new(egui::Context::default());
    let mut cold = Vec::with_capacity(7);
    for sample in 0..8 {
        backend.close_session(&local.source.id, SOURCE);
        let data = query(&backend, SOURCE, &local.source, "SELECT id FROM parent");
        let request = metadata_request(&local.source, &data, RelatedDirection::Referencing);
        let started = Instant::now();
        let result = metadata(&backend, request);
        let elapsed = started.elapsed();
        assert_eq!(result.unwrap().keys.len(), 1);
        if sample != 0 {
            cold.push(elapsed);
        }
    }
    report_related_timings(
        DriverKind::Sqlite,
        &format!("reverse_catalog_cold_{empty_tables}_empty_tables_plus_2"),
        cold,
    );
    let data = query(&backend, SOURCE, &local.source, "SELECT id FROM parent");
    let mut warm = Vec::with_capacity(7);
    for sample in 0..8 {
        let request = metadata_request(&local.source, &data, RelatedDirection::Referencing);
        let started = Instant::now();
        let result = metadata(&backend, request);
        let elapsed = started.elapsed();
        assert_eq!(result.unwrap().keys.len(), 1);
        if sample != 0 {
            warm.push(elapsed);
        }
    }
    report_related_timings(
        DriverKind::Sqlite,
        &format!("reverse_catalog_warm_{empty_tables}_empty_tables_plus_2"),
        warm,
    );
}

/// Backend wall-clock request latency, including channel/runtime scheduling and
/// database work, not UI-frame or GPU time. Setup, opening connections, source
/// SELECTs, key capture and row-count assertions are outside the samples.
/// Metadata includes live context/TX guards; bound reads include source guards,
/// request construction, SQL generation, fresh FK validation on both sessions,
/// SELECT and decoding. The shared helpers' event/token checks are included.
/// One warmup + seven
/// samples per phase; SQLite reverse scans explicitly report the catalog size.
#[test]
#[ignore = "manual release measurement of lazy FK metadata and bound Related Rows reads"]
#[allow(clippy::assertions_on_constants)]
fn related_metadata_and_bound_read_latency() {
    assert!(!cfg!(debug_assertions), "run this benchmark with --release");
    println!(
        "related_rows metric=backend_wall_clock no_ui=true no_gpu=true connect_outside=true source_select_outside=true cold_cache=per_connection_epoch warm_cache=same_connection warmup=1 samples=7"
    );
    let local = LocalDatabase::new();
    benchmark_related_requests(local.source.clone());
    for driver in [DriverKind::Postgres, DriverKind::MySql] {
        if let Some(source) = server(driver) {
            benchmark_related_requests(source);
        }
    }
    for tables in [500, 5000] {
        benchmark_sqlite_reverse_catalog(tables);
    }
}

/// Inserts child rows 1..=count for parent 1 in a scrambled physical order.
fn seed_children_sql(driver: DriverKind, child: &str, count: usize) -> Vec<String> {
    let scrambled = format!("(n * 7919) % {count} + 1");
    match driver {
        DriverKind::Postgres => vec![format!(
            "INSERT INTO {child} SELECT {scrambled}, 1, repeat('x', 40) FROM generate_series(0, {}) n",
            count - 1
        )],
        DriverKind::MySql => vec![
            format!("SET SESSION cte_max_recursion_depth = {}", count + 10),
            format!(
                "INSERT INTO {child} WITH RECURSIVE s(n) AS (SELECT 0 UNION ALL SELECT n + 1 FROM s WHERE n < {}) SELECT {scrambled}, 1, REPEAT('x', 40) FROM s",
                count - 1
            ),
        ],
        DriverKind::Sqlite => vec![format!(
            "INSERT INTO {child} WITH RECURSIVE s(n) AS (SELECT 0 UNION ALL SELECT n + 1 FROM s WHERE n < {}) SELECT {scrambled}, 1, printf('%.40c', 'x') FROM s",
            count - 1
        )],
    }
}

struct PagingTables {
    parent: String,
    child: String,
}

fn create_paging_tables(runtime: &tokio::runtime::Runtime, ds: &DataSource, children: usize) -> PagingTables {
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let schema = match ds.driver {
        DriverKind::Postgres => "public",
        DriverKind::MySql => "dbeast",
        DriverKind::Sqlite => "main",
    };
    let parent_name = format!("rel_page_{}_parent", &suffix[..10]);
    let parent = sqlgen::qualified(ds.driver, Some(schema), &parent_name);
    let parent_reference = if ds.driver == DriverKind::Sqlite {
        sqlgen::ident(ds.driver, &parent_name)
    } else {
        parent.clone()
    };
    let child = sqlgen::qualified(ds.driver, Some(schema), &format!("rel_page_{}_child", &suffix[..10]));
    let engine = if ds.driver == DriverKind::MySql { " ENGINE=InnoDB" } else { "" };
    runtime.block_on(async {
        let mut conn = Connection::open(ds).await.unwrap();
        let mut statements = vec![
            format!("CREATE TABLE {parent}(id INTEGER PRIMARY KEY, label VARCHAR(80)){engine}"),
            format!("CREATE TABLE {child}(id INTEGER PRIMARY KEY, parent_id INTEGER, note VARCHAR(80), FOREIGN KEY(parent_id) REFERENCES {parent_reference}(id)){engine}"),
            format!("INSERT INTO {parent} VALUES (1, 'many'), (2, 'none')"),
        ];
        statements.extend(seed_children_sql(ds.driver, &child, children));
        for sql in statements {
            conn.execute(&sql, Page::ALL).await.unwrap();
        }
    });
    PagingTables { parent, child }
}

fn drop_paging_tables(runtime: &tokio::runtime::Runtime, ds: &DataSource, tables: &PagingTables) {
    runtime.block_on(async {
        let mut conn = Connection::open(ds).await.unwrap();
        for table in [&tables.child, &tables.parent] {
            conn.execute(&format!("DROP TABLE IF EXISTS {table}"), Page::ALL).await.unwrap();
        }
    });
}

fn ints(data: &QueryResult, column: &str) -> Vec<i64> {
    let index = data.columns.iter().position(|c| c.name == column).unwrap();
    data.rows
        .iter()
        .map(|row| match row[index] {
            Value::Int(value) => value,
            ref other => panic!("{other:?}"),
        })
        .collect()
}

fn exercise_related_paging(ds: DataSource) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let backend = Backend::new(egui::Context::default());
    let tables = create_paging_tables(&runtime, &ds, 1250);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let parents = query(&backend, SOURCE, &ds, &format!("SELECT id FROM {} ORDER BY id", tables.parent));
        let keys = metadata(&backend, metadata_request(&ds, &parents, RelatedDirection::Referencing)).unwrap();
        let many = capture(&ds, &parents, 0, &keys.keys[0], RelatedDirection::Referencing);
        let none = capture(&ds, &parents, 1, &keys.keys[0], RelatedDirection::Referencing);
        let mut seen = Vec::new();
        for (skip, expected, truncated) in [(0, 500, true), (500, 500, true), (1000, 250, false), (1250, 0, false)] {
            let page = bound(&backend, &ds, &many, Page { skip, limit: 500 }, None).unwrap();
            let ids = ints(&page, "id");
            assert_eq!((ids.len(), page.truncated), (expected, truncated), "page at {skip}");
            assert!(ids.windows(2).all(|pair| pair[0] < pair[1]), "pages follow the child key");
            seen.extend(ids);
        }
        assert_eq!(seen, (1..=1250).collect::<Vec<_>>(), "no row is repeated or skipped");
        let empty = bound(&backend, &ds, &none, Page::first(500), None).unwrap();
        assert_eq!((empty.rows.len(), empty.truncated), (0, false));
        assert_eq!(empty.columns.len(), 3, "an empty page keeps its columns");
        // The referenced side has a single row and the same server-side window.
        let children = query(&backend, SOURCE, &ds, &format!("SELECT parent_id FROM {} WHERE id = 7", tables.child));
        let outgoing = metadata(&backend, metadata_request(&ds, &children, RelatedDirection::Referenced)).unwrap();
        let outgoing = capture(&ds, &children, 0, &outgoing.keys[0], RelatedDirection::Referenced);
        let parent = bound(&backend, &ds, &outgoing, Page::first(500), Some(&children)).unwrap();
        assert_eq!((ints(&parent, "id"), parent.truncated), (vec![1], false));
        let beyond = bound(&backend, &ds, &outgoing, Page { skip: 1, limit: 500 }, None).unwrap();
        assert!(beyond.rows.is_empty());
    }));
    drop(backend);
    drop_paging_tables(&runtime, &ds, &tables);
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn sqlite_related_pages_are_limited_on_the_server_in_key_order() {
    let local = LocalDatabase::new();
    exercise_related_paging(local.source.clone());
}

#[test]
fn postgres_related_pages_are_limited_on_the_server_in_key_order() {
    if let Some(ds) = server(DriverKind::Postgres) {
        exercise_related_paging(ds);
    }
}

#[test]
fn mysql_related_pages_are_limited_on_the_server_in_key_order() {
    if let Some(ds) = server(DriverKind::MySql) {
        exercise_related_paging(ds);
    }
}

/// Time until a related page is decoded, for the old shape (the whole
/// relation read, the client skipping rows) and for the server window.
#[test]
#[ignore = "manual release measurement: cargo test --release --test related_rows related_paging_cost -- --ignored --nocapture"]
fn related_paging_cost() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let local = LocalDatabase::new();
    let mut sources = vec![local.source.clone()];
    sources.extend([DriverKind::Postgres, DriverKind::MySql].into_iter().filter_map(server));
    for ds in sources {
        let tables = create_paging_tables(&runtime, &ds, 100_000);
        let backend = Backend::new(egui::Context::default());
        let parents = query(&backend, SOURCE, &ds, &format!("SELECT id FROM {} ORDER BY id", tables.parent));
        let keys = metadata(&backend, metadata_request(&ds, &parents, RelatedDirection::Referencing)).unwrap();
        let captured = capture(&ds, &parents, 0, &keys.keys[0], RelatedDirection::Referencing);
        drop(backend);
        runtime.block_on(async {
            let mut conn = Connection::open(&ds).await.unwrap();
            conn.apply_resolved_context(&captured.context).await.unwrap();
            let base = sqlgen::related_query(ds.driver, &captured).unwrap();
            for skip in [0, 50_000, 99_500] {
                let window = Page { skip, limit: 500 };
                let mut best = [Duration::MAX; 2];
                for _ in 0..5 {
                    let started = Instant::now();
                    let old = conn.execute_bound_query(&base, window, None).await.unwrap();
                    best[0] = best[0].min(started.elapsed());
                    let (paged, page) = sqlgen::related_page(ds.driver, captured.direction, &base, &["id".into()], window);
                    let started = Instant::now();
                    let new = conn.execute_bound_query(&paged, page, None).await.unwrap();
                    best[1] = best[1].min(started.elapsed());
                    assert_eq!(old.rows.len(), new.rows.len());
                }
                println!(
                    "related_paging driver={:?} children=100000 skip={skip} limit=500 whole_relation_ms={:.3} server_window_ms={:.3}",
                    ds.driver,
                    best[0].as_secs_f64() * 1000.0,
                    best[1].as_secs_f64() * 1000.0
                );
            }
        });
        drop_paging_tables(&runtime, &ds, &tables);
    }
}

fn export_outcome(backend: &Backend, task: u64) -> Result<String, String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Event::Task { id, event: dbeast::backend::TaskEvent::Finished(result) } = receive(backend, deadline)
            && id == task
        {
            return result;
        }
    }
}

fn export(backend: &Backend, task: u64, ds: &DataSource, captured: &CapturedRelatedQuery, path: &std::path::Path) {
    backend.export_related(
        task,
        ds.clone(),
        Arc::new(captured.clone()),
        ExportOptions::default(),
        TableName::new("children"),
        path.to_path_buf(),
    );
}

fn children_capture(backend: &Backend, ds: &DataSource, parent: &str) -> CapturedRelatedQuery {
    let parents = query(backend, SOURCE, ds, &format!("SELECT id FROM {parent} ORDER BY id"));
    let keys = metadata(backend, metadata_request(ds, &parents, RelatedDirection::Referencing)).unwrap();
    capture(ds, &parents, 0, &keys.keys[0], RelatedDirection::Referencing)
}

/// Exports more rows than one stream batch, keeps the previous file when the
/// export fails or stops, and never leaves a partial file behind.
fn exercise_related_export(ds: DataSource) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let backend = Backend::new(egui::Context::default());
    let tables = create_paging_tables(&runtime, &ds, 2500);
    let dir = std::env::temp_dir().join(format!("dbeast_related_export_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let captured = children_capture(&backend, &ds, &tables.parent);
        let target = dir.join("children.csv");
        export(&backend, 71, &ds, &captured, &target);
        assert_eq!(export_outcome(&backend, 71).unwrap(), format!("2500 rows exported to {}", target.display()));
        let csv = std::fs::read_to_string(&target).unwrap();
        let mut ids: Vec<i64> = csv.lines().skip(1).map(|line| line.split(',').next().unwrap().parse().unwrap()).collect();
        ids.sort();
        assert_eq!(ids, (1..=2500).collect::<Vec<_>>(), "every related row is written once");

        std::fs::write(&target, "previous export\n").unwrap();
        // Stop before the first row: the previous file stays.
        export(&backend, 72, &ds, &captured, &target);
        backend.stop_task(72);
        assert_eq!(export_outcome(&backend, 72).unwrap_err(), "Stopped");
        // A write error: the target is a directory that cannot be replaced.
        let blocked = dir.join("blocked.csv");
        std::fs::create_dir(&blocked).unwrap();
        std::fs::write(blocked.join("keep"), "x").unwrap();
        export(&backend, 73, &ds, &captured, &blocked);
        assert!(export_outcome(&backend, 73).is_err());
        assert!(blocked.join("keep").exists());
        // The relation now resolves to another physical table.
        let mut moved = captured.clone();
        moved.relation.source.name.push_str("_gone");
        export(&backend, 74, &ds, &moved, &target);
        assert!(export_outcome(&backend, 74).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "previous export\n");
        let mut names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        names.sort();
        assert_eq!(names, ["blocked.csv", "children.csv"], "no partial files are left");
    }));
    drop(backend);
    drop_paging_tables(&runtime, &ds, &tables);
    std::fs::remove_dir_all(&dir).ok();
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn sqlite_related_export_streams_and_keeps_previous_files() {
    let local = LocalDatabase::new();
    exercise_related_export(local.source.clone());
}

#[test]
fn postgres_related_export_streams_and_keeps_previous_files() {
    if let Some(ds) = server(DriverKind::Postgres) {
        exercise_related_export(ds);
    }
}

#[test]
fn mysql_related_export_streams_and_keeps_previous_files() {
    if let Some(ds) = server(DriverKind::MySql) {
        exercise_related_export(ds);
    }
}

/// A value DBeast cannot read exactly after the first stream batch fails the
/// export and the rows already written never replace the previous file.
#[test]
fn postgres_related_export_error_after_batches_keeps_previous_file() {
    let Some(ds) = server(DriverKind::Postgres) else { return };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..10].to_string();
    let (parent, child) = (format!("rel_exp_{suffix}_parent"), format!("rel_exp_{suffix}_child"));
    runtime.block_on(async {
        let mut conn = Connection::open(&ds).await.unwrap();
        for sql in [
            format!("CREATE TABLE {parent}(id INTEGER PRIMARY KEY)"),
            format!("CREATE TABLE {child}(id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES {parent}(id), price money)"),
            format!("INSERT INTO {parent} VALUES (1)"),
            format!("INSERT INTO {child} SELECT n, 1, CASE WHEN n = 1500 THEN 2.5::money END FROM generate_series(1, 2000) n"),
        ] {
            conn.execute(&sql, Page::ALL).await.unwrap();
        }
    });
    let backend = Backend::new(egui::Context::default());
    let dir = std::env::temp_dir().join(format!("dbeast_related_export_{suffix}"));
    std::fs::create_dir_all(&dir).unwrap();
    let target = dir.join("children.csv");
    std::fs::write(&target, "previous export\n").unwrap();
    let captured = children_capture(&backend, &ds, &parent);
    export(&backend, 75, &ds, &captured, &target);
    let error = export_outcome(&backend, 75).unwrap_err();
    drop(backend);
    runtime.block_on(async {
        let mut conn = Connection::open(&ds).await.unwrap();
        conn.execute(&format!("DROP TABLE {child}, {parent}"), Page::ALL).await.unwrap();
    });
    assert!(error.contains("\"price\""), "{error}");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "previous export\n");
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "no partial file is left");
    std::fs::remove_dir_all(&dir).ok();
}

/// Stop, and closing the console, end a related read that waits for a
/// table lock; the console reads again afterwards.
#[test]
fn postgres_stop_ends_a_related_read_waiting_for_a_lock() {
    let Some(ds) = server(DriverKind::Postgres) else { return };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let backend = Backend::new(egui::Context::default());
    let tables = create_paging_tables(&runtime, &ds, 10);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let captured = children_capture(&backend, &ds, &tables.parent);
        for close in [false, true] {
            backend.run(RunRequest {
                console_id: 703,
                ds: ds.clone(),
                statements: vec![format!("LOCK TABLE {} IN ACCESS EXCLUSIVE MODE", tables.child)],
                page: Page::ALL,
                manual_tx: true,
                context: ExecutionContext::default(),
                expected_context: None,
            });
            let (statements, in_tx) = finish(&backend, 703);
            assert!(statements.iter().all(|(_, result)| result.is_ok()) && in_tx);
            backend.run_bound(BoundRunRequest {
                console_id: TARGET,
                ds: ds.clone(),
                query: Arc::new(captured.clone()),
                page: Page::first(500),
                manual_tx: false,
                source_guard: None,
            });
            std::thread::sleep(Duration::from_millis(300));
            let started = Instant::now();
            if close {
                backend.close_session(&ds.id, TARGET);
            } else {
                backend.cancel(TARGET);
            }
            let (mut statements, in_tx) = finish(&backend, TARGET);
            assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
            assert!(!in_tx);
            let error = statements.remove(0).1.unwrap_err();
            assert!(error.starts_with("Stopped"), "{error}");
            query(&backend, 703, &ds, "ROLLBACK");
        }
        let page = bound(&backend, &ds, &captured, Page::first(500), None).unwrap();
        assert_eq!(page.rows.len(), 10);
    }));
    backend.close_session(&ds.id, 703);
    drop(backend);
    drop_paging_tables(&runtime, &ds, &tables);
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}
