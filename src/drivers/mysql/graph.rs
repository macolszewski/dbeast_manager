//! Database Graph from MySQL INFORMATION_SCHEMA: engine estimates only, one
//! narrow lookup per view, no table scans and no ANALYZE TABLE.

use super::MyConn;
use crate::schema_graph::{
    EdgeKind, GraphEdge, GraphNode, MAX_OBJECTS, Metric, MetricStatus, NodeKind, SchemaGraph,
};
use anyhow::Result;
use mysql_async::prelude::*;
use std::collections::HashMap;

impl MyConn {
    pub async fn schema_graph(&mut self, schema: &str) -> Result<SchemaGraph> {
        let mut graph = SchemaGraph {
            database: schema.into(),
            read_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            ..Default::default()
        };
        type TableRow = (String, String, Option<u64>, Option<u64>, Option<u64>, Option<String>, Option<String>, Option<String>);
        let tables: Vec<TableRow> = self
            .conn
            .exec(
                // Statistics may open every table on older servers; the hint
                // bounds the statement on MySQL and is a comment elsewhere.
                "select /*+ MAX_EXECUTION_TIME(15000) */ table_name, table_type, table_rows, data_length, index_length, engine,
                        create_options, cast(update_time as char)
                   from information_schema.tables where table_schema = ?
                  order by table_name limit ?",
                (schema, (MAX_OBJECTS + 1) as u64),
            )
            .await?;
        if tables.len() > MAX_OBJECTS {
            graph.truncated = true;
            graph.notes.push(format!("Only the first {MAX_OBJECTS} objects are shown."));
        }
        graph.notes.push(
            "MySQL row counts and sizes are engine estimates and may be cached (information_schema_stats_expiry).".into(),
        );
        let mut index_of: HashMap<String, usize> = HashMap::new();
        for (name, table_type, rows, data, indexes, engine, options, updated) in tables.into_iter().take(MAX_OBJECTS) {
            let view = table_type.eq_ignore_ascii_case("VIEW");
            let (rows, table_bytes, total_bytes) = if view {
                let none = || Metric::missing(MetricStatus::NotApplicable, "", "A view stores no data of its own.");
                (none(), none(), none())
            } else {
                let estimate = |value: Option<u64>, source: &str| match value {
                    Some(value) => Metric::known(value as f64, MetricStatus::Estimated, source),
                    None => Metric::missing(MetricStatus::Unknown, source, "The engine reports no value."),
                };
                (
                    estimate(rows, "information_schema.tables.table_rows"),
                    estimate(data, "information_schema.tables.data_length"),
                    estimate(data.zip(indexes).map(|(d, i)| d + i), "data_length + index_length"),
                )
            };
            let mut facts = Vec::new();
            if let Some(engine) = engine {
                facts.push(("Engine".into(), engine));
            }
            if options.as_deref().is_some_and(|o| o.contains("partitioned")) {
                facts.push(("Partitioned".into(), "yes; partitions are not shown separately".into()));
            }
            if let Some(updated) = updated {
                facts.push(("Last update".into(), updated));
            }
            index_of.insert(name.clone(), graph.nodes.len());
            graph.nodes.push(GraphNode {
                schema: schema.into(),
                path: vec![schema.into(), name.clone()],
                name,
                kind: if view { NodeKind::View } else { NodeKind::Table },
                rows,
                table_bytes,
                total_bytes,
                parent: None,
                facts,
            });
        }
        // A single view, filtered by schema; joins of INFORMATION_SCHEMA views
        // can take MySQL a long time to optimize.
        type KeyRow = (String, String, String, String, String);
        let keys: Vec<KeyRow> = self
            .conn
            .exec(
                "select constraint_name, table_name, column_name, referenced_table_name, referenced_column_name
                   from information_schema.key_column_usage
                  where table_schema = ? and referenced_table_schema = ? and referenced_table_name is not null
                  order by table_name, constraint_name, ordinal_position",
                (schema, schema),
            )
            .await?;
        let mut last: Option<(String, String)> = None;
        for (constraint, table, column, referenced, referenced_column) in keys {
            let id = (table.clone(), constraint.clone());
            if last.as_ref() == Some(&id)
                && let Some(edge) = graph.edges.last_mut()
            {
                edge.columns.push((column, referenced_column));
                continue;
            }
            let (Some(&from), Some(&to)) = (index_of.get(&table), index_of.get(&referenced)) else {
                continue;
            };
            last = Some(id);
            graph.edges.push(GraphEdge {
                from,
                to,
                kind: EdgeKind::ForeignKey,
                name: Some(constraint),
                columns: vec![(column, referenced_column)],
                validated: None,
            });
        }
        // VIEW_TABLE_USAGE exists from MySQL 8.0.13; older servers and MariaDB lack it.
        let usage: Result<Vec<(String, String)>, _> = self
            .conn
            .exec(
                "select view_name, table_name from information_schema.view_table_usage
                  where view_schema = ? and table_schema = ?",
                (schema, schema),
            )
            .await;
        match usage {
            Ok(usage) => {
                for (view, table) in usage {
                    if let (Some(&from), Some(&to)) = (index_of.get(&view), index_of.get(&table))
                        && from != to
                    {
                        graph.edges.push(GraphEdge {
                            from,
                            to,
                            kind: EdgeKind::ViewDependency,
                            name: None,
                            columns: Vec::new(),
                            validated: None,
                        });
                    }
                }
            }
            Err(_) => graph
                .notes
                .push("View dependencies are not available on this server (VIEW_TABLE_USAGE needs MySQL 8.0.13).".into()),
        }
        Ok(graph)
    }
}
