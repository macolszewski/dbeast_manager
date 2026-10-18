//! End-to-end driver tests against live databases.
//!
//! Start them with `scripts/test-dbs.sh` (Docker); each test returns
//! early when its database is unreachable.

use dbeast::drivers::Connection;
use dbeast::model::{
    BoundUpdate, DataSource, DbObject, DriverKind, ObjectKind, OriginColumn, OriginTable, Page,
    SchemaSelection, Value,
};
use futures_util::FutureExt;

fn pg() -> DataSource {
    let mut ds = DataSource::new(DriverKind::Postgres);
    ds.port = 55432;
    ds.user = "dbeast".into();
    ds.password = "dbeast".into();
    ds.database = "dbeast".into();
    ds
}

fn mysql() -> DataSource {
    let mut ds = DataSource::new(DriverKind::MySql);
    ds.host = "127.0.0.1".into();
    ds.port = 53306;
    ds.user = "dbeast".into();
    ds.password = "dbeast".into();
    ds.database = "shop".into();
    ds
}

async fn open(ds: &DataSource) -> Option<Connection> {
    match Connection::open(ds).await {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("skipping, {} unavailable: {e}", ds.driver.label());
            None
        }
    }
}

#[tokio::test]
async fn postgres_schema_reset_preserves_the_initial_path() {
    if let Err(error) = std::net::TcpStream::connect("127.0.0.1:55432") {
        eprintln!("skipping PostgreSQL schema reset test: TCP probe failed: {error}");
        return;
    }
    let mut conn = Connection::open(&pg()).await.unwrap();
    let initial = conn.resolved_context().await.unwrap();
    assert_eq!(initial.database.as_deref(), Some("dbeast"));
    let selected = conn
        .change_schema(&SchemaSelection::Named("shop".into()))
        .await
        .unwrap();
    assert!(selected.schemas.iter().any(|schema| schema == "shop"));
    let current = conn
        .execute_with_context("SELECT current_schema()", Page::first(1), None)
        .await
        .unwrap();
    assert_eq!(current.rows[0][0], Value::Text("shop".into()));
    assert_eq!(current.context, Some(selected.clone()));
    assert!(
        conn.change_schema(&SchemaSelection::Named("".into()))
            .await
            .is_err()
    );
    assert_eq!(conn.resolved_context().await.unwrap(), selected);
    assert_eq!(
        conn.change_schema(&SchemaSelection::Default).await.unwrap(),
        initial
    );
}

#[tokio::test]
async fn mysql_reset_without_an_initial_database_does_not_reconnect() {
    if let Err(error) = std::net::TcpStream::connect("127.0.0.1:53306") {
        eprintln!("skipping MySQL default reset test: TCP probe failed: {error}");
        return;
    }
    let mut ds = mysql();
    ds.database.clear();
    let mut conn = Connection::open(&ds).await.unwrap();
    assert_eq!(conn.resolved_context().await.unwrap().database, None);
    let connection = conn
        .execute("SELECT CONNECTION_ID()", Page::first(1))
        .await
        .unwrap()
        .rows[0][0]
        .clone();
    let selected = conn
        .change_schema(&SchemaSelection::Named("shop".into()))
        .await
        .unwrap();
    assert_eq!(selected.database.as_deref(), Some("shop"));
    conn.execute(
        "CREATE TEMPORARY TABLE context_reset_guard (note varchar(16))",
        Page::first(1),
    )
    .await
    .unwrap();
    conn.execute(
        "INSERT INTO context_reset_guard VALUES ('kept')",
        Page::first(1),
    )
    .await
    .unwrap();
    assert!(
        conn.change_schema(&SchemaSelection::Default).await.is_err(),
        "an impossible reset must not reconnect silently"
    );
    assert_eq!(conn.resolved_context().await.unwrap(), selected);
    assert_eq!(
        conn.execute("SELECT CONNECTION_ID()", Page::first(1))
            .await
            .unwrap()
            .rows[0][0],
        connection
    );
    assert_eq!(
        conn.execute("SELECT note FROM context_reset_guard", Page::first(1))
            .await
            .unwrap()
            .rows[0][0],
        Value::Text("kept".into())
    );
}

#[tokio::test]
async fn sqlite_schema_selector_does_not_change_name_resolution() {
    let mut ds = DataSource::new(DriverKind::Sqlite);
    ds.file = ":memory:".into();
    let mut conn = Connection::open(&ds).await.unwrap();
    for sql in [
        "CREATE TABLE main.same_name (marker TEXT)",
        "INSERT INTO main.same_name VALUES ('main')",
        "ATTACH DATABASE ':memory:' AS attached",
        "CREATE TABLE attached.same_name (marker TEXT)",
        "INSERT INTO attached.same_name VALUES ('attached')",
        "CREATE TEMP TABLE same_name (marker TEXT)",
        "INSERT INTO temp.same_name VALUES ('temp')",
    ] {
        conn.execute(sql, Page::first(1)).await.unwrap();
    }
    assert!(
        conn.change_schema(&SchemaSelection::Named("attached".into()))
            .await
            .is_err()
    );
    conn.change_schema(&SchemaSelection::Default).await.unwrap();
    for (sql, expected) in [
        ("SELECT marker FROM same_name", "temp"),
        ("SELECT marker FROM main.same_name", "main"),
        ("SELECT marker FROM attached.same_name", "attached"),
    ] {
        let result = conn
            .execute_with_context(sql, Page::first(1), None)
            .await
            .unwrap();
        assert_eq!(result.rows[0][0], Value::Text(expected.into()));
        assert!(result.context.is_some());
    }
}

