//! Database Graph from PostgreSQL catalogs: statistics and file sizes only,
//! no table scans, no ANALYZE and no remote queries through foreign servers.

use super::PgConn;
use crate::schema_graph::{
    EdgeKind, GraphEdge, GraphNode, MAX_OBJECTS, Metric, MetricStatus, NodeKind, SchemaGraph,
};
use anyhow::Result;
use std::collections::HashMap;

/// Catalog rows only; sizes come from [`SIZES_SQL`] in batches. The statistics
/// functions read one counter each instead of the `pg_stat_all_tables` view.
const OBJECTS_SQL: &str = "select c.oid, n.nspname, c.relname, c.relkind::text, c.relispartition,
       c.reltuples::float8, c.relpages::int8,
       c.relkind <> 'm' or c.relispopulated,
       fs.srvname, w.fdwname,
       greatest(pg_catalog.pg_stat_get_last_analyze_time(c.oid),
                pg_catalog.pg_stat_get_last_autoanalyze_time(c.oid))::text
  from pg_catalog.pg_class c
  join pg_catalog.pg_namespace n on n.oid = c.relnamespace
  left join pg_catalog.pg_foreign_table ft on ft.ftrelid = c.oid
  left join pg_catalog.pg_foreign_server fs on fs.oid = ft.ftserver
  left join pg_catalog.pg_foreign_data_wrapper w on w.oid = fs.srvfdw
 where c.relkind in ('r','p','v','m','f')
   and n.nspname <> 'information_schema' and n.nspname !~ '^pg_'
   and ($1::text[] is null or n.nspname = any($1))
 order by n.nspname, c.relname
 limit $2";

/// One row per foreign key column pair, in key order. Constraints cloned onto
/// partitions are skipped: the parent constraint stands for them.
/// Size functions open each relation (and its indexes and TOAST) with a lock
/// held until the statement ends, so they run in small batches with a lock
/// timeout instead of over thousands of relations at once.
const SIZES_SQL: &str = "select r.oid, pg_catalog.pg_table_size(r.oid), pg_catalog.pg_total_relation_size(r.oid)
  from unnest($1::oid[]) r(oid)";
const SIZE_BATCH: usize = 500;

const FOREIGN_KEYS_SQL: &str = "select con.oid, con.conname, con.conrelid, con.confrelid, con.convalidated,
       k.ord, sa.attname, da.attname
  from unnest($1::oid[]) src(oid)
  join pg_catalog.pg_constraint con on con.conrelid = src.oid
  cross join lateral pg_catalog.unnest(con.conkey) with ordinality k(attnum, ord)
  join lateral pg_catalog.unnest(con.confkey) with ordinality f(attnum, ord) on f.ord = k.ord
  join pg_catalog.pg_attribute sa on sa.attrelid = con.conrelid and sa.attnum = k.attnum
  join pg_catalog.pg_attribute da on da.attrelid = con.confrelid and da.attnum = f.attnum
 where con.contype = 'f' and con.conparentid = 0
   and con.confrelid in (select unnest($1::oid[]))
 order by con.oid, k.ord";

/// Views and materialized views depend on relations through their rewrite rule.
const VIEW_DEPENDENCIES_SQL: &str = "select distinct r.ev_class, d.refobjid
  from unnest($1::oid[]) v(oid)
  join pg_catalog.pg_rewrite r on r.ev_class = v.oid
  join pg_catalog.pg_depend d on d.classid = 'pg_catalog.pg_rewrite'::regclass and d.objid = r.oid
 where d.refclassid = 'pg_catalog.pg_class'::regclass and d.deptype = 'n'
   and d.refobjid <> r.ev_class and d.refobjid in (select unnest($1::oid[]))";

const PARTITIONS_SQL: &str = "select i.inhrelid, i.inhparent
  from unnest($1::oid[]) c(oid)
  join pg_catalog.pg_inherits i on i.inhrelid = c.oid
  join pg_catalog.pg_class p on p.oid = i.inhparent
 where p.relkind = 'p'";

