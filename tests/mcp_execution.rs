//! MCP execution tests use temporary files and the loopback Docker fixtures only.

use dbeast::{
    drivers::Connection,
    mcp::{ExecutionError, ExecutionOutput, ExecutionRequest, Operation, TableRef, executor},
    model::{DataSource, DriverKind, Page},
};
use tokio_util::sync::CancellationToken;

struct SqliteFixture {
    source: DataSource,
    path: std::path::PathBuf,
}
impl SqliteFixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("dbeast-mcp-{}.sqlite", uuid::Uuid::new_v4()));
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection.execute_batch("CREATE TABLE customers(id INTEGER PRIMARY KEY,name TEXT); CREATE TABLE orders(id INTEGER PRIMARY KEY,customer_id INTEGER,total REAL); INSERT INTO customers VALUES(1,'Alice'),(2,'Bob'); INSERT INTO orders VALUES(1,1,10),(2,1,20),(3,2,4);").unwrap();
        let mut source = DataSource::new(DriverKind::Sqlite);
        source.file = path.to_string_lossy().into_owned();
        Self { source, path }
    }
    fn alter(&self, sql: &str) {
        rusqlite::Connection::open(&self.path)
            .unwrap()
            .execute_batch(sql)
            .unwrap();
    }
}
impl Drop for SqliteFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn table(schema: &str, name: &str) -> TableRef {
    TableRef {
        schema: schema.into(),
        name: name.into(),
    }
}
fn read(source: &DataSource, schema: &str, name: &str, columns: &[&str]) -> Operation {
    Operation::ReadTable {
        source_id: source.id.clone(),
        table: table(schema, name),
        columns: columns.iter().map(|column| (*column).into()).collect(),
        limit: 100,
        offset: 0,
    }
}
fn read_sql(source: &DataSource, sql: &str) -> Operation {
    Operation::ReadSql {
        source_id: source.id.clone(),
        sql: sql.into(),
    }
}
async fn execute(
    source: &DataSource,
    operation: Operation,
    expected: Option<String>,
) -> Result<ExecutionOutput, ExecutionError> {
    executor::execute(ExecutionRequest {
        source: source.clone(),
        operation,
        expected_table_identity: expected,
        cancellation: CancellationToken::new(),
        execution_lease: None,
    })
    .await
}

#[tokio::test]
async fn sqlite_structured_and_read_sql_support_scoped_relational_analysis() {
    let fixture = SqliteFixture::new();
    let first = execute(
        &fixture.source,
        read(&fixture.source, "main", "customers", &["id", "name"]),
        None,
    )
    .await
    .unwrap();
    assert_eq!(first.value["rows"][0], serde_json::json!(["1", "Alice"]));
    assert!(first.table_identity.is_some());
    let query = "WITH totals AS (SELECT customer_id,sum(total) AS total FROM main.orders GROUP BY customer_id) SELECT c.name,t.total FROM main.customers c JOIN totals t ON c.id=t.customer_id WHERE t.total>(SELECT avg(total) FROM main.orders) ORDER BY t.total DESC";
    let output = execute(&fixture.source, read_sql(&fixture.source, query), None)
        .await
        .unwrap();
    assert_eq!(output.value["rows"], serde_json::json!([["Alice", "30"]]));
    assert!(
        output.table_identity.is_none(),
        "a join cannot mint a single-table grant"
    );
    let output = execute(
        &fixture.source,
        read_sql(
            &fixture.source,
            "SELECT count(*) FROM main.customers WHERE lower(name) LIKE 'a%'",
        ),
        first.table_identity,
    )
    .await
    .unwrap();
    assert_eq!(output.value["rows"], serde_json::json!([["1"]]));
}