#[tokio::test]
async fn sqlite_result_origins_and_bound_values_keep_the_attachment() {
    let mut ds = DataSource::new(DriverKind::Sqlite);
    ds.file = ":memory:".into();
    let mut conn = Connection::open(&ds).await.unwrap();
    for sql in [
        "CREATE TABLE contacts (tenant INTEGER NOT NULL,id INTEGER NOT NULL,name TEXT,note TEXT,payload BLOB,generated_name TEXT GENERATED ALWAYS AS (name || ':' || id) STORED,PRIMARY KEY(tenant,id))",
        "INSERT INTO contacts(tenant,id,name) VALUES (7,1,'main sentinel')",
        "ATTACH DATABASE ':memory:' AS \"other Żółw\"",
        "CREATE TABLE \"other Żółw\".contacts (tenant INTEGER NOT NULL,id INTEGER NOT NULL,name TEXT,note TEXT,payload BLOB,generated_name TEXT GENERATED ALWAYS AS (name || ':' || id) STORED,PRIMARY KEY(tenant,id))",
        "INSERT INTO \"other Żółw\".contacts(tenant,id,name) VALUES (7,1,'attached'),(8,1,'other tenant')",
    ] {
        conn.execute(sql, Page::ALL).await.unwrap();
    }
    let result = conn.execute_with_context(
        "SELECT id AS selected_id,tenant AS selected_tenant,name AS display_name,note,payload,generated_name,id+1 AS expression FROM \"other Żółw\".contacts ORDER BY tenant,id",
        Page::ALL,None).await.unwrap();
    let source = result.source.as_ref().unwrap();
    assert_ne!(source.session_epoch, 0);
    assert_eq!(source.columns.len(), result.columns.len());
    assert_eq!(source.exact_columns.len(), result.columns.len());
    let origin = source.columns[0].as_ref().unwrap();
    assert_eq!(
        origin.table,
        OriginTable::Named {
            schema: "other Żółw".into(),
            table: "contacts".into()
        }
    );
    assert_eq!(origin.column, OriginColumn::Named("id".into()));
    assert_eq!(
        source.columns[2].as_ref().unwrap().column,
        OriginColumn::Named("name".into())
    );
    assert!(source.columns[6].is_none());
    let metadata = conn.table_edit_metadata(&origin.table).await.unwrap();
    assert_eq!(metadata.table.schema, "other Żółw");
    assert_eq!(metadata.primary_key, ["tenant", "id"]);
    assert!(metadata.transactional);
    assert!(
        metadata
            .columns
            .iter()
            .find(|column| column.name == "generated_name")
            .unwrap()
            .generated
    );
    assert!(metadata.columns.iter().all(|column| !column.update_default));
    let payload = "O'Neil\\folder\nŻółw; -- SELECT 'x'";
    let bytes = vec![0, 39, 92, 255];
    let affected = conn.execute_bound_update(&BoundUpdate {
        sql:"UPDATE \"other Żółw\".contacts SET name=?1,note=?2,payload=?3 WHERE tenant=?4 AND id=?5".into(),
        params:vec![Value::Text(payload.into()),Value::Null,Value::Bytes(bytes.clone()),Value::Int(7),Value::Int(1)],source_row:0,
    }).await.unwrap();
    assert_eq!(affected, 1);
    let actual = conn
        .execute(
            "SELECT name,note,payload FROM \"other Żółw\".contacts WHERE tenant=7 AND id=1",
            Page::ALL,
        )
        .await
        .unwrap();
    assert_eq!(
        actual.rows[0],
        [
            Value::Text(payload.into()),
            Value::Null,
            Value::Bytes(bytes)
        ]
    );
    assert_eq!(
        conn.execute("SELECT name FROM main.contacts", Page::ALL)
            .await
            .unwrap()
            .rows[0][0],
        Value::Text("main sentinel".into())
    );
    assert_eq!(
        conn.execute(
            "SELECT name FROM \"other Żółw\".contacts WHERE tenant=8",
            Page::ALL
        )
        .await
        .unwrap()
        .rows[0][0],
        Value::Text("other tenant".into())
    );
    let saved = serde_json::to_string(&result).unwrap();
    let restored: dbeast::model::QueryResult = serde_json::from_str(&saved).unwrap();
    assert!(
        restored.source.is_none(),
        "saved provenance cannot authorize an UPDATE after restart"
    );
}

#[tokio::test]
async fn postgres_result_origins_include_keys_and_bound_parameters() {
    if let Err(error) = std::net::TcpStream::connect("127.0.0.1:55432") {
        eprintln!("skipping PostgreSQL edit metadata: TCP probe failed: {error}");
        return;
    }
    let mut conn = Connection::open(&pg()).await.unwrap();
    conn.execute("CREATE TEMP TABLE \"edit Żółw\" (tenant integer NOT NULL,id integer NOT NULL,name text,note text,payload bytea,generated_name text GENERATED ALWAYS AS (name || ':' || id::text) STORED,PRIMARY KEY(tenant,id) INCLUDE(name))",Page::ALL).await.unwrap();
    conn.execute(
        "INSERT INTO \"edit Żółw\"(tenant,id,name) VALUES (7,1,'before'),(8,1,'sentinel')",
        Page::ALL,
    )
    .await
    .unwrap();
    let result=conn.execute_with_context("SELECT id AS selected_id,tenant AS selected_tenant,name AS display_name,note,payload,generated_name,id+1 AS expression FROM \"edit Żółw\" ORDER BY tenant,id",Page::ALL,None).await.unwrap();
    let source = result.source.as_ref().unwrap();
    assert_ne!(source.session_epoch, 0);
    assert!(source.columns[6].is_none());
    assert_eq!(
        source.columns[0].as_ref().unwrap().column,
        OriginColumn::Postgres(2)
    );
    assert_eq!(
        source.columns[2].as_ref().unwrap().column,
        OriginColumn::Postgres(3)
    );
    let origin = &source.columns[0].as_ref().unwrap().table;
    let metadata = conn.table_edit_metadata(origin).await.unwrap();
    assert_eq!(metadata.table.name, "edit Żółw");
    assert!(metadata.table.schema.starts_with("pg_temp_"));
    assert_eq!(
        metadata.primary_key,
        ["tenant", "id"],
        "INCLUDE columns are not part of the primary key"
    );
    assert!(
        metadata
            .columns
            .iter()
            .find(|column| column.name == "generated_name")
            .unwrap()
            .generated
    );
    let payload = "O'Neil\\folder\nŻółw; -- SELECT 'x'";
    for mode in ["on", "off"] {
        conn.execute(
            &format!("SET standard_conforming_strings={mode}"),
            Page::ALL,
        )
        .await
        .unwrap();
        assert_eq!(
            conn.execute_bound_update(&BoundUpdate {
                sql:
                    "UPDATE \"edit Żółw\" SET name=$1,note=$2,payload=$3 WHERE tenant=$4 AND id=$5"
                        .into(),
                params: vec![
                    Value::Text(payload.into()),
                    Value::Null,
                    Value::Bytes(vec![0, 39, 92, 255]),
                    Value::Int(7),
                    Value::Int(1)
                ],
                source_row: 0,
            })
            .await
            .unwrap(),
            1
        );
        let actual = conn
            .execute(
                "SELECT name,note,payload FROM \"edit Żółw\" WHERE tenant=7 AND id=1",
                Page::ALL,
            )
            .await
            .unwrap();
        assert_eq!(
            actual.rows[0],
            [
                Value::Text(payload.into()),
                Value::Null,
                Value::Bytes(vec![0, 39, 92, 255])
            ]
        );
    }
    assert_eq!(
        conn.execute("SELECT name FROM \"edit Żółw\" WHERE tenant=8", Page::ALL)
            .await
            .unwrap()
            .rows[0][0],
        Value::Text("sentinel".into())
    );
    conn.execute(
        "CREATE TEMP TABLE wide_key (id numeric(40,0) PRIMARY KEY,note text)",
        Page::ALL,
    )
    .await
    .unwrap();
    conn.execute("INSERT INTO wide_key VALUES (1234567890123456789012345678901234567890,'first'),(1234567890123456789012345678901234567891,'second')",Page::ALL).await.unwrap();
    let wide = conn
        .execute_with_context("SELECT id,note FROM wide_key ORDER BY id", Page::ALL, None)
        .await
        .unwrap();
    assert!(
        !wide.source.as_ref().unwrap().exact_columns[0],
        "a numeric placeholder must never authorize a primary key"
    );
}

