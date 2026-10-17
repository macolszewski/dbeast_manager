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