#[tokio::test]
async fn sqlite_rejects_unsafe_schema_and_stale_table_identity() {
    let fixture = SqliteFixture::new();
    fixture.alter("CREATE VIEW unsafe_view AS SELECT load_extension('/must-not-open') AS id; CREATE TABLE generated(id INTEGER,g INTEGER GENERATED ALWAYS AS (id+1) STORED)");
    for name in ["unsafe_view", "generated"] {
        let error = execute(
            &fixture.source,
            read(&fixture.source, "main", name, &["id"]),
            None,
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.code, "unsupported_safe_read");
    }
    let identity = execute(
        &fixture.source,
        read(&fixture.source, "main", "customers", &["id"]),
        None,
    )
    .await
    .unwrap()
    .table_identity;
    fixture.alter("DROP TABLE customers; CREATE TABLE customers(id INTEGER PRIMARY KEY,name TEXT); INSERT INTO customers VALUES(99,'replacement')");
    let error = execute(
        &fixture.source,
        read_sql(&fixture.source, "SELECT id FROM main.customers"),
        identity.clone(),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(error.code, "resource_unavailable");
    fixture.alter("DROP TABLE customers");
    assert_eq!(
        execute(
            &fixture.source,
            read(&fixture.source, "main", "customers", &["id"]),
            identity.clone()
        )
        .await
        .err()
        .unwrap()
        .code,
        "resource_unavailable"
    );
    fixture.alter("CREATE VIEW customers AS SELECT load_extension('/must-not-open') AS id");
    assert_eq!(
        execute(
            &fixture.source,
            read(&fixture.source, "main", "customers", &["id"]),
            identity
        )
        .await
        .err()
        .unwrap()
        .code,
        "resource_unavailable"
    );
}

#[tokio::test]
async fn sqlite_read_sql_rejects_writes_functions_paths_and_excessive_cells() {
    let fixture = SqliteFixture::new();
    for sql in [
        "SELECT load_extension('/must-not-open')",
        "DELETE FROM main.customers",
        "PRAGMA query_only=OFF",
        "ATTACH '/must-not-open' AS other",
        "WITH x AS (DELETE FROM main.customers RETURNING *) SELECT * FROM x",
        "SELECT * FROM main.sqlite_schema",
        "SELECT 1; DELETE FROM main.customers",
    ] {
        assert!(
            execute(&fixture.source, read_sql(&fixture.source, sql), None)
                .await
                .is_err(),
            "{sql}"
        );
    }
    fixture.alter("UPDATE customers SET name=printf('%.*c',70000,'x') WHERE id=1");
    let error = execute(
        &fixture.source,
        read(&fixture.source, "main", "customers", &["name"]),
        None,
    )
    .await
    .err()
    .unwrap();
    assert_eq!(error.code, "limit_exceeded");
    assert!(!error.message.contains(&fixture.source.file));
    let connection = rusqlite::Connection::open(&fixture.path).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM customers", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn sqlite_full_sql_is_explicit_and_cancellation_is_bounded() {
    let fixture = SqliteFixture::new();
    let operation = Operation::Sql {
        source_id: fixture.source.id.clone(),
        sql: "INSERT INTO customers VALUES(3,'permitted')".into(),
    };
    let mut read_only = fixture.source.clone();
    read_only.read_only = true;
    assert_eq!(
        execute(&read_only, operation.clone(), None)
            .await
            .err()
            .unwrap()
            .code,
        "permission_denied"
    );
    assert_eq!(
        execute(
            &read_only,
            read_sql(&read_only, "SELECT count(*) FROM main.customers"),
            None
        )
        .await
        .unwrap()
        .value["rows"],
        serde_json::json!([["2"]])
    );
    let mut unavailable = fixture.source.clone();
    unavailable.credentials_unavailable = true;
    assert_eq!(
        execute(&unavailable, operation.clone(), None)
            .await
            .err()
            .unwrap()
            .code,
        "execution_failed"
    );
    execute(&fixture.source, operation, None).await.unwrap();
    let other = SqliteFixture::new();
    let original = std::fs::read(&other.path).unwrap();
    for sql in [
        format!("ATTACH '{}' AS other", other.path.to_string_lossy()),
        format!("VACUUM INTO '{}'", other.path.to_string_lossy()),
        "PRAGMA writable_schema=ON".into(),
        "SELECT load_extension('/must-not-open')".into(),
        "CREATE TEMP TABLE escape(id int)".into(),
        "CREATE VIRTUAL TABLE escape USING fts5(value)".into(),
    ] {
        assert!(
            execute(
                &fixture.source,
                Operation::Sql {
                    source_id: fixture.source.id.clone(),
                    sql
                },
                None
            )
            .await
            .is_err()
        );
    }
    assert_eq!(std::fs::read(&other.path).unwrap(), original);
    let cancel = CancellationToken::new();
    let request=ExecutionRequest{source:fixture.source.clone(),operation:Operation::Sql{source_id:fixture.source.id.clone(),sql:"WITH RECURSIVE n(x) AS (SELECT 0 UNION ALL SELECT x+1 FROM n WHERE x<1000000000) SELECT sum(x) FROM n".into()},expected_table_identity:None,cancellation:cancel.clone(),execution_lease:None};
    let started = std::time::Instant::now();
    let running = tokio::spawn(executor::execute(request));
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    cancel.cancel();
    let error = tokio::time::timeout(std::time::Duration::from_secs(3), running)
        .await
        .unwrap()
        .unwrap()
        .err()
        .unwrap();
    assert!(error.outcome_unknown);
    assert!(started.elapsed() < std::time::Duration::from_secs(3));
}

fn pg() -> DataSource {
    let mut source = DataSource::new(DriverKind::Postgres);
    source.host = "127.0.0.1".into();
    source.port = 55432;
    source.user = "dbeast".into();
    source.password = "dbeast".into();
    source.database = "dbeast".into();
    source
}
fn mysql() -> DataSource {
    let mut source = DataSource::new(DriverKind::MySql);
    source.host = "127.0.0.1".into();
    source.port = 53306;
    source.user = "root".into();
    source.password = "dbeast".into();
    source.database = "shop".into();
    source
}
fn available(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        std::time::Duration::from_millis(500),
    )
    .is_ok()
}
async fn sql(connection: &mut Connection, sql: &str) {
    connection.execute(sql, Page::first(1)).await.unwrap();
}