#[tokio::test]
async fn mysql_bound_updates_count_matches_and_reject_temporary_shadowing() {
    if let Err(error) = std::net::TcpStream::connect("127.0.0.1:53306") {
        eprintln!("skipping MySQL edit metadata: TCP probe failed: {error}");
        return;
    }
    let mut conn = Connection::open(&mysql()).await.unwrap();
    let table = format!("dbeast_edit_{}", uuid::Uuid::new_v4().simple());
    let test=std::panic::AssertUnwindSafe(async {
        conn.execute(&format!("CREATE TABLE `{table}` (tenant integer NOT NULL,id integer NOT NULL,name text,note text,payload blob,generated_name text GENERATED ALWAYS AS (CONCAT(name, ':', id)) STORED,PRIMARY KEY(tenant,id)) ENGINE=InnoDB"),Page::ALL).await.unwrap();
        conn.execute(&format!("INSERT INTO `{table}`(tenant,id,name) VALUES (7,1,'before'),(8,1,'sentinel')"),Page::ALL).await.unwrap();
        let query=format!("SELECT id AS selected_id,tenant AS selected_tenant,name AS display_name,note,payload,generated_name,id+1 AS expression FROM `{table}` ORDER BY tenant,id");
        let result=conn.execute_with_context(&query,Page::ALL,None).await.unwrap();
        let source=result.source.as_ref().unwrap();
        assert_ne!(source.session_epoch,0);
        assert_eq!(source.columns[0].as_ref().unwrap().column,OriginColumn::Named("id".into()));
        assert_eq!(source.columns[2].as_ref().unwrap().column,OriginColumn::Named("name".into()));
        assert!(source.columns[6].is_none());
        let origin=source.columns[0].as_ref().unwrap().table.clone();
        let metadata=conn.table_edit_metadata(&origin).await.unwrap();
        assert_eq!(metadata.primary_key,["tenant","id"]);
        assert!(metadata.transactional);
        assert!(metadata.columns.iter().find(|column|column.name=="generated_name").unwrap().generated);
        let payload="O'Neil\\folder\nŻółw; -- SELECT 'x'";
        let update=BoundUpdate {
            sql:format!("UPDATE `{table}` SET name=?,note=?,payload=? WHERE tenant=? AND id=?"),
            params:vec![Value::Text(payload.into()),Value::Null,Value::Bytes(vec![0,39,92,255]),Value::Int(7),Value::Int(1)],source_row:0,
        };
        for sql_mode in ["", "NO_BACKSLASH_ESCAPES"] {
            conn.execute(&format!("SET SESSION sql_mode='{sql_mode}'"),Page::ALL).await.unwrap();
            assert_eq!(conn.execute_bound_update(&update).await.unwrap(),1);
            assert_eq!(conn.execute_bound_update(&update).await.unwrap(),1,"no-op must report one matched row");
            let actual=conn.execute(&format!("SELECT name,note,payload FROM `{table}` WHERE tenant=7 AND id=1"),Page::ALL).await.unwrap();
            assert_eq!(actual.rows[0],[Value::Text(payload.into()),Value::Null,Value::Bytes(vec![0,39,92,255])]);
        }
        assert_eq!(conn.execute(&format!("SELECT name FROM `{table}` WHERE tenant=8"),Page::ALL).await.unwrap().rows[0][0],Value::Text("sentinel".into()));
        let wide=conn.execute_with_context("SELECT CAST(18446744073709551615 AS UNSIGNED) AS wide_key",Page::ALL,None).await.unwrap();
        assert_eq!(wide.rows[0][0],Value::Decimal("18446744073709551615".into()));
        assert!(wide.source.as_ref().unwrap().exact_columns[0],"unsigned values must never wrap through i64");
        conn.execute(&format!("CREATE TEMPORARY TABLE `{table}` (id integer PRIMARY KEY,name text)"),Page::ALL).await.unwrap();
        conn.execute(&format!("INSERT INTO `{table}` VALUES (1,'temporary sentinel')"),Page::ALL).await.unwrap();
        let temporary=conn.execute_with_context(&format!("SELECT id,name FROM `{table}`"),Page::ALL,None).await.unwrap();
        let temp_origin=&temporary.source.as_ref().unwrap().columns[0].as_ref().unwrap().table;
        assert!(conn.table_edit_metadata(temp_origin).await.is_err(),"permanent table metadata must not authorize updates to a temporary namesake");
    }).catch_unwind().await;
    conn.execute(
        &format!("DROP TEMPORARY TABLE IF EXISTS `{table}`"),
        Page::ALL,
    )
    .await
    .unwrap();
    conn.execute(&format!("DROP TABLE IF EXISTS `{table}`"), Page::ALL)
        .await
        .unwrap();
    if let Err(panic) = test {
        std::panic::resume_unwind(panic);
    }
}

async fn expand(conn: &mut Connection, node: &DbObject) -> Vec<DbObject> {
    conn.children(Some(node)).await.unwrap()
}

fn find<'a>(nodes: &'a [DbObject], name: &str) -> &'a DbObject {
    nodes.iter().find(|n| n.name == name).unwrap_or_else(|| {
        panic!(
            "{name} not in {:?}",
            nodes.iter().map(|n| &n.name).collect::<Vec<_>>()
        )
    })
}

fn folder<'a>(nodes: &'a [DbObject], name: &str) -> &'a [DbObject] {
    find(nodes, name).children.as_deref().unwrap()
}

#[tokio::test]
async fn postgres_queries_and_types() {
    let Some(mut conn) = open(&pg()).await else {
        return;
    };

    let r = conn
        .execute("select o.total, o.status, o.paid, o.ref, o.ip, o.duration, c.tags, c.meta, c.created_at, null::text as nothing from shop.orders o join shop.customers c on c.id = o.customer_id order by o.id", Page::first(10)).await
        .unwrap();
    assert_eq!(r.rows.len(), 10);
    assert!(r.truncated);
    assert_eq!(r.columns[1].type_name, "status");
    let row = &r.rows[0];
    assert_eq!(row[0], Value::Decimal("13.37".into()));
    assert_eq!(row[1], Value::Text("paid".into()), "enum decoded as text");
    assert_eq!(row[2], Value::Bool(false));
    assert!(matches!(&row[3], Value::Text(u) if u.len() == 36));
    assert_eq!(row[4], Value::Text("10.0.0.1".into()));
    assert_eq!(row[5], Value::Text("01:01:00".into()));
    assert_eq!(row[6], Value::Text("{a,b2}".into()));
    assert!(matches!(&row[7], Value::Text(j) if j.contains("\"vip\"")));
    assert_eq!(row[9], Value::Null);

    let r = conn
        .execute(
            "update shop.orders set paid = paid where id <= 3",
            Page::first(500),
        )
        .await
        .unwrap();
    assert_eq!(r.affected, Some(3));

    let err = conn
        .execute("select * from no_such_table", Page::first(500))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no_such_table"), "{err}");
    // The session survives an error.
    assert_eq!(
        conn.execute("select 1", Page::first(1)).await.unwrap().rows[0][0],
        Value::Int(1)
    );
}

#[tokio::test]
async fn postgres_explorer_tree() {
    let Some(mut conn) = open(&pg()).await else {
        return;
    };
    let roots = conn.children(None).await.unwrap();
    assert_eq!(roots[0].kind, ObjectKind::Database);
    let schemas = expand(&mut conn, &roots[0]).await;
    let shop = find(&schemas, "shop");
    let objects = expand(&mut conn, shop).await;
    let tables = folder(&objects, "tables");
    assert!(
        folder(&objects, "views")
            .iter()
            .any(|v| v.name == "customer_totals")
    );
    assert!(
        folder(&objects, "routines")
            .iter()
            .any(|f| f.name == "order_count(cid integer)")
    );
    let orders = find(tables, "orders");
    assert_eq!(orders.path, ["dbeast", "shop", "orders"]);

    let details = expand(&mut conn, orders).await;
    let cols = folder(&details, "columns");
    assert_eq!(cols[0].name, "id");
    assert!(
        cols[0].detail.starts_with("bigint not null = nextval"),
        "{}",
        cols[0].detail
    );
    assert!(
        folder(&details, "keys")
            .iter()
            .any(|k| k.detail.starts_with("FOREIGN KEY (customer_id)"))
    );
    assert!(
        folder(&details, "indexes")
            .iter()
            .any(|i| i.name == "orders_customer_idx")
    );
}

