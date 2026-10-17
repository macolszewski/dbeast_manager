//! Database Graph from the SQLite schema: declared foreign keys, ANALYZE
//! statistics when present and page sizes from dbstat when compiled in.

use super::LiteConn;
use crate::schema_graph::{
    EdgeKind, GraphEdge, GraphNode, MAX_OBJECTS, Metric, MetricStatus, NodeKind, SchemaGraph,
};
use anyhow::Result;
use rusqlite::Connection;
use std::collections::HashMap;

/// Largest file whose sizes are measured page by page.
const MAX_MEASURED_FILE: i64 = 256 * 1024 * 1024;

impl LiteConn {
    pub async fn schema_graph(&self) -> Result<SchemaGraph> {
        self.with(read_graph).await
    }
}

fn read_graph(conn: &Connection) -> Result<SchemaGraph> {
    let mut graph = SchemaGraph {
        database: "main".into(),
        read_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        ..Default::default()
    };
    let mut statement = conn.prepare(
        "select name, type from main.sqlite_schema
          where type in ('table', 'view') and name not like 'sqlite\\_%' escape '\\'
          order by name limit ?1",
    )?;
    let objects: Vec<(String, String)> = statement
        .query_map([(MAX_OBJECTS + 1) as i64], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?;
    if objects.len() > MAX_OBJECTS {
        graph.truncated = true;
        graph.notes.push(format!("Only the first {MAX_OBJECTS} objects are shown."));
    }
    // ANALYZE writes the row count as the first number of each index entry.
    let mut estimates: HashMap<String, f64> = HashMap::new();
    let has_stat: bool = conn.query_row(
        "select exists(select 1 from main.sqlite_schema where name = 'sqlite_stat1')",
        [],
        |row| row.get(0),
    )?;
    if has_stat {
        let mut statement = conn.prepare("select tbl, stat from main.sqlite_stat1")?;
        for row in statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)))? {
            let (table, stat) = row?;
            if let Some(count) = stat.and_then(|s| s.split(' ').next().and_then(|n| n.parse::<f64>().ok())) {
                estimates.insert(table, count);
            }
        }
    }
    // dbstat is optional at compile time; without it sizes are unavailable, not zero.
    // It visits every page, so large files are not measured.
    let file_bytes: i64 = conn.query_row(
        "select page_count * page_size from pragma_page_count('main'), pragma_page_size('main')",
        [],
        |row| row.get(0),
    )?;
    let mut own_bytes: HashMap<String, f64> = HashMap::new();
    let dbstat = if file_bytes > MAX_MEASURED_FILE {
        Err(rusqlite::Error::InvalidQuery)
    } else {
        conn.prepare("select name, sum(pgsize) from dbstat('main') group by name")
        .and_then(|mut statement| {
            statement
                .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?)))?
                .collect::<Result<Vec<_>, _>>()
        })
    };
    let sizes_available = match dbstat {
        Ok(rows) => {
            own_bytes.extend(rows);
            true
        }
        Err(_) if file_bytes > MAX_MEASURED_FILE => {
            graph.notes.push(format!(
                "Sizes are not read: measuring reads the whole {} MB file.",
                file_bytes / (1024 * 1024)
            ));
            false
        }
        Err(_) => {
            graph.notes.push("Sizes are unavailable: this SQLite build has no dbstat table.".into());
            false
        }
    };
    let mut index_bytes: HashMap<String, f64> = HashMap::new();
    if sizes_available {
        let mut statement = conn.prepare("select name, tbl_name from main.sqlite_schema where type = 'index'")?;
        for row in statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))? {
            let (index, table) = row?;
            *index_bytes.entry(table).or_default() += own_bytes.get(&index).copied().unwrap_or(0.0);
        }
    }
    graph.notes.push("SQLite does not record view dependencies; views have no edges.".into());
    let mut index_of: HashMap<String, usize> = HashMap::new();
    for (name, kind) in objects.into_iter().take(MAX_OBJECTS) {
        let view = kind == "view";
        let none = || Metric::missing(MetricStatus::NotApplicable, "", "A view stores no data of its own.");
        let (rows, table_bytes, total_bytes) = if view {
            (none(), none(), none())
        } else {
            let rows = match estimates.get(&name) {
                Some(count) => Metric::known(*count, MetricStatus::Estimated, "sqlite_stat1"),
                None => Metric::missing(MetricStatus::Unknown, "sqlite_stat1", "No ANALYZE statistics for this table."),
            };
            let size = |extra: f64, source: &str| {
                if sizes_available {
                    Metric::known(own_bytes.get(&name).copied().unwrap_or(0.0) + extra, MetricStatus::Measured, source)
                } else {
                    Metric::missing(MetricStatus::Unavailable, source, "Sizes were not read; see the status line.")
                }
            };
            let indexes = index_bytes.get(&name).copied().unwrap_or(0.0);
            (rows, size(0.0, "dbstat"), size(indexes, "dbstat, with indexes"))
        };
        index_of.insert(name.clone(), graph.nodes.len());
        graph.nodes.push(GraphNode {
            schema: "main".into(),
            path: vec!["main".into(), name.clone()],
            name,
            kind: if view { NodeKind::View } else { NodeKind::Table },
            rows,
            table_bytes,
            total_bytes,
            parent: None,
            facts: Vec::new(),
        });
    }
    let tables: Vec<(String, usize)> = index_of
        .iter()
        .filter(|(_, index)| graph.nodes[**index].kind == NodeKind::Table)
        .map(|(name, index)| (name.clone(), *index))
        .collect();
    let mut tables = tables;
    tables.sort();
    let mut statement = conn.prepare("select id, seq, \"table\", \"from\", \"to\" from pragma_foreign_key_list(?1, 'main') order by id, seq")?;
    for (table, from) in tables {
        let keys: Vec<(i64, String, String, Option<String>)> = statement
            .query_map([&table], |row| Ok((row.get(0)?, row.get(2)?, row.get(3)?, row.get(4)?)))?
            .collect::<Result<_, _>>()?;
        let mut last = None;
        for (id, referenced, column, referenced_column) in keys {
            // A missing target column means the referenced primary key.
            let pair = (column, referenced_column.unwrap_or_else(|| "(primary key)".into()));
            if last == Some(id)
                && let Some(edge) = graph.edges.last_mut()
            {
                edge.columns.push(pair);
                continue;
            }
            let Some(&to) = index_of.get(&referenced) else {
                continue;
            };
            last = Some(id);
            graph.edges.push(GraphEdge {
                from,
                to,
                kind: EdgeKind::ForeignKey,
                name: None,
                columns: vec![pair],
                validated: None,
            });
        }
    }
    Ok(graph)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_tables_views_composite_keys_and_statistics_without_scanning() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "create table customers(id integer primary key, region text, code text, unique(region, code));
             create table orders(id integer primary key, customer_id integer references customers(id),
                                 region text, code text,
                                 foreign key(region, code) references customers(region, code));
             create table notes(body text);
             create view big_orders as select * from orders;
             insert into customers values (1, 'eu', 'a'), (2, 'us', 'b');
             analyze;",
        )
        .unwrap();
        let graph = read_graph(&conn).unwrap();
        let names: Vec<&str> = graph.nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["big_orders", "customers", "notes", "orders"]);
        let customers = &graph.nodes[1];
        assert_eq!(customers.rows.value, Some(2.0));
        assert_eq!(customers.rows.status, MetricStatus::Estimated);
        assert_eq!(graph.nodes[2].rows.status, MetricStatus::Unknown, "no index, no statistics");
        assert_eq!(graph.nodes[0].rows.status, MetricStatus::NotApplicable);
        let keys: Vec<_> = graph.edges.iter().filter(|e| e.kind == EdgeKind::ForeignKey).collect();
        assert_eq!(keys.len(), 2);
        let composite = keys.iter().find(|e| e.columns.len() == 2).expect("composite key kept whole");
        assert_eq!(composite.columns[0], ("region".to_string(), "region".to_string()));
        assert_eq!(composite.columns[1], ("code".to_string(), "code".to_string()));
        assert_eq!((composite.from, composite.to), (3, 1));
    }
}