#[tokio::test]
async fn postgres_safe_reads_deny_executable_objects_and_preserve_identity() {
    if !available(55432) {
        eprintln!("skipping local PostgreSQL MCP fixture: port 55432 unavailable");
        return;
    }
    let source = pg();
    let mut connection = Connection::open(&source).await.unwrap();
    let schema = format!("mcp_{}", uuid::Uuid::new_v4().simple());
    for statement in [
        format!("CREATE SCHEMA {schema}"),
        format!("CREATE TABLE {schema}.items(id integer,name text)"),
        format!("INSERT INTO {schema}.items VALUES(1,'Alice'),(2,'Bob')"),
        format!("CREATE TABLE {schema}.marker(id integer)"),
        format!(
            "CREATE FUNCTION {schema}.side_effect() RETURNS integer LANGUAGE plpgsql AS $$ BEGIN INSERT INTO {schema}.marker VALUES(1); RETURN 1; END $$"
        ),
        format!("CREATE VIEW {schema}.unsafe AS SELECT {schema}.side_effect() AS id"),
        format!(
            "CREATE TABLE {schema}.generated(id integer,g integer GENERATED ALWAYS AS(id+1) STORED)"
        ),
        format!("CREATE DOMAIN {schema}.custom AS integer"),
        format!("CREATE TABLE {schema}.custom_type(id {schema}.custom)"),
        format!("CREATE TABLE {schema}.rls(id integer)"),
        format!("ALTER TABLE {schema}.rls ENABLE ROW LEVEL SECURITY"),
    ] {
        sql(&mut connection, &statement).await;
    }
    let first = execute(
        &source,
        read(&source, &schema, "items", &["id", "name"]),
        None,
    )
    .await
    .unwrap();
    assert_eq!(first.value["rows"][0], serde_json::json!(["1", "Alice"]));
    sql(
        &mut connection,
        &format!("CREATE TABLE {schema}.checked(id integer CHECK(id>0))"),
    )
    .await;
    sql(&mut connection,&format!("CREATE FUNCTION {schema}.coerce(item {schema}.items) RETURNS numeric LANGUAGE plpgsql AS $$ BEGIN INSERT INTO {schema}.marker VALUES(1); RETURN item.id; END $$")).await;
    sql(&mut connection,&format!("CREATE CAST ({schema}.items AS numeric) WITH FUNCTION {schema}.coerce({schema}.items) AS IMPLICIT")).await;
    // A CHECK with only built-in operators is not user code; one calling a
    // user routine stays denied because the planner may fold it.
    execute(&source, read(&source, &schema, "checked", &["id"]), None)
        .await
        .unwrap();
    sql(
        &mut connection,
        &format!("CREATE TABLE {schema}.user_checked(id integer CHECK({schema}.side_effect() > 0 OR id > 0))"),
    )
    .await;
    for name in ["unsafe", "generated", "custom_type", "rls", "user_checked"] {
        let error = execute(&source, read(&source, &schema, name, &["id"]), None)
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, "unsupported_safe_read", "{name}");
    }
    let output=execute(&source,read_sql(&source,&format!("WITH x AS (SELECT id,name FROM {schema}.items) SELECT lower(a.name),count(*) FROM x a JOIN {schema}.items b ON a.id=b.id GROUP BY a.name ORDER BY a.name")),first.table_identity.clone()).await.unwrap();
    assert_eq!(output.value["rows"][0], serde_json::json!(["alice", "1"]));
    assert!(
        execute(
            &source,
            read_sql(&source, &format!("SELECT {schema}.side_effect()")),
            None
        )
        .await
        .is_err()
    );
    for query in [
        format!("SELECT round(t,0) FROM {schema}.items t"),
        format!("SELECT round({schema}.items,0) FROM {schema}.items"),
    ] {
        assert_eq!(
            execute(&source, read_sql(&source, &query), None)
                .await
                .err()
                .unwrap()
                .code,
            "unsupported_safe_read"
        );
    }
    let prefix = "a".repeat(63);
    assert_eq!(
        execute(
            &source,
            read_sql(
                &source,
                &format!("SELECT round({prefix},0) FROM {schema}.items AS {prefix}b")
            ),
            None
        )
        .await
        .err()
        .unwrap()
        .code,
        "invalid_input"
    );
    let marker = connection
        .execute(
            &format!("SELECT count(*) FROM {schema}.marker"),
            Page::first(1),
        )
        .await
        .unwrap();
    assert_eq!(marker.rows[0][0], dbeast::model::Value::Int(0));
    sql(
        &mut connection,
        &format!("DROP CAST ({schema}.items AS numeric)"),
    )
    .await;
    sql(
        &mut connection,
        &format!("DROP FUNCTION {schema}.coerce({schema}.items)"),
    )
    .await;
    sql(&mut connection, &format!("DROP TABLE {schema}.items")).await;
    assert_eq!(
        execute(
            &source,
            read(&source, &schema, "items", &["id"]),
            first.table_identity.clone()
        )
        .await
        .err()
        .unwrap()
        .code,
        "resource_unavailable"
    );
    sql(
        &mut connection,
        &format!("CREATE TABLE {schema}.items(id integer,name text)"),
    )
    .await;
    assert_eq!(
        execute(
            &source,
            read(&source, &schema, "items", &["id"]),
            first.table_identity
        )
        .await
        .err()
        .unwrap()
        .code,
        "resource_unavailable"
    );
    sql(&mut connection, &format!("DROP SCHEMA {schema} CASCADE")).await;
}