#[tokio::test]
async fn mysql_queries_and_tree() {
    let Some(mut conn) = open(&mysql()).await else {
        return;
    };
    let r = conn.execute("select c.name, c.bio, c.avatar, o.total, o.paid, o.score, o.placed_on, c.id from customers c join orders o on o.customer_id = c.id order by o.id", Page::first(500)).await.unwrap();
    assert_eq!(r.rows.len(), 3);
    assert_eq!(r.rows[0][0], Value::Text("Zażółć".into()));
    assert_eq!(
        r.rows[0][1],
        Value::Text("line1\nline2".into()),
        "TEXT is not binary"
    );
    assert_eq!(r.rows[0][2], Value::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]));
    assert_eq!(r.rows[0][3], Value::Decimal("10.50".into()));
    assert_eq!(r.rows[0][4], Value::Int(1));
    assert_eq!(r.rows[2][5], Value::Float(-2.25));
    assert_eq!(r.rows[0][6], Value::Text("2026-10-01".into()));
    assert_eq!(r.rows[1][6], Value::Null);
    assert_eq!(r.rows[0][7], Value::Int(1));

    let r = conn
        .execute(
            "update orders set paid = paid + 0 where customer_id = 1",
            Page::first(500),
        )
        .await
        .unwrap();
    assert!(r.affected.is_some() && !r.has_result_set());

    let roots = conn.children(None).await.unwrap();
    let shop = find(&roots, "shop");
    let objects = expand(&mut conn, shop).await;
    let orders = find(folder(&objects, "tables"), "orders");
    let details = expand(&mut conn, orders).await;
    assert!(
        folder(&details, "columns")
            .iter()
            .any(|c| c.name == "total" && c.detail.starts_with("decimal(12,2) not null"))
    );
    assert!(
        folder(&details, "keys")
            .iter()
            .any(|k| k.name == "fk_customer")
    );
    assert!(
        folder(&details, "indexes")
            .iter()
            .any(|i| i.name == "orders_customer_idx")
    );
}

#[tokio::test]
async fn sqlite_end_to_end() {
    let dir = std::env::temp_dir().join(format!("dbeast_test_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut ds = DataSource::new(DriverKind::Sqlite);
    ds.file = dir.join("t.sqlite").to_string_lossy().into_owned();
    let mut conn = Connection::open(&ds).await.unwrap();

    let script =
        "create table a (id integer primary key, name text not null, data blob, score real);
        create index a_name on a(name);
        create table b (id integer primary key, a_id integer references a(id));
        create trigger tr after insert on a begin insert into b (a_id) values (new.id); end;
        insert into a (name, data, score) values ('x', x'0102', 1.5), ('y', null, null);";
    for stmt in dbeast::splitter::split(script, DriverKind::Sqlite) {
        conn.execute(&stmt.text, Page::first(500)).await.unwrap();
    }
    let r = conn
        .execute(
            "select a.*, (select count(*) from b) as b_count from a",
            Page::first(500),
        )
        .await
        .unwrap();
    assert_eq!(r.columns[1].type_name, "text");
    assert_eq!(
        r.rows[0],
        vec![
            Value::Int(1),
            Value::Text("x".into()),
            Value::Bytes(vec![1, 2]),
            Value::Float(1.5),
            Value::Int(2)
        ]
    );

    let roots = conn.children(None).await.unwrap();
    let objects = expand(&mut conn, find(&roots, "main")).await;
    let details = expand(&mut conn, find(folder(&objects, "tables"), "b")).await;
    assert!(folder(&details, "keys")[0].detail.contains("-> a"));
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn postgres_paging_keys_meta_and_ddl() {
    let Some(mut conn) = open(&pg()).await else {
        return;
    };
    let page = Page { skip: 10, limit: 5 };
    let r = conn
        .execute("select id from shop.customers order by id", page)
        .await
        .unwrap();
    assert_eq!(
        r.rows.iter().map(|r| r[0].clone()).collect::<Vec<_>>(),
        (11..=15).map(Value::Int).collect::<Vec<_>>()
    );
    assert!(r.truncated);

    assert_eq!(
        conn.primary_key(Some("shop"), "orders").await.unwrap(),
        ["id"]
    );
    assert!(
        conn.primary_key(Some("shop"), "customer_totals")
            .await
            .unwrap()
            .is_empty()
    );

    let meta = conn.completion_meta().await.unwrap();
    let orders = meta
        .iter()
        .find(|t| t.schema == "shop" && t.name == "orders")
        .unwrap();
    assert_eq!(
        orders.columns[2],
        ("total".to_string(), "numeric(12,2)".to_string())
    );
    assert!(
        meta.iter()
            .any(|t| t.name == "customer_totals" && t.is_view)
    );
    assert!(!meta.iter().any(|t| t.schema == "pg_catalog"));

    let roots = conn.children(None).await.unwrap();
    let schemas = expand(&mut conn, &roots[0]).await;
    let objects = expand(&mut conn, find(&schemas, "shop")).await;
    let ddl = conn
        .ddl(find(folder(&objects, "tables"), "orders"))
        .await
        .unwrap();
    assert!(
        ddl.starts_with("CREATE TABLE shop.orders (\n    id bigint DEFAULT nextval"),
        "{ddl}"
    );
    assert!(ddl.contains("CONSTRAINT orders_customer_id_fkey FOREIGN KEY (customer_id) REFERENCES shop.customers(id)"), "{ddl}");
    assert!(
        ddl.contains(
            "CREATE INDEX orders_customer_idx ON shop.orders USING btree (customer_id, placed_on);"
        ),
        "{ddl}"
    );
    let view = conn
        .ddl(find(folder(&objects, "views"), "customer_totals"))
        .await
        .unwrap();
    assert!(
        view.starts_with("CREATE OR REPLACE VIEW shop.customer_totals AS\n SELECT"),
        "{view}"
    );
    let func = conn
        .ddl(find(
            folder(&objects, "routines"),
            "order_count(cid integer)",
        ))
        .await
        .unwrap();
    assert!(
        func.starts_with("CREATE OR REPLACE FUNCTION shop.order_count(cid integer)"),
        "{func}"
    );
    let seq = conn
        .ddl(find(folder(&objects, "sequences"), "orders_id_seq"))
        .await
        .unwrap();
    assert!(
        seq.starts_with("CREATE SEQUENCE shop.orders_id_seq"),
        "{seq}"
    );
}

#[tokio::test]
async fn mysql_paging_keys_meta_and_ddl() {
    let Some(mut conn) = open(&mysql()).await else {
        return;
    };
    let r = conn
        .execute(
            "select id from orders order by id",
            Page { skip: 1, limit: 1 },
        )
        .await
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(2)]]);
    assert!(r.truncated);
    assert_eq!(conn.primary_key(None, "orders").await.unwrap(), ["id"]);
    let meta = conn.completion_meta().await.unwrap();
    assert!(meta.iter().any(|t| t.schema == "shop"
        && t.name == "customers"
        && t.columns.iter().any(|(c, _)| c == "email")));
    let roots = conn.children(None).await.unwrap();
    let objects = expand(&mut conn, find(&roots, "shop")).await;
    let ddl = conn
        .ddl(find(folder(&objects, "tables"), "orders"))
        .await
        .unwrap();
    assert!(
        ddl.starts_with("CREATE TABLE `orders`") && ddl.contains("CONSTRAINT `fk_customer`"),
        "{ddl}"
    );
}