impl PgConn {
    pub async fn schema_graph(&self, database: &str, schemas: Option<&[String]>) -> Result<SchemaGraph> {
        let schemas: Option<Vec<String>> = schemas.map(<[String]>::to_vec);
        let limit = (MAX_OBJECTS + 1) as i64;
        let rows = self.client.query(OBJECTS_SQL, &[&schemas, &limit]).await?;
        let mut graph = SchemaGraph {
            database: database.into(),
            read_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            ..Default::default()
        };
        if rows.len() > MAX_OBJECTS {
            graph.truncated = true;
            graph.notes.push(format!(
                "Only the first {MAX_OBJECTS} objects are shown. Choose fewer schemas in the data source to see the rest."
            ));
        }
        let mut index_of: HashMap<u32, usize> = HashMap::new();
        let mut servers: HashMap<String, usize> = HashMap::new();
        let mut foreign: Vec<(usize, String)> = Vec::new();
        let mut sized: Vec<u32> = Vec::new();
        for row in rows.iter().take(MAX_OBJECTS) {
            let oid: u32 = row.get(0);
            let schema: String = row.get(1);
            let name: String = row.get(2);
            let relkind: String = row.get(3);
            let is_partition: bool = row.get(4);
            let reltuples: f64 = row.get(5);
            let relpages: i64 = row.get(6);
            let populated: bool = row.get(7);
            let server: Option<String> = row.get(8);
            let wrapper: Option<String> = row.get(9);
            let analyzed: Option<String> = row.get(10);
            let kind = match relkind.as_str() {
                "p" => NodeKind::PartitionedTable,
                "v" => NodeKind::View,
                "m" => NodeKind::MaterializedView,
                "f" => NodeKind::ForeignTable,
                _ if is_partition => NodeKind::Partition,
                _ => NodeKind::Table,
            };
            // Before PostgreSQL 14 an unanalyzed table reports 0 with 0 pages.
            let rows_metric = match kind {
                NodeKind::View => Metric::missing(MetricStatus::NotApplicable, "", "A view stores no rows of its own."),
                NodeKind::PartitionedTable => Metric::missing(MetricStatus::NotApplicable, "", "Partitions store the rows."),
                NodeKind::MaterializedView if !populated => {
                    Metric::missing(MetricStatus::Unknown, "pg_class.reltuples", "The materialized view is not populated.")
                }
                _ if reltuples < 0.0 || (reltuples == 0.0 && relpages == 0 && analyzed.is_none()) => Metric::missing(
                    MetricStatus::Unknown,
                    "pg_class.reltuples",
                    "No statistics yet: the table was never analyzed.",
                ),
                NodeKind::ForeignTable => {
                    let mut metric = Metric::known(reltuples, MetricStatus::Estimated, "pg_class.reltuples");
                    metric.note = Some("Local statistic of the foreign table; the remote table was not asked.".into());
                    metric
                }
                _ => Metric::known(reltuples, MetricStatus::Estimated, "pg_class.reltuples"),
            };
            let size = |source: &str| match kind {
                NodeKind::View => Metric::missing(MetricStatus::NotApplicable, "", "A view stores no data of its own."),
                NodeKind::PartitionedTable => Metric::missing(MetricStatus::NotApplicable, "", "Partitions store the data."),
                NodeKind::ForeignTable => Metric::missing(
                    MetricStatus::Unavailable,
                    "",
                    "The data is on the foreign server; its size is not read.",
                ),
                // Filled by the size batches below.
                _ => Metric::missing(MetricStatus::Unavailable, source, "The size could not be read."),
            };
            let mut facts = Vec::new();
            if let Some(analyzed) = &analyzed {
                facts.push(("Statistics updated".into(), analyzed.chars().take(19).collect()));
            } else if matches!(kind, NodeKind::Table | NodeKind::Partition | NodeKind::MaterializedView) {
                facts.push(("Statistics updated".into(), "never".into()));
            }
            if let Some(wrapper) = &wrapper {
                facts.push(("Foreign data wrapper".into(), wrapper.clone()));
            }
            let index = graph.nodes.len();
            index_of.insert(oid, index);
            graph.nodes.push(GraphNode {
                path: vec![database.to_owned(), schema.clone(), name.clone()],
                schema,
                name,
                kind,
                rows: rows_metric,
                table_bytes: size("pg_table_size"),
                total_bytes: size("pg_total_relation_size"),
                parent: None,
                facts,
            });
            if let Some(server) = server {
                foreign.push((index, server));
            }
            if matches!(kind, NodeKind::Table | NodeKind::Partition | NodeKind::MaterializedView) {
                sized.push(oid);
            }
        }
        self.read_sizes(&mut graph, &index_of, &sized).await;
        for (table, server) in foreign {
            let server_index = *servers.entry(server.clone()).or_insert_with(|| {
                let not_stored = || Metric::missing(MetricStatus::NotApplicable, "", "A foreign server stores no data here.");
                graph.nodes.push(GraphNode {
                    schema: "Foreign servers".into(),
                    name: server.clone(),
                    kind: NodeKind::ForeignServer,
                    path: Vec::new(),
                    rows: not_stored(),
                    table_bytes: not_stored(),
                    total_bytes: not_stored(),
                    parent: None,
                    facts: Vec::new(),
                });
                graph.nodes.len() - 1
            });
            graph.edges.push(GraphEdge {
                from: table,
                to: server_index,
                kind: EdgeKind::ForeignSource,
                name: None,
                columns: Vec::new(),
                validated: None,
            });
        }
        let oids: Vec<u32> = index_of.keys().copied().collect();
        for row in self.client.query(PARTITIONS_SQL, &[&oids]).await? {
            let (Some(&child), Some(&parent)) = (index_of.get(&row.get::<_, u32>(0)), index_of.get(&row.get::<_, u32>(1))) else {
                continue;
            };
            graph.nodes[child].parent = Some(parent);
            graph.edges.push(GraphEdge {
                from: child,
                to: parent,
                kind: EdgeKind::Partition,
                name: None,
                columns: Vec::new(),
                validated: None,
            });
        }
        let mut last: Option<u32> = None;
        for row in self.client.query(FOREIGN_KEYS_SQL, &[&oids]).await? {
            let constraint: u32 = row.get(0);
            let pair = (row.get::<_, String>(6), row.get::<_, String>(7));
            if last == Some(constraint)
                && let Some(edge) = graph.edges.last_mut()
            {
                edge.columns.push(pair);
                continue;
            }
            let (Some(&from), Some(&to)) = (index_of.get(&row.get::<_, u32>(2)), index_of.get(&row.get::<_, u32>(3))) else {
                continue;
            };
            last = Some(constraint);
            graph.edges.push(GraphEdge {
                from,
                to,
                kind: EdgeKind::ForeignKey,
                name: Some(row.get(1)),
                columns: vec![pair],
                validated: Some(row.get(4)),
            });
        }
        for row in self.client.query(VIEW_DEPENDENCIES_SQL, &[&oids]).await? {
            let (Some(&view), Some(&source)) = (index_of.get(&row.get::<_, u32>(0)), index_of.get(&row.get::<_, u32>(1))) else {
                continue;
            };
            let kind = if graph.nodes[view].kind == NodeKind::MaterializedView {
                EdgeKind::MaterializedViewDependency
            } else {
                EdgeKind::ViewDependency
            };
            graph.edges.push(GraphEdge {
                from: view,
                to: source,
                kind,
                name: None,
                columns: Vec::new(),
                validated: None,
            });
        }
        graph.aggregate_partitions();
        Ok(graph)
    }