#[tokio::test]
async fn postgres_cancellation_and_large_protocol_rows_do_not_hang() {
    if !available(55432) {
        return;
    }
    let source = pg();
    let mut observer = Connection::open(&source).await.unwrap();
    let cancel = CancellationToken::new();
    let running = tokio::spawn(executor::execute(ExecutionRequest {
        source: source.clone(),
        operation: Operation::Sql {
            source_id: source.id.clone(),
            sql: "SELECT pg_sleep(10)".into(),
        },
        expected_table_identity: None,
        cancellation: cancel.clone(),
        execution_lease: None,
    }));
    tokio::time::timeout(std::time::Duration::from_secs(5),async {
        loop {
            let active=observer.execute("SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE application_name='DBeast MCP' AND state='active' AND wait_event='PgSleep'",Page::first(1)).await.unwrap();
            if active.rows[0][0]!=dbeast::model::Value::Int(0){break;}
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await.expect("MCP fixture query did not become active");
    cancel.cancel();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(3), running)
            .await
            .unwrap()
            .unwrap()
            .err()
            .unwrap()
            .outcome_unknown
    );
    let huge = execute(
        &source,
        Operation::Sql {
            source_id: source.id.clone(),
            sql: "SELECT repeat('x',10000000)".into(),
        },
        None,
    )
    .await;
    assert!(huge.is_err());
    let binary = execute(
        &source,
        Operation::Sql {
            source_id: source.id.clone(),
            sql: "SELECT decode('0001ff','hex'),NULL::bytea".into(),
        },
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        binary.value["rows"],
        serde_json::json!([[{"binary_hex":"0001ff"},null]])
    );
}

#[tokio::test]
async fn mysql_safe_reads_verify_locks_identity_and_reject_generated_views() {
    if !available(53306) {
        eprintln!("skipping local MySQL MCP fixture: port 53306 unavailable");
        return;
    }
    let source = mysql();
    let mut connection = Connection::open(&source).await.unwrap();
    let name = format!("mcp_{}", uuid::Uuid::new_v4().simple());
    for statement in [
        format!("CREATE TABLE shop.{name}(id int,name varchar(50)) ENGINE=InnoDB"),
        format!("INSERT INTO shop.{name} VALUES(1,'Alice'),(2,'Bob')"),
        format!(
            "CREATE TABLE shop.{name}_generated(id int,g int GENERATED ALWAYS AS(id+1) STORED) ENGINE=InnoDB"
        ),
        format!("CREATE VIEW shop.{name}_view AS SELECT SLEEP(5) AS id"),
    ] {
        sql(&mut connection, &statement).await;
    }
    let first = execute(&source, read(&source, "shop", &name, &["id", "name"]), None)
        .await
        .unwrap();
    assert_eq!(first.value["rows"][0], serde_json::json!(["1", "Alice"]));
    for suffix in ["_generated", "_view"] {
        assert_eq!(
            execute(
                &source,
                read(&source, "shop", &format!("{name}{suffix}"), &["id"]),
                None
            )
            .await
            .err()
            .unwrap()
            .code,
            "unsupported_safe_read"
        );
    }
    let output=execute(&source,read_sql(&source,&format!("SELECT lower(a.name),count(*) FROM shop.{name} a JOIN shop.{name} b ON a.id=b.id GROUP BY a.name ORDER BY a.name")),first.table_identity.clone()).await.unwrap();
    assert_eq!(output.value["rows"][0], serde_json::json!(["alice", "1"]));
    let mut restricted = source.clone();
    restricted.user = "dbeast".into();
    assert_eq!(
        execute(&restricted, read(&restricted, "shop", &name, &["id"]), None)
            .await
            .err()
            .unwrap()
            .code,
        "unsupported_safe_read"
    );
    sql(&mut connection, &format!("DROP TABLE shop.{name}")).await;
    assert_eq!(
        execute(
            &source,
            read(&source, "shop", &name, &["id"]),
            first.table_identity.clone()
        )
        .await
        .err()
        .unwrap()
        .code,
        "resource_unavailable"
    );
    sql(
        &mut connection,
        &format!("CREATE TABLE shop.{name}(id int,name varchar(50)) ENGINE=InnoDB"),
    )
    .await;
    assert_eq!(
        execute(
            &source,
            read(&source, "shop", &name, &["id"]),
            first.table_identity
        )
        .await
        .err()
        .unwrap()
        .code,
        "resource_unavailable"
    );
    for statement in [
        format!("DROP VIEW shop.{name}_view"),
        format!("DROP TABLE shop.{name},shop.{name}_generated"),
    ] {
        sql(&mut connection, &statement).await;
    }
}