#[tokio::test]
async fn sqlite_paging_keys_meta_and_ddl() {
    let dir = std::env::temp_dir().join(format!("dbeast_meta_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut ds = DataSource::new(DriverKind::Sqlite);
    ds.file = dir.join("t.sqlite").to_string_lossy().into_owned();
    let mut conn = Connection::open(&ds).await.unwrap();
    for sql in [
        "create table k (a int, b text, c real, primary key (b, a))",
        "create index k_c on k(c)",
        "insert into k values (1, 'x', 0), (2, 'y', 0), (3, 'z', 0)",
    ] {
        conn.execute(sql, Page::first(0)).await.unwrap();
    }
    let r = conn
        .execute("select a from k order by a", Page { skip: 2, limit: 10 })
        .await
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(3)]]);
    assert!(!r.truncated);
    assert_eq!(conn.primary_key(None, "k").await.unwrap(), ["b", "a"]);
    let meta = conn.completion_meta().await.unwrap();
    assert_eq!(meta[0].columns.len(), 3);
    assert_eq!(meta[0].columns[1], ("b".to_string(), "text".to_string()));
    let roots = conn.children(None).await.unwrap();
    let objects = expand(&mut conn, find(&roots, "main")).await;
    let ddl = conn
        .ddl(find(folder(&objects, "tables"), "k"))
        .await
        .unwrap();
    // SQLite stores the original text but upper-cases the leading keywords.
    assert_eq!(
        ddl,
        "CREATE TABLE k (a int, b text, c real, primary key (b, a));\n\nCREATE INDEX k_c on k(c);"
    );
    std::fs::remove_dir_all(dir).ok();
}

/// Values without an exact typed form are shown exactly as PostgreSQL prints them.
#[tokio::test]
async fn postgres_values_match_the_server_text() {
    let Some(mut conn) = open(&pg()).await else { return };
    let expressions = [
        r#"array['a,b', 'NULL', NULL, '', 'say "hi"', 'back\slash', '{x}', ' y ']"#,
        "'{{1,2},{3,NULL}}'::int[]",
        "'[0:1]={x,y}'::text[]",
        "'{0.1,1e20,-Infinity,NaN}'::float8[]",
        "'{1.5,1e10}'::float4[]",
        "'{t,f}'::bool[]",
        "'{2024-01-02,infinity}'::date[]",
        r#"'{"2024-01-02 03:04:05.5"}'::timestamp[]"#,
        "'{1 day,-00:00:01}'::interval[]",
        "'{(1,2),(3,4);(5,6),(7,8)}'::box[]",
        "'{a5d4e0a0-8b1c-4f3e-9a76-2d3c4b5a6f70}'::uuid[]",
        r#"'{"\\x00ff"}'::bytea[]"#,
        "1e40::numeric",
        "'NaN'::numeric",
        "'-Infinity'::numeric",
        "0.1234567890123456789012345678901::numeric",
        "'1 year 2 mons -3 days -00:00:00.5'::interval",
        "'-1 hour -30 minutes'::interval",
        "'1 2'::int2vector",
        "'26 23'::oidvector",
        "'x'::\"char\"",
        "'abc'::name",
        "'[1,5)'::int4range",
        "'{[1,2),[5,6)}'::int4multirange",
        r#"'["2024-01-01 00:00","2024-01-02 00:00")'::tsrange"#,
        "'empty'::numrange",
        "row(1, 'a b', NULL, '')",
        "'(1.5,2)'::point",
        "'((0,0),(1,1))'::box",
        "'<(1,2),3>'::circle",
        "'[(0,0),(1,1)]'::lseg",
        "'[(0,0),(1,1),(2,0)]'::path",
        "'((0,0),(1,1),(2,0))'::polygon",
        "'{1,-1,0}'::line",
        "'08:00:2b:01:02:03'::macaddr",
        "'08:00:2b:01:02:03:04:05'::macaddr8",
        "'10.0.0.1/8'::inet",
        "'10.0.0.0/8'::cidr",
        "'::1'::inet",
        "B'101'",
        "'1011'::varbit",
        "'16/B374D848'::pg_lsn",
        "'5'::xid8",
        "'12:00:00+02'::timetz",
        "'24:00:00'::time",
        "'infinity'::date",
        "'-infinity'::timestamp",
        "'0044-03-15 12:00 BC'::timestamp",
        "'$.a'::jsonpath",
        "''::text",
    ];
    let sql = format!(
        "select {}",
        expressions
            .iter()
            // format() uses the type's output function; a cast to text may not (inet).
            .map(|e| format!("{e}, pg_catalog.format('%s', {e})"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let r = conn.execute_with_context(&sql, Page::first(1), None).await.unwrap();
    for (i, expression) in expressions.iter().enumerate() {
        let shown = match &r.rows[0][2 * i] {
            Value::Text(text) | Value::Decimal(text) => text.clone(),
            other => panic!("{expression}: {other:?}"),
        };
        assert_eq!(r.rows[0][2 * i + 1], Value::Text(shown), "{expression}");
    }
}

/// Time until a page is ready and the session is free again, with the server
/// limiting the work and without it.
#[tokio::test]
#[ignore = "measurement against the test PostgreSQL: cargo test --test drivers postgres_paging_cost -- --ignored --nocapture"]
async fn postgres_paging_cost() {
    let Some(mut conn) = open(&pg()).await else { return };
    let page = Page::first(500);
    let time = async |conn: &mut Connection, sql: &str, page: Page| {
        let mut best = std::time::Duration::MAX;
        for _ in 0..5 {
            let started = std::time::Instant::now();
            conn.execute_with_context(sql, page, None).await.unwrap();
            best = best.min(started.elapsed());
        }
        best
    };
    // The qualified call is not cancelled: the session reads all 2 million rows.
    let full = time(&mut conn, "select pg_catalog.generate_series(1, 2000000) as g", page).await;
    let stopped = time(&mut conn, "select generate_series(1, 2000000) as g", page).await;
    eprintln!("console page of a 2 million row SELECT: {full:?} read to the end, {stopped:?} stopped after the page");
    for rows in [600, 5000] {
        let full = time(&mut conn, &format!("select pg_catalog.generate_series(1, {rows}) as g"), page).await;
        let stopped = time(&mut conn, &format!("select generate_series(1, {rows}) as g"), page).await;
        eprintln!("console page of a {rows} row SELECT: {full:?} read to the end, {stopped:?} with a cancel request");
    }
    conn.execute("create table dbeast_paging_cost (id int primary key, note text)", Page::first(0)).await.unwrap();
    conn.execute(
        "insert into dbeast_paging_cost select g, md5(g::text) from generate_series(1, 20000) g",
        Page::first(0),
    )
    .await
    .unwrap();
    let base = r#"SELECT * FROM "public"."dbeast_paging_cost""#;
    for skip in [0, 19_800, 25_000] {
        let window = Page { skip, limit: 500 };
        let client = time(&mut conn, base, window).await;
        let (sql, server_window) = dbeast::drivers::page_query(DriverKind::Postgres, base, &["id".into()], window);
        let server = time(&mut conn, &sql, server_window).await;
        let mut limited = std::time::Duration::MAX;
        for _ in 0..5 {
            let started = std::time::Instant::now();
            conn.execute_limited_with_context(&sql, server_window, None).await.unwrap();
            limited = limited.min(started.elapsed());
        }
        eprintln!("table page at offset {skip} of 20000 rows: {client:?} skipped by the client, {server:?} with LIMIT/OFFSET and a cancel request, {limited:?} with LIMIT/OFFSET read to the end");
    }
    let mut key = std::time::Duration::MAX;
    for _ in 0..5 {
        let started = std::time::Instant::now();
        conn.primary_key(Some("public"), "dbeast_paging_cost").await.unwrap();
        key = key.min(started.elapsed());
    }
    eprintln!("primary key lookup before each table page: {key:?}");
    conn.execute("drop table dbeast_paging_cost", Page::first(0)).await.unwrap();
}

/// Forwards TCP connections to the test PostgreSQL and counts them.
async fn counting_proxy() -> (u16, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = accepted.clone();
    tokio::spawn(async move {
        while let Ok((mut client, _)) = listener.accept().await {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::spawn(async move {
                if let Ok(mut server) = tokio::net::TcpStream::connect("127.0.0.1:55432").await {
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
                }
            });
        }
    });
    (port, accepted)
}

/// Table pages carry their own LIMIT, so the row after the page is read and
/// no cancel connection is opened; a console SELECT still stops early.
#[tokio::test]
async fn postgres_limited_pages_open_no_cancel_connection() {
    if std::net::TcpStream::connect("127.0.0.1:55432").is_err() {
        eprintln!("skipping, PostgreSQL unavailable");
        return;
    }
    let (port, accepted) = counting_proxy().await;
    let mut ds = pg();
    ds.host = "127.0.0.1".into();
    ds.port = port;
    let mut conn = Connection::open(&ds).await.unwrap();
    let opened = accepted.load(std::sync::atomic::Ordering::SeqCst);
    let page = Page { skip: 1000, limit: 500 };
    let base = "select g from generate_series(1, 5000) g";
    let (sql, window) = dbeast::drivers::page_query(DriverKind::Postgres, base, &[], page);
    for _ in 0..3 {
        let r = conn.execute_limited_with_context(&sql, window, None).await.unwrap();
        assert_eq!((r.rows.len(), r.truncated), (500, true));
        assert_eq!(r.rows[0][0], Value::Int(1001));
    }
    assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), opened, "no cancel connections");
    // An error in the row after the page still fails the page.
    let failing = "select g, 1 / (g - 1501) from generate_series(1, 5000) g";
    let (sql_error, window) = dbeast::drivers::page_query(DriverKind::Postgres, failing, &[], page);
    let error = conn.execute_limited_with_context(&sql_error, window, None).await.unwrap_err();
    assert!(error.to_string().contains("division by zero"), "{error}");
    let next = conn.execute_with_context("select 42", Page::first(500), None).await.unwrap();
    assert_eq!(next.rows, vec![vec![Value::Int(42)]], "the next statement is not cancelled");
    // The general path for console SQL keeps its early cancel.
    let r = conn.execute_with_context(base, Page::first(500), None).await.unwrap();
    assert_eq!((r.rows.len(), r.truncated), (500, true));
    assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), opened + 1, "one cancel connection");
}