    /// Measured sizes in batches; a locked relation fails only its batch,
    /// which keeps the "unavailable" status and a note instead of a wait.
    async fn read_sizes(&self, graph: &mut SchemaGraph, index_of: &HashMap<u32, usize>, oids: &[u32]) {
        if oids.is_empty() || self.client.batch_execute("set lock_timeout = '2s'").await.is_err() {
            return;
        }
        let mut failed = 0;
        for batch in oids.chunks(SIZE_BATCH) {
            match self.client.query(SIZES_SQL, &[&batch]).await {
                Ok(rows) => {
                    for row in rows {
                        let Some(&index) = index_of.get(&row.get::<_, u32>(0)) else {
                            continue;
                        };
                        let node = &mut graph.nodes[index];
                        if let Some(bytes) = row.get::<_, Option<i64>>(1) {
                            node.table_bytes = Metric::known(bytes as f64, MetricStatus::Measured, "pg_table_size");
                        }
                        if let Some(bytes) = row.get::<_, Option<i64>>(2) {
                            node.total_bytes = Metric::known(bytes as f64, MetricStatus::Measured, "pg_total_relation_size");
                        }
                    }
                }
                Err(_) => failed += batch.len(),
            }
        }
        let _ = self.client.batch_execute("reset lock_timeout").await;
        if failed > 0 {
            graph.notes.push(format!(
                "Sizes of {failed} objects were not read: a lock was held longer than 2 s."
            ));
        }
    }
}
