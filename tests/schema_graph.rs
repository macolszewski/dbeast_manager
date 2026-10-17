//! Database Graph from real catalogs. Skipped when the test containers are not running.

use dbeast::drivers::Connection;
use dbeast::model::{DataSource, DriverKind, Page};
use dbeast::schema_graph::{EdgeKind, MetricStatus, NodeKind, SchemaGraph};

fn server(driver: DriverKind) -> Option<DataSource> {
    let port = if driver == DriverKind::Postgres { 55432 } else { 53306 };
    if let Err(error) = std::net::TcpStream::connect(("127.0.0.1", port)) {
        eprintln!("skipping {driver:?} Database Graph integration: TCP probe failed: {error}");
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

fn node<'a>(graph: &'a SchemaGraph, name: &str) -> (usize, &'a dbeast::schema_graph::GraphNode) {
    graph
        .nodes
        .iter()
        .enumerate()
        .find(|(_, node)| node.name == name)
        .unwrap_or_else(|| panic!("{name} missing: {:?}", graph.nodes.iter().map(|n| &n.name).collect::<Vec<_>>()))
}

async fn run(conn: &mut Connection, sql: &str) {
    conn.execute(sql, Page::first(10)).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
}

#[tokio::test]
async fn postgres_graph_reads_kinds_metrics_and_typed_edges_from_catalogs() {
    let Some(ds) = server(DriverKind::Postgres) else { return };
    let suffix = &uuid::Uuid::new_v4().simple().to_string()[..12];
    let schema = format!("graph_{suffix}");
    let foreign_server = format!("graph_srv_{suffix}");
    let mut conn = Connection::open(&ds).await.unwrap();
    for sql in [
        format!("create schema {schema}"),
        format!("create table {schema}.customers(id int primary key, region text, code text, unique(region, code))"),
        format!(
            "create table {schema}.orders(id int primary key, customer_id int references {schema}.customers(id),
             region text, code text)"
        ),
        format!("alter table {schema}.orders add foreign key(region, code) references {schema}.customers(region, code) not valid"),
        format!("insert into {schema}.customers select g, 'r' || g, 'c' || g from generate_series(1, 200) g"),
        format!("analyze {schema}.customers"),
        format!("create table {schema}.never_analyzed(id int)"),
        format!("create table {schema}.events(id int, at date) partition by range (at)"),
        format!("create table {schema}.events_2026_01 partition of {schema}.events for values from ('2026-01-01') to ('2026-02-01')"),
        format!("create table {schema}.events_2026_02 partition of {schema}.events for values from ('2026-02-01') to ('2026-03-01')"),
        format!("insert into {schema}.events select g, date '2026-01-01' + (g % 50) from generate_series(1, 100) g"),
        format!("analyze {schema}.events"),
        format!("create view {schema}.order_view as select o.id, c.region from {schema}.orders o join {schema}.customers c on c.id = o.customer_id"),
        format!("create materialized view {schema}.region_totals as select region, count(*) from {schema}.customers group by region"),
        "create extension if not exists postgres_fdw".into(),
        format!("create server {foreign_server} foreign data wrapper postgres_fdw options (host 'localhost', dbname 'nowhere')"),
        format!("create foreign table {schema}.remote_items(id int) server {foreign_server}"),
    ] {
        run(&mut conn, &sql).await;
    }
    let graph = conn.schema_graph("dbeast", Some(std::slice::from_ref(&schema))).await;
    run(&mut conn, &format!("drop schema {schema} cascade")).await;
    run(&mut conn, &format!("drop server {foreign_server} cascade")).await;
    let graph = graph.unwrap();

    let (customers_index, customers) = node(&graph, "customers");
    assert_eq!(customers.kind, NodeKind::Table);
    assert_eq!(customers.rows.value, Some(200.0));
    assert_eq!(customers.rows.status, MetricStatus::Estimated);
    assert_eq!(customers.table_bytes.status, MetricStatus::Measured);
    assert!(customers.total_bytes.value > customers.table_bytes.value, "indexes are only in the total");
    assert_eq!(customers.path, ["dbeast", schema.as_str(), "customers"]);
    let (_, never) = node(&graph, "never_analyzed");
    assert_eq!(never.rows.status, MetricStatus::Unknown, "never analyzed is not zero");

    let (events_index, events) = node(&graph, "events");
    assert_eq!(events.kind, NodeKind::PartitionedTable);
    assert_eq!(events.rows.value, Some(100.0), "parent sums its partitions");
    assert!(events.rows.note.as_deref().unwrap().contains("2 partitions"));
    let (january, partition) = node(&graph, "events_2026_01");
    assert_eq!(partition.kind, NodeKind::Partition);
    assert_eq!(partition.parent, Some(events_index));
    assert!(graph.edges.iter().any(|e| e.kind == EdgeKind::Partition && e.from == january && e.to == events_index));

    let (orders_index, _) = node(&graph, "orders");
    let keys: Vec<_> = graph
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::ForeignKey && e.from == orders_index && e.to == customers_index)
        .collect();
    assert_eq!(keys.len(), 2, "two constraints, not one per column");
    let composite = keys.iter().find(|e| e.columns.len() == 2).unwrap();
    assert_eq!(composite.columns, [("region".to_string(), "region".to_string()), ("code".to_string(), "code".to_string())]);
    assert_eq!(composite.validated, Some(false), "NOT VALID is reported");

    let (view_index, view) = node(&graph, "order_view");
    assert_eq!(view.kind, NodeKind::View);
    assert_eq!(view.total_bytes.status, MetricStatus::NotApplicable);
    for target in [orders_index, customers_index] {
        assert!(graph.edges.iter().any(|e| e.kind == EdgeKind::ViewDependency && e.from == view_index && e.to == target));
    }
    let (totals_index, totals) = node(&graph, "region_totals");
    assert_eq!(totals.kind, NodeKind::MaterializedView);
    assert_eq!(totals.table_bytes.status, MetricStatus::Measured);
    assert!(graph.edges.iter().any(|e| e.kind == EdgeKind::MaterializedViewDependency && e.from == totals_index && e.to == customers_index));

    let (remote_index, remote) = node(&graph, "remote_items");
    assert_eq!(remote.kind, NodeKind::ForeignTable);
    assert_eq!(remote.total_bytes.status, MetricStatus::Unavailable, "no remote size, not zero");
    assert!(remote.facts.iter().any(|(k, v)| k == "Foreign data wrapper" && v == "postgres_fdw"));
    let (server_index, server) = node(&graph, &foreign_server);
    assert_eq!(server.kind, NodeKind::ForeignServer);
    assert!(graph.edges.iter().any(|e| e.kind == EdgeKind::ForeignSource && e.from == remote_index && e.to == server_index));
    // Server options and user mappings stay out of the graph.
    let text = format!("{graph:?}");
    assert!(!text.contains("nowhere") && !text.contains("localhost"));
}