/// Stop between statements of a batch: the mutations before it ran, none
/// after it was sent, and the index of the first unsent one is reported.
#[tokio::test]
async fn update_batches_stop_between_statements() {
    let directory = std::env::temp_dir().join(format!("dbeast_batch_stop_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let mut sqlite = DataSource::new(DriverKind::Sqlite);
    sqlite.file = directory.join("stop.sqlite").to_string_lossy().into_owned();
    let mut sources = vec![sqlite];
    for ds in [pg(), mysql()] {
        if std::net::TcpStream::connect(("127.0.0.1", ds.port)).is_ok() {
            sources.push(ds);
        } else {
            eprintln!("skipping {:?} batch stop", ds.driver);
        }
    }
    for ds in sources {
        let mut conn = Connection::open(&ds).await.unwrap();
        let table = format!("dbeast_batch_stop_{}", uuid::Uuid::new_v4().simple());
        let marker = if ds.driver == DriverKind::Postgres { "$1" } else { "?" };
        conn.execute(&format!("create table {table} (id int primary key, n int)"), Page::first(0)).await.unwrap();
        let values = (1..=100).map(|id| format!("({id}, 0)")).collect::<Vec<_>>().join(", ");
        conn.execute(&format!("insert into {table} values {values}"), Page::first(0)).await.unwrap();
        let updates: Vec<BoundUpdate> = (1..=100)
            .map(|id| BoundUpdate {
                sql: format!("update {table} set n = 1 where id = {marker}"),
                params: vec![Value::Int(id)],
                source_row: id as usize - 1,
            })
            .collect();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = calls.clone();
        let stopped = std::sync::Arc::new(move || counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 40);
        let (index, error) = conn.execute_bound_updates(&updates, stopped).await.unwrap_err();
        assert_eq!(index, 40, "{:?}", ds.driver);
        assert!(error.to_string().contains("Stopped"), "{error}");
        let r = conn.execute(&format!("select sum(n) from {table}"), Page::first(1)).await.unwrap();
        let sum = match &r.rows[0][0] {
            Value::Int(n) => *n,
            Value::Decimal(n) | Value::Text(n) => n.parse().unwrap(),
            other => panic!("{other:?}"),
        };
        assert_eq!(sum, 40, "{:?}: only the mutations before Stop ran", ds.driver);
        conn.execute(&format!("drop table {table}"), Page::first(0)).await.unwrap();
    }
    std::fs::remove_dir_all(directory).ok();
}

/// Forwards to the test PostgreSQL, delaying every chunk by `delay` in each
/// direction, like a remote server.
async fn delaying_proxy(delay: std::time::Duration) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    async fn pump(mut from: tokio::net::tcp::OwnedReadHalf, mut to: tokio::net::tcp::OwnedWriteHalf, delay: std::time::Duration) {
        let mut buffer = vec![0u8; 64 * 1024];
        while let Ok(read) = from.read(&mut buffer).await {
            if read == 0 {
                break;
            }
            tokio::time::sleep(delay).await;
            if to.write_all(&buffer[..read]).await.is_err() {
                break;
            }
        }
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let Ok(server) = tokio::net::TcpStream::connect("127.0.0.1:55432").await else { continue };
            let (client_read, client_write) = client.into_split();
            let (server_read, server_write) = server.into_split();
            tokio::spawn(pump(client_read, server_write, delay));
            tokio::spawn(pump(server_read, client_write, delay));
        }
    });
    port
}

/// 100 UPDATEs over a link with 2 ms one-way delay: one at a time, and
/// pipelined by `execute_bound_updates`.
#[tokio::test]
#[ignore = "manual measurement: cargo test --release --test drivers postgres_update_batch_latency -- --ignored --nocapture"]
async fn postgres_update_batch_latency() {
    if std::net::TcpStream::connect("127.0.0.1:55432").is_err() {
        eprintln!("skipping, PostgreSQL unavailable");
        return;
    }
    let mut ds = pg();
    ds.host = "127.0.0.1".into();
    ds.port = delaying_proxy(std::time::Duration::from_millis(2)).await;
    let mut conn = Connection::open(&ds).await.unwrap();
    let table = format!("dbeast_batch_cost_{}", uuid::Uuid::new_v4().simple());
    conn.execute(&format!("create table {table} (id int primary key, n int)"), Page::first(0)).await.unwrap();
    conn.execute(&format!("insert into {table} select g, 0 from generate_series(1, 100) g"), Page::first(0)).await.unwrap();
    let updates: Vec<BoundUpdate> = (1..=100)
        .map(|id| BoundUpdate {
            sql: format!("update {table} set n = n + 1 where id = $1"),
            params: vec![Value::Int(id)],
            source_row: id as usize - 1,
        })
        .collect();
    let mut best = [std::time::Duration::MAX; 2];
    for _ in 0..5 {
        let started = std::time::Instant::now();
        for update in &updates {
            assert_eq!(conn.execute_bound_update(update).await.unwrap(), 1);
        }
        best[0] = best[0].min(started.elapsed());
        let started = std::time::Instant::now();
        conn.execute_bound_updates(&updates, std::sync::Arc::new(|| false)).await.map_err(|(_, error)| error).unwrap();
        best[1] = best[1].min(started.elapsed());
    }
    let r = conn.execute(&format!("select min(n), max(n) from {table}"), Page::first(1)).await.unwrap();
    assert_eq!(r.rows[0], vec![Value::Int(10), Value::Int(10)]);
    conn.execute(&format!("drop table {table}"), Page::first(0)).await.unwrap();
    eprintln!("100 updates over 2 ms one-way delay: {:?} one at a time, {:?} pipelined", best[0], best[1]);
}

/// Errors in, at and after the page boundary (page size 500) fail the
/// statement, in autocommit and inside a transaction block.
#[tokio::test]
async fn postgres_errors_after_the_page_are_never_reported_as_success() {
    let Some(mut conn) = open(&pg()).await else { return };
    let page = Page::first(500);
    for n in [300, 500, 501, 800, 1001] {
        // A qualified call is never cancelled after the page: the whole result is read.
        let sql = format!("select g, 1 / (g - {n}) from pg_catalog.generate_series(1, 1200) g");
        let error = conn.execute_with_context(&sql, page, None).await.unwrap_err();
        assert!(error.to_string().contains("division by zero"), "row {n}: {error}");
        // Inside a block the plain read is not cancelled either.
        conn.execute("BEGIN", Page::first(0)).await.unwrap();
        let sql = format!("select g, 1 / (g - {n}) from generate_series(1, 1200) g");
        let error = conn.execute_with_context(&sql, page, None).await.unwrap_err();
        assert!(error.to_string().contains("division by zero"), "row {n} in a block: {error}");
        conn.execute("ROLLBACK", Page::first(0)).await.unwrap();
    }
    for n in [300, 500, 501] {
        // Rows up to the one after the page are always read.
        let sql = format!("select g, 1 / (g - {n}) from generate_series(1, 1200) g");
        let error = conn.execute_with_context(&sql, page, None).await.unwrap_err();
        assert!(error.to_string().contains("division by zero"), "cancellable, row {n}: {error}");
    }
    let r = conn
        .execute_with_context("select g, 1 / (g - 5000) from pg_catalog.generate_series(1, 1200) g", page, None)
        .await
        .unwrap();
    assert_eq!((r.rows.len(), r.truncated), (500, true));
    let r = conn
        .execute_with_context("select g from generate_series(1, 500) g", page, None)
        .await
        .unwrap();
    assert_eq!((r.rows.len(), r.truncated), (500, false));
}

/// A plain read stops on the server once its page is read; DML with RETURNING
/// always runs to completion, and no late cancel hits the next statement.
#[tokio::test]
async fn postgres_stops_only_plain_reads_after_their_page() {
    let Some(mut conn) = open(&pg()).await else { return };
    let page = Page::first(500);
    let started = std::time::Instant::now();
    let r = conn
        .execute_with_context("select generate_series(1, 300000000) as g", page, None)
        .await
        .unwrap();
    let elapsed = started.elapsed();
    eprintln!("first page of a 300 million row SELECT: {elapsed:?}");
    assert_eq!((r.rows.len(), r.truncated), (500, true));
    assert!(elapsed < std::time::Duration::from_secs(10), "the server kept producing rows: {elapsed:?}");
    // Statements that end right after their page race with the cancel request.
    for _ in 0..30 {
        let r = conn
            .execute_with_context("select generate_series(1, 520) as g", page, None)
            .await
            .unwrap();
        assert_eq!((r.rows.len(), r.truncated), (500, true));
        conn.execute_with_context("select pg_catalog.pg_sleep(0.002)", Page::first(1), None)
            .await
            .unwrap();
    }
    conn.execute("create temp table dbeast_returning_page (id int)", Page::first(0))
        .await
        .unwrap();
    let r = conn
        .execute_with_context(
            "insert into dbeast_returning_page select generate_series(1, 3000) returning id",
            page,
            None,
        )
        .await
        .unwrap();
    assert_eq!((r.rows.len(), r.truncated), (500, true));
    let r = conn
        .execute_with_context(
            "with d as (delete from dbeast_returning_page where id > 1000 returning id) select id from d",
            page,
            None,
        )
        .await
        .unwrap();
    assert_eq!((r.rows.len(), r.truncated), (500, true));
    let count = conn
        .execute_with_context("select count(*) from dbeast_returning_page", Page::first(1), None)
        .await
        .unwrap();
    assert_eq!(count.rows[0][0], Value::Int(1000));
    conn.execute("drop table dbeast_returning_page", Page::first(0)).await.unwrap();
}

/// Forwards connections to the test MySQL and counts bytes sent to the client.
async fn mysql_byte_counting_proxy() -> (u16, std::sync::Arc<std::sync::atomic::AtomicU64>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let received = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let counter = received.clone();
    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let Ok(server) = tokio::net::TcpStream::connect("127.0.0.1:53306").await else { continue };
            let (mut client_read, mut client_write) = client.into_split();
            let (mut server_read, mut server_write) = server.into_split();
            tokio::spawn(async move {
                let _ = tokio::io::copy(&mut client_read, &mut server_write).await;
                let _ = server_write.shutdown().await;
            });
            let counter = counter.clone();
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 64 * 1024];
                while let Ok(read) = server_read.read(&mut buffer).await {
                    if read == 0 || client_write.write_all(&buffer[..read]).await.is_err() {
                        break;
                    }
                    counter.fetch_add(read as u64, std::sync::atomic::Ordering::SeqCst);
                }
            });
        }
    });
    (port, received)
}

async fn mysql_numbers_table(conn: &mut Connection, rows: usize, width: usize) -> String {
    let table = format!("dbeast_page_{}", uuid::Uuid::new_v4().simple());
    conn.execute(&format!("create table {table} (id int primary key, note varchar({width}))"), Page::first(0)).await.unwrap();
    let mut id = 0;
    while id < rows {
        let chunk = (id + 1..=(id + 1000).min(rows))
            .map(|n| format!("({n}, repeat('x', {width}))"))
            .collect::<Vec<_>>()
            .join(",");
        conn.execute(&format!("insert into {table} values {chunk}"), Page::first(0)).await.unwrap();
        id += 1000;
    }
    table
}