#[tokio::test]
async fn mysql_graph_reads_estimates_keys_and_view_usage() {
    let Some(ds) = server(DriverKind::MySql) else { return };
    let mut conn = Connection::open(&ds).await.unwrap();
    // The test user may not create databases; unique tables in its own database.
    let p = format!("g{}", &uuid::Uuid::new_v4().simple().to_string()[..10]);
    for sql in [
        format!("create table {p}_customers(id int primary key, region varchar(10), code varchar(10), unique(region, code))"),
        format!(
            "create table {p}_orders(id int primary key, customer_id int, region varchar(10), code varchar(10),
             foreign key(customer_id) references {p}_customers(id),
             foreign key(region, code) references {p}_customers(region, code))"
        ),
        format!("create view {p}_view as select o.id, c.region from {p}_orders o join {p}_customers c on c.id = o.customer_id"),
    ] {
        run(&mut conn, &sql).await;
    }
    let graph = conn.schema_graph("dbeast", None).await;
    for sql in [format!("drop view {p}_view"), format!("drop table {p}_orders"), format!("drop table {p}_customers")] {
        run(&mut conn, &sql).await;
    }
    let graph = graph.unwrap();
    let (customers, node_customers) = node(&graph, &format!("{p}_customers"));
    assert_eq!(node_customers.rows.status, MetricStatus::Estimated);
    assert_eq!(node_customers.path, ["dbeast".to_string(), format!("{p}_customers")]);
    let (orders, _) = node(&graph, &format!("{p}_orders"));
    let keys: Vec<_> = graph.edges.iter().filter(|e| e.kind == EdgeKind::ForeignKey && e.from == orders && e.to == customers).collect();
    assert_eq!(keys.len(), 2);
    assert!(keys.iter().any(|e| e.columns.len() == 2));
    let (view, node_view) = node(&graph, &format!("{p}_view"));
    assert_eq!(node_view.kind, NodeKind::View);
    assert!(graph.edges.iter().any(|e| e.kind == EdgeKind::ViewDependency && e.from == view && e.to == orders));
}

#[tokio::test]
async fn postgres_graph_does_not_wait_on_a_locked_table() {
    let Some(ds) = server(DriverKind::Postgres) else { return };
    let suffix = &uuid::Uuid::new_v4().simple().to_string()[..12];
    let schema = format!("graph_lock_{suffix}");
    let mut setup = Connection::open(&ds).await.unwrap();
    for sql in [
        format!("create schema {schema}"),
        format!("create table {schema}.busy(id int primary key)"),
        format!("create table {schema}.free(id int primary key)"),
    ] {
        run(&mut setup, &sql).await;
    }
    // Another session holds an exclusive lock, as a long migration would.
    let mut locker = Connection::open(&ds).await.unwrap();
    run(&mut locker, "begin").await;
    run(&mut locker, &format!("lock table {schema}.busy in access exclusive mode")).await;
    let started = std::time::Instant::now();
    let mut reader = Connection::open(&ds).await.unwrap();
    let graph = reader.schema_graph("dbeast", Some(std::slice::from_ref(&schema))).await;
    let elapsed = started.elapsed();
    run(&mut locker, "rollback").await;
    run(&mut setup, &format!("drop schema {schema} cascade")).await;
    let graph = graph.unwrap();
    assert!(elapsed < std::time::Duration::from_secs(10), "the scan waited {elapsed:?}");
    assert!(graph.notes.iter().any(|n| n.contains("lock")), "{:?}", graph.notes);
    let (_, busy) = node(&graph, "busy");
    assert_eq!(busy.total_bytes.status, MetricStatus::Unavailable, "a locked size is unknown, not zero");
    // The catalog part is still complete.
    node(&graph, "free");
}