/// A console SELECT outside a transaction ends on the server one row after
/// its page; the bytes received do not depend on the size of the result.
#[tokio::test]
async fn mysql_console_pages_end_on_the_server() {
    if std::net::TcpStream::connect("127.0.0.1:53306").is_err() {
        eprintln!("skipping, MySQL unavailable");
        return;
    }
    let (port, received) = mysql_byte_counting_proxy().await;
    let mut ds = mysql();
    ds.port = port;
    let mut conn = Connection::open(&ds).await.unwrap();
    let table = mysql_numbers_table(&mut conn, 20_000, 200).await;
    let select = format!("select id, note from {table} order by id");
    let page = Page { skip: 1000, limit: 500 };
    let before = received.load(std::sync::atomic::Ordering::SeqCst);
    let r = conn.execute_with_context(&select, page, None).await.unwrap();
    let bounded = received.load(std::sync::atomic::Ordering::SeqCst) - before;
    assert_eq!((r.rows.len(), r.truncated), (500, true));
    assert_eq!(r.rows[0][0], Value::Int(1001));
    assert!(r.source.as_ref().unwrap().columns.iter().all(Option::is_some), "column origins kept");
    // The same statement read in full, as before, for comparison.
    let before = received.load(std::sync::atomic::Ordering::SeqCst);
    let full = conn.execute(&select, page).await.unwrap();
    let drained = received.load(std::sync::atomic::Ordering::SeqCst) - before;
    assert_eq!(full.rows, r.rows);
    assert!(bounded * 5 < drained, "bounded {bounded} B, drained {drained} B");
    // The last page is complete and not marked truncated.
    let last = conn.execute_with_context(&select, Page { skip: 19_500, limit: 500 }, None).await.unwrap();
    assert_eq!((last.rows.len(), last.truncated), (500, false));
    // The statement's own LIMIT still applies, and the next statement runs normally.
    let own = conn.execute_with_context(&format!("{select} limit 3"), page, None).await.unwrap();
    assert!(own.rows.is_empty() && !own.truncated);
    let next = conn.execute_with_context("select 42", Page::first(500), None).await.unwrap();
    assert_eq!(next.rows, vec![vec![Value::Int(42)]]);
    // Inside a transaction and for locking reads the full result is read.
    conn.execute("begin", Page::first(0)).await.unwrap();
    let before = received.load(std::sync::atomic::Ordering::SeqCst);
    let in_tx = conn.execute_with_context(&select, page, None).await.unwrap();
    assert!(received.load(std::sync::atomic::Ordering::SeqCst) - before >= drained);
    assert_eq!(in_tx.rows, r.rows);
    conn.execute("rollback", Page::first(0)).await.unwrap();
    let locking = conn.execute_with_context(&format!("{select} for share"), page, None).await.unwrap();
    assert_eq!((locking.rows.len(), locking.truncated), (500, true));
    conn.execute(&format!("drop table {table}"), Page::first(0)).await.unwrap();
}

/// First console page of a large MySQL result, read in full and ended on
/// the server.
#[tokio::test]
#[ignore = "manual measurement: cargo test --release --test drivers mysql_console_page_cost -- --ignored --nocapture"]
async fn mysql_console_page_cost() {
    if std::net::TcpStream::connect("127.0.0.1:53306").is_err() {
        eprintln!("skipping, MySQL unavailable");
        return;
    }
    let (port, received) = mysql_byte_counting_proxy().await;
    let mut ds = mysql();
    ds.port = port;
    let mut conn = Connection::open(&ds).await.unwrap();
    for rows in [10_000, 1_000_000] {
        let table = mysql_numbers_table(&mut conn, rows, 100).await;
        let select = format!("select id, note from {table}");
        let mut runs = Vec::new();
        for bounded in [false, true] {
            let mut best = (std::time::Duration::MAX, 0);
            for _ in 0..5 {
                let before = received.load(std::sync::atomic::Ordering::SeqCst);
                let started = std::time::Instant::now();
                let r = if bounded {
                    conn.execute_with_context(&select, Page::first(500), None).await.unwrap()
                } else {
                    conn.execute(&select, Page::first(500)).await.unwrap()
                };
                assert_eq!(r.rows.len(), 500);
                let elapsed = started.elapsed();
                if elapsed < best.0 {
                    best = (elapsed, received.load(std::sync::atomic::Ordering::SeqCst) - before);
                }
            }
            runs.push(best);
        }
        eprintln!(
            "MySQL first console page of {rows} rows: full read {:?} ({} B received), ended on the server {:?} ({} B received)",
            runs[0].0, runs[0].1, runs[1].0, runs[1].1
        );
        conn.execute(&format!("drop table {table}"), Page::first(0)).await.unwrap();
    }
}

/// Table page cost by depth: LIMIT/OFFSET as generated today against a range
/// read after the previous page's last key, on 1,000,000 rows, and the
/// primary key lookup before each page over a link with 2 ms one-way delay.
#[tokio::test]
#[ignore = "manual measurement: cargo test --release --test drivers table_page_depth_cost -- --ignored --nocapture"]
async fn table_page_depth_cost() {
    if std::net::TcpStream::connect("127.0.0.1:55432").is_err() || std::net::TcpStream::connect("127.0.0.1:53306").is_err() {
        eprintln!("skipping, PostgreSQL or MySQL unavailable");
        return;
    }
    for ds in [pg(), mysql()] {
        let mut conn = Connection::open(&ds).await.unwrap();
        let table = format!("dbeast_depth_{}", uuid::Uuid::new_v4().simple());
        let fill = match ds.driver {
            DriverKind::Postgres => format!("insert into {table} select g, md5(g::text) from generate_series(1, 1000000) g"),
            _ => format!("insert into {table} with recursive d(n) as (select 0 union all select n + 1 from d where n < 999) select a.n * 1000 + b.n + 1, md5(a.n * 1000 + b.n) from d a cross join d b"),
        };
        conn.execute(&format!("create table {table} (id int primary key, note varchar(40))"), Page::first(0)).await.unwrap();
        if ds.driver == DriverKind::MySql {
            conn.execute("set session cte_max_recursion_depth = 2000", Page::first(0)).await.unwrap();
        }
        conn.execute(&fill, Page::first(0)).await.unwrap();
        let base = format!("SELECT * FROM {table}");
        let keys = vec!["id".to_string()];
        for skip in [0usize, 100_000, 990_000] {
            let page = Page { skip, limit: 500 };
            let (sql, window) = dbeast::drivers::page_query(ds.driver, &base, &keys, page);
            let keyset = format!("{base} WHERE id > {skip} ORDER BY id LIMIT 501");
            let (mut offset, mut range) = (std::time::Duration::MAX, std::time::Duration::MAX);
            for _ in 0..5 {
                let started = std::time::Instant::now();
                let r = conn.execute_limited_with_context(&sql, window, None).await.unwrap();
                offset = offset.min(started.elapsed());
                assert_eq!(r.rows.len(), 500);
                let started = std::time::Instant::now();
                let r = conn.execute_limited_with_context(&keyset, Page::first(500), None).await.unwrap();
                range = range.min(started.elapsed());
                assert_eq!(r.rows.len(), 500);
            }
            eprintln!("{:?} page at row {skip} of 1,000,000: LIMIT/OFFSET {offset:?}, after the last key {range:?}", ds.driver);
        }
        conn.execute(&format!("drop table {table}"), Page::first(0)).await.unwrap();
    }
    let mut ds = pg();
    ds.host = "127.0.0.1".into();
    ds.port = delaying_proxy(std::time::Duration::from_millis(2)).await;
    let mut conn = Connection::open(&ds).await.unwrap();
    let mut key = std::time::Duration::MAX;
    for _ in 0..5 {
        let started = std::time::Instant::now();
        conn.primary_key(Some("shop"), "customers").await.unwrap();
        key = key.min(started.elapsed());
    }
    eprintln!("PostgreSQL primary key lookup with 2 ms one-way delay: {key:?}");
}
