//! Native plans are normalized once on a worker; the UI only visits visible nodes.

pub mod diagram;

use crate::model::{DriverKind, QueryResult, ResolvedContext, Value};
use serde_json::Value as Json;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::time::Duration;

/// Plan only estimates. Analyse executes the statement and reports actual metrics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExplainMode {
    #[default]
    Plan,
    Analyse,
}

impl ExplainMode {
    pub fn label(self) -> &'static str {
        match self {
            ExplainMode::Plan => "Explain Plan",
            ExplainMode::Analyse => "Explain Analyse",
        }
    }
}

#[derive(Debug)]
pub struct QueryPlan {
    pub mode: ExplainMode,
    pub sql: String,
    pub raw: String,
    pub raw_lines: Vec<Range<usize>>,
    pub nodes: Vec<PlanNode>,
    /// Prepared in the same worker as parsing, without another database request.
    pub diagram: Result<Box<diagram::DiagramLayout>, diagram::DiagramError>,
    pub warning: Option<String>,
    pub elapsed: Duration,
    pub context: Option<ResolvedContext>,
    /// Only Analyse pays for actual metrics; estimated plans keep the node size.
    pub analysis: Option<Box<PlanAnalysis>>,
}

#[derive(Debug, Default)]
pub struct PlanAnalysis {
    /// One entry per `QueryPlan::nodes` item.
    pub nodes: Vec<ActualMetrics>,
    pub summary: PlanSummary,
}

/// Server values are kept as text: a missing value is not zero, and actual
/// times are per loop and include child work, so they are never summed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ActualMetrics {
    pub startup_ms: Option<String>,
    pub total_ms: Option<String>,
    pub rows: Option<String>,
    pub loops: Option<String>,
    pub never_executed: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PlanSummary {
    pub planning_time_ms: Option<String>,
    pub execution_time_ms: Option<String>,
}

const ACTUAL_KEYS: [&str; 4] = [
    "Actual Startup Time",
    "Actual Total Time",
    "Actual Rows",
    "Actual Loops",
];

#[derive(Debug, Default)]
pub struct PlanNode {
    pub depth: usize,
    pub subtree_end: usize,
    pub operation: String,
    pub object: String,
    pub startup_cost: String,
    pub total_cost: String,
    pub rows: String,
    pub details: Vec<(String, String)>,
}

/// Backend callers still use a prepared single statement: a client-side splitter
/// cannot know every session's SQL lexical mode.
pub fn explain_sql(driver: DriverKind, statement: &str) -> Result<String, String> {
    explain_sql_mode(driver, statement, ExplainMode::Plan)
}

pub fn explain_sql_mode(
    driver: DriverKind,
    statement: &str,
    mode: ExplainMode,
) -> Result<String, String> {
    if mode == ExplainMode::Analyse && driver == DriverKind::Sqlite {
        return Err("Explain Analyse is available for PostgreSQL and MySQL data sources.".into());
    }
    let statements = crate::splitter::split(statement, driver);
    if statements.len() != 1 {
        return Err("Select one statement to explain.".into());
    }
    let statement = &statements[0].text;
    let word = leading_word(statement, driver)?;
    let allowed = ["SELECT", "WITH", "INSERT", "UPDATE", "DELETE"]
        .iter()
        .any(|candidate| word.eq_ignore_ascii_case(candidate))
        || (driver != DriverKind::MySql && word.eq_ignore_ascii_case("VALUES"))
        || (driver == DriverKind::Postgres && word.eq_ignore_ascii_case("MERGE"));
    let allowed = allowed || (driver == DriverKind::MySql && word.eq_ignore_ascii_case("TABLE"));
    if driver == DriverKind::MySql
        && mode == ExplainMode::Analyse
        && word.eq_ignore_ascii_case("INSERT")
    {
        return Err("MySQL Explain Analyse supports SELECT, TABLE and server-supported UPDATE/DELETE forms, not INSERT.".into());
    }
    if !allowed {
        return Err(format!(
            "{} supports query and data modification statements. Select the original statement without EXPLAIN options.",
            mode.label()
        ));
    }
    let prefix = match driver {
        DriverKind::Postgres if mode == ExplainMode::Analyse => {
            "EXPLAIN (ANALYZE TRUE, FORMAT JSON)"
        }
        DriverKind::Postgres => "EXPLAIN (FORMAT JSON)",
        DriverKind::MySql if mode == ExplainMode::Analyse => "EXPLAIN ANALYZE FORMAT=TREE",
        DriverKind::MySql => "EXPLAIN FORMAT=JSON",
        DriverKind::Sqlite => "EXPLAIN QUERY PLAN",
    };
    Ok(format!("{prefix} {statement}"))
}

fn leading_word(sql: &str, driver: DriverKind) -> Result<&str, String> {
    let mut rest = sql.trim_start();
    loop {
        let mysql_line_comment = rest.starts_with("--")
            && rest
                .as_bytes()
                .get(2)
                .is_none_or(|b| b.is_ascii_whitespace() || b.is_ascii_control());
        if (rest.starts_with("--") && (driver != DriverKind::MySql || mysql_line_comment))
            || (driver == DriverKind::MySql && rest.starts_with('#'))
        {
            rest = rest.find('\n').map_or("", |i| &rest[i + 1..]).trim_start();
        } else if rest.starts_with("/*") {
            if rest.starts_with("/*!") {
                return Err("Explain Plan cannot classify an executable leading comment.".into());
            }
            let bytes = rest.as_bytes();
            let (mut i, mut depth) = (2, 1);
            while i < bytes.len() && depth > 0 {
                if bytes[i..].starts_with(b"/*") && driver == DriverKind::Postgres {
                    depth += 1;
                    i += 2;
                } else if bytes[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            if depth != 0 {
                return Err("Unterminated SQL comment.".into());
            }
            rest = rest[i..].trim_start();
        } else {
            let end = rest.bytes().take_while(u8::is_ascii_alphabetic).count();
            return Ok(&rest[..end]);
        }
    }
}

pub fn parse(driver: DriverKind, sql: String, data: QueryResult) -> QueryPlan {
    parse_mode(driver, sql, data, ExplainMode::Plan)
}

pub fn parse_mode(
    driver: DriverKind,
    sql: String,
    data: QueryResult,
    mode: ExplainMode,
) -> QueryPlan {
    let analyse = mode == ExplainMode::Analyse;
    let raw = if driver == DriverKind::Sqlite {
        let mut text = data
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>()
            .join("\t");
        for row in &data.rows {
            text.push('\n');
            for (index, value) in row.iter().enumerate() {
                if index > 0 {
                    text.push('\t');
                }
                use std::fmt::Write;
                write!(text, "{value}").unwrap();
            }
        }
        text
    } else {
        data.rows
            .iter()
            .filter_map(|row| row.first())
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    };
    let result = if driver == DriverKind::Sqlite {
        sqlite_nodes(&data).map(|nodes| (nodes, Vec::new(), PlanSummary::default()))
    } else if driver == DriverKind::MySql && analyse {
        mysql_tree_nodes(&raw)
    } else {
        serde_json::from_str::<Json>(&raw)
            .map_err(|error| {
                format!("The server returned a plan that cannot be parsed as JSON: {error}.")
            })
            .and_then(|json| match driver {
                DriverKind::Postgres => postgres_nodes(&json, analyse),
                DriverKind::MySql => {
                    mysql_nodes(&json).map(|nodes| (nodes, Vec::new(), PlanSummary::default()))
                }
                DriverKind::Sqlite => unreachable!(),
            })
    };
    // An unknown format keeps Raw and the Analyse mode. The statement already
    // ran, so normalization errors must never lead to another execution.
    let (mut nodes, metrics, summary, warning) = match result {
        Ok((nodes, metrics, summary)) if !nodes.is_empty() => (nodes, metrics, summary, None),
        Ok(_) => (
            Vec::new(),
            Vec::new(),
            PlanSummary::default(),
            Some("The server returned no operations. View the original plan in Raw.".into()),
        ),
        Err(reason) => (
            Vec::new(),
            Vec::new(),
            PlanSummary::default(),
            Some(format!("{reason} View the original plan in Raw.")),
        ),
    };
    let analysis = analyse.then(|| {
        debug_assert_eq!(metrics.len(), nodes.len());
        Box::new(PlanAnalysis {
            nodes: metrics,
            summary,
        })
    });
    // Close each ancestor once, independent of the depth of the tree.
    let mut ancestors: Vec<usize> = Vec::new();
    for index in 0..nodes.len() {
        while ancestors
            .last()
            .is_some_and(|&parent| nodes[parent].depth >= nodes[index].depth)
        {
            nodes[ancestors.pop().unwrap()].subtree_end = index;
        }
        ancestors.push(index);
    }
    for index in ancestors {
        nodes[index].subtree_end = nodes.len();
    }
    let mut offset = 0;
    let raw_lines = raw
        .split_inclusive('\n')
        .map(|line| {
            let start = offset;
            offset += line.len();
            start..offset - usize::from(line.ends_with('\n'))
        })
        .collect();
    let diagram = diagram::build(&nodes).map(Box::new);
    QueryPlan {
        mode,
        sql,
        raw,
        raw_lines,
        nodes,
        diagram,
        warning,
        elapsed: data.elapsed,
        context: data.context,
        analysis,
    }
}

fn text(value: Option<&Json>) -> String {
    match value {
        Some(Json::String(value)) => value.clone(),
        Some(Json::Null) | None => String::new(),
        Some(value) => value.to_string(),
    }
}

fn properties(value: &Json) -> Vec<(String, String)> {
    value
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, value)| {
            !value.is_object()
                && !value.as_array().is_some_and(|items| {
                    items.iter().any(|item| item.is_object() || item.is_array())
                })
        })
        .map(|(key, value)| (key.clone(), text(Some(value))))
        .collect()
}

fn optional(value: Option<&Json>) -> Option<String> {
    value
        .filter(|value| !value.is_null())
        .map(|value| text(Some(value)))
}

type ParsedPlan = (Vec<PlanNode>, Vec<ActualMetrics>, PlanSummary);

fn postgres_nodes(json: &Json, analyse: bool) -> Result<ParsedPlan, String> {
    let roots = json
        .as_array()
        .ok_or("Unrecognized PostgreSQL plan format.")?;
    let mut pending = Vec::new();
    for root in roots.iter().rev() {
        pending.push((root.get("Plan").ok_or("Missing PostgreSQL Plan field.")?, 0));
    }
    // Server totals are unambiguous only for a single source document.
    let summary = match roots.as_slice() {
        [root] if analyse => PlanSummary {
            planning_time_ms: optional(root.get("Planning Time")),
            execution_time_ms: optional(root.get("Execution Time")),
        },
        _ => PlanSummary::default(),
    };
    let mut nodes = Vec::new();
    let mut metrics = Vec::new();
    while let Some((value, depth)) = pending.pop() {
        let operation = value
            .get("Node Type")
            .and_then(Json::as_str)
            .ok_or("Missing PostgreSQL operation type.")?;
        let relation = text(value.get("Relation Name"));
        let schema = text(value.get("Schema"));
        let object = if schema.is_empty() || relation.is_empty() {
            relation
        } else {
            format!("{schema}.{relation}")
        };
        let mut details = properties(value);
        if analyse {
            let loops = optional(value.get("Actual Loops"));
            metrics.push(ActualMetrics {
                startup_ms: optional(value.get("Actual Startup Time")),
                total_ms: optional(value.get("Actual Total Time")),
                rows: optional(value.get("Actual Rows")),
                never_executed: value
                    .get("Actual Loops")
                    .and_then(Json::as_f64)
                    .is_some_and(|loops| loops == 0.0),
                loops,
            });
            // Promoted metrics are shown once; Raw keeps the original document.
            details.retain(|(key, _)| !ACTUAL_KEYS.contains(&key.as_str()));
        }
        nodes.push(PlanNode {
            depth,
            operation: operation.into(),
            object,
            startup_cost: text(value.get("Startup Cost")),
            total_cost: text(value.get("Total Cost")),
            rows: text(value.get("Plan Rows")),
            details,
            ..Default::default()
        });
        if let Some(children) = value.get("Plans") {
            for child in children
                .as_array()
                .ok_or("Invalid PostgreSQL child operations.")?
                .iter()
                .rev()
            {
                pending.push((child, depth + 1));
            }
        }
    }
    Ok((nodes, metrics, summary))
}

/// TREE is not a stable interchange format. Parse only recognizable native
/// lines; preserve the entire Raw plan if the server adds an unknown structure.
fn mysql_tree_nodes(raw: &str) -> Result<ParsedPlan, String> {
    let mut nodes = Vec::new();
    let mut metrics = Vec::new();
    let mut indents = Vec::new();
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        let trimmed = line.trim_start_matches(' ');
        let indent = line.len() - trimmed.len();
        let operation = trimmed
            .strip_prefix("-> ")
            .ok_or("Unrecognized MySQL TREE operation.")?;
        if nodes.is_empty() && indent != 0 {
            return Err("Unrecognized MySQL TREE root indentation.".into());
        }
        let dedenting = indents.last().is_some_and(|&previous| previous > indent);
        while indents.last().is_some_and(|&previous| previous > indent) {
            indents.pop();
        }
        if dedenting && indents.last() != Some(&indent) {
            return Err("Unrecognized MySQL TREE indentation.".into());
        }
        if indents.last() != Some(&indent) {
            indents.push(indent);
        }
        let depth = indents.len() - 1;
        let mut node = PlanNode {
            depth,
            ..Default::default()
        };
        let mut actual = ActualMetrics::default();
        let mut rest = operation.trim_end();
        if let Some(prefix) = rest.strip_suffix(" (never executed)") {
            actual.never_executed = true;
            rest = prefix.trim_end();
        } else if let Some((prefix, values)) = rest.rsplit_once(" (actual ") {
            let values = values
                .strip_suffix(')')
                .ok_or("Invalid MySQL actual metrics.")?;
            let mut seen = HashSet::new();
            for part in values.split_whitespace() {
                let (key, value) = part.split_once('=').ok_or("Invalid MySQL actual metric.")?;
                if !seen.insert(key) {
                    return Err("Duplicate MySQL actual metric.".into());
                }
                match key {
                    "time" => {
                        let (first, last) =
                            value.split_once("..").ok_or("Invalid MySQL actual time.")?;
                        actual.startup_ms = Some(tree_number(first)?);
                        actual.total_ms = Some(tree_number(last)?);
                    }
                    "rows" => actual.rows = Some(tree_number(value)?),
                    "loops" => {
                        actual.loops = Some(tree_number(value)?);
                        actual.never_executed = value.parse::<f64>().ok() == Some(0.0);
                    }
                    _ => node.details.push((format!("Actual {key}"), value.into())),
                }
            }
            rest = prefix.trim_end();
        }
        if let Some((prefix, values)) = rest.rsplit_once(" (cost=") {
            let values = values
                .strip_suffix(')')
                .ok_or("Invalid MySQL estimated metrics.")?;
            let mut parts = values.split_whitespace();
            let cost = parts.next().ok_or("Missing MySQL cost.")?;
            if let Some((first, last)) = cost.split_once("..") {
                node.startup_cost = tree_number(first)?;
                node.total_cost = tree_number(last)?;
            } else {
                node.total_cost = tree_number(cost)?;
            }
            for part in parts {
                let (key, value) = part
                    .split_once('=')
                    .ok_or("Invalid MySQL estimated metric.")?;
                if key == "rows" {
                    node.rows = tree_number(value)?;
                } else {
                    node.details.push((key.into(), value.into()));
                }
            }
            rest = prefix.trim_end();
        }
        if rest.is_empty() || rest.contains(" (actual ") || rest.contains(" (never executed)") {
            return Err("Unrecognized MySQL TREE metrics.".into());
        }
        node.operation = rest.into();
        nodes.push(node);
        metrics.push(actual);
    }
    // TREE has no separate server planning/execution summary. The root's time
    // includes its children and must not be promoted to a server total.
    Ok((nodes, metrics, PlanSummary::default()))
}

fn tree_number(value: &str) -> Result<String, String> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || b".+-eE".contains(&b))
        || !value
            .parse::<f64>()
            .is_ok_and(|number| number.is_finite() && number >= 0.0)
    {
        return Err("Unrecognized MySQL TREE number.".into());
    }
    Ok(value.into())
}

fn mysql_nodes(json: &Json) -> Result<Vec<PlanNode>, String> {
    let (kind, root) = ["query_block", "insert"]
        .iter()
        .find_map(|&kind| json.get(kind).map(|value| (kind, value)))
        .ok_or("Unrecognized MySQL plan format.")?;
    let mut pending = vec![(kind, root, 0)];
    let mut nodes = Vec::new();
    while let Some((kind, value, depth)) = pending.pop() {
        if let Some(entries) = value.as_array() {
            nodes.push(PlanNode {
                depth,
                operation: kind.replace('_', " "),
                ..Default::default()
            });
            let mut children = Vec::new();
            for entry in entries {
                if let Some(entry) = entry.as_object() {
                    for (kind, value) in entry {
                        if value.is_object() || value.is_array() {
                            children.push((kind.as_str(), value, depth + 1));
                        }
                    }
                }
            }
            pending.extend(children.into_iter().rev());
            continue;
        }
        let Some(object) = value.as_object() else {
            return Err("Invalid MySQL operation.".into());
        };
        let access = text(value.get("access_type"));
        let operation = if access.is_empty() {
            kind.replace('_', " ")
        } else {
            format!("{kind} ({access})")
        };
        let cost = value.get("cost_info");
        let total =
            cost.and_then(|cost| cost.get("query_cost").or_else(|| cost.get("prefix_cost")));
        let mut details = properties(value);
        if let Some(cost) = cost {
            details.extend(
                properties(cost)
                    .into_iter()
                    .map(|(key, value)| (format!("cost_info.{key}"), value)),
            );
        }
        nodes.push(PlanNode {
            depth,
            operation,
            object: text(value.get("table_name")),
            total_cost: text(total),
            rows: text(
                value
                    .get("rows_produced_per_join")
                    .or_else(|| value.get("rows_examined_per_scan")),
            ),
            details,
            ..Default::default()
        });
        let mut children = Vec::new();
        for (key, child) in object {
            if key == "cost_info" {
                continue;
            }
            if child.is_object()
                || child
                    .as_array()
                    .is_some_and(|array| array.iter().any(Json::is_object))
            {
                children.push((key.as_str(), child, depth + 1));
            }
        }
        pending.extend(children.into_iter().rev());
    }
    Ok(nodes)
}

fn sqlite_nodes(data: &QueryResult) -> Result<Vec<PlanNode>, String> {
    let mut indices = HashMap::with_capacity(data.rows.len());
    let mut parents = Vec::with_capacity(data.rows.len());
    for (index, row) in data.rows.iter().enumerate() {
        let [Value::Int(id), Value::Int(parent), _, Value::Text(_)] = row.as_slice() else {
            return Err("Unrecognized SQLite query plan columns.".into());
        };
        if indices.insert(*id, index).is_some() {
            return Err("Duplicate SQLite operation identifier.".into());
        }
        parents.push(*parent);
    }
    let mut children = vec![Vec::new(); parents.len()];
    let mut roots = Vec::new();
    for (index, parent) in parents.iter().enumerate() {
        if let Some(&parent_index) = indices
            .get(parent)
            .filter(|&&parent_index| parent_index != index)
        {
            children[parent_index].push(index);
        } else {
            roots.push(index);
        }
    }
    let mut visited = HashSet::with_capacity(parents.len());
    let mut pending: Vec<_> = roots.into_iter().rev().map(|index| (index, 0)).collect();
    let mut nodes = Vec::with_capacity(parents.len());
    while let Some((index, depth)) = pending.pop() {
        if !visited.insert(index) {
            return Err("Cyclic SQLite query plan.".into());
        }
        let row = &data.rows[index];
        nodes.push(PlanNode {
            depth,
            operation: row[3].to_string(),
            details: vec![
                ("id".into(), row[0].to_string()),
                ("parent".into(), row[1].to_string()),
                ("detail".into(), row[3].to_string()),
            ],
            ..Default::default()
        });
        pending.extend(
            children[index]
                .iter()
                .rev()
                .map(|&child| (child, depth + 1)),
        );
    }
    if visited.len() != parents.len() {
        return Err("Cyclic SQLite query plan.".into());
    }
    Ok(nodes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json_result(raw: &str) -> QueryResult {
        QueryResult {
            rows: vec![vec![Value::Text(raw.into())]],
            ..Default::default()
        }
    }

    #[test]
    fn generated_commands_never_inherit_explain_options_or_multiple_statements() {
        for driver in DriverKind::ALL {
            for sql in [
                "",
                "SELECT 1; DELETE FROM accounts",
                "EXPLAIN ANALYZE DELETE FROM accounts",
                "ANALYZE accounts",
                "BEGIN",
                "DROP TABLE accounts",
                "/*! SELECT 1 */",
            ] {
                assert!(explain_sql(driver, sql).is_err(), "{driver:?}: {sql}");
            }
            let sql = explain_sql(driver, "/* Żółw */ SELECT ';' AS value; -- trailing\n").unwrap();
            assert!(sql.contains("SELECT ';' AS value"));
            assert!(!sql.contains("ANALYZE"));
            assert!(explain_sql(driver, "WITH names AS (SELECT 1) SELECT * FROM names").is_ok());
            assert!(explain_sql(driver, "UPDATE accounts SET id = 2 WHERE id = 1").is_ok());
        }
        assert!(explain_sql(DriverKind::Postgres, "/* a /* nested */ b */ SELECT 1").is_ok());
        assert!(explain_sql(DriverKind::MySql, "# comment\nSELECT 1").is_ok());
    }

    #[test]
    fn postgres_tree_preserves_server_order_costs_and_subtree_bounds() {
        let raw = r#"[{"Plan":{"Node Type":"Nested Loop","Startup Cost":0.0,"Total Cost":12.345,"Plan Rows":9,"Plans":[{"Node Type":"Index Scan","Relation Name":"Żółw","Schema":"other","Index Cond":"(id = 1)","Total Cost":2.34},{"Node Type":"Seq Scan","Relation Name":"child","Plan Rows":3}]}}]"#;
        let plan = parse(
            DriverKind::Postgres,
            "SELECT original".into(),
            json_result(raw),
        );
        assert!(plan.warning.is_none());
        assert_eq!(plan.raw, raw);
        assert_eq!(
            plan.nodes
                .iter()
                .map(|node| (node.depth, node.subtree_end))
                .collect::<Vec<_>>(),
            [(0, 3), (1, 2), (1, 3)]
        );
        assert_eq!(plan.nodes[0].total_cost, "12.345");
        assert_eq!(plan.nodes[1].object, "other.Żółw");
        assert!(
            plan.nodes[1]
                .details
                .contains(&("Index Cond".into(), "(id = 1)".into()))
        );
        assert!(plan.nodes[1].rows.is_empty());
        assert_eq!(&plan.raw[plan.raw_lines[0].clone()], raw);
        let diagram = plan.diagram.as_ref().unwrap();
        assert_eq!(diagram.len(), plan.nodes.len());
        assert_eq!(diagram.first_child(0), Some(1));
        assert_eq!(diagram.parent(1), Some(0));
        assert_eq!(diagram.parent(2), Some(0));
    }

    #[test]
    fn analyse_generator_keeps_driver_and_single_statement_rules() {
        assert_eq!(
            explain_sql_mode(
                DriverKind::Postgres,
                "UPDATE t SET n = n + 1;",
                ExplainMode::Analyse
            )
            .unwrap(),
            "EXPLAIN (ANALYZE TRUE, FORMAT JSON) UPDATE t SET n = n + 1"
        );
        assert_eq!(
            explain_sql(DriverKind::Postgres, "SELECT 1").unwrap(),
            "EXPLAIN (FORMAT JSON) SELECT 1"
        );
        assert!(
            explain_sql_mode(
                DriverKind::Postgres,
                "SELECT 1; SELECT 2",
                ExplainMode::Analyse
            )
            .is_err()
        );
        for sql in ["EXPLAIN ANALYZE SELECT 1", "COMMIT"] {
            let error =
                explain_sql_mode(DriverKind::Postgres, sql, ExplainMode::Analyse).unwrap_err();
            assert!(error.contains("Explain Analyse"), "{sql}: {error}");
        }
        assert!(
            explain_sql_mode(DriverKind::Sqlite, "SELECT 1", ExplainMode::Analyse)
                .unwrap_err()
                .contains("PostgreSQL")
        );
    }

    #[test]
    fn mysql_analyse_generator_and_tree_metrics() {
        for sql in [
            "SELECT 1",
            "TABLE t",
            "WITH c AS (SELECT 1) SELECT * FROM c",
            "UPDATE t JOIN u USING(id) SET t.n=1",
            "DELETE t FROM t JOIN u USING(id)",
        ] {
            assert_eq!(
                explain_sql_mode(DriverKind::MySql, sql, ExplainMode::Analyse).unwrap(),
                format!("EXPLAIN ANALYZE FORMAT=TREE {sql}")
            );
        }
        for sql in [
            "INSERT INTO t VALUES(1)",
            "EXPLAIN SELECT 1",
            "SELECT 1; DELETE FROM t",
            "/*! SELECT 1 */",
        ] {
            assert!(explain_sql_mode(DriverKind::MySql, sql, ExplainMode::Analyse).is_err());
        }
        let raw = "-> Nested loop inner join  (cost=2.1 rows=9) (actual time=934e-6..1.23e+9 rows=3.5 loops=2)\n    -> Table scan on `żółć`  (cost=0.1..2 rows=4) (actual time=0..0 rows=0 loops=0)\n    -> Hash\n        -> Index lookup on t (id=1)  (cost=1 rows=1) (never executed)\n";
        let plan = parse_mode(
            DriverKind::MySql,
            "SELECT".into(),
            json_result(raw),
            ExplainMode::Analyse,
        );
        assert_eq!(plan.warning, None);
        assert_eq!(plan.raw, raw);
        assert_eq!(
            plan.nodes.iter().map(|n| n.depth).collect::<Vec<_>>(),
            vec![0, 1, 1, 2]
        );
        assert_eq!(plan.nodes[0].subtree_end, 4);
        assert_eq!(plan.nodes[1].startup_cost, "0.1");
        let analysis = plan.analysis.unwrap();
        assert_eq!(analysis.nodes[0].startup_ms.as_deref(), Some("934e-6"));
        assert_eq!(analysis.nodes[0].total_ms.as_deref(), Some("1.23e+9"));
        assert_eq!(analysis.nodes[0].rows.as_deref(), Some("3.5"));
        assert!(analysis.nodes[1].never_executed);
        assert_eq!(analysis.nodes[2], ActualMetrics::default());
        assert!(analysis.nodes[3].never_executed);
        assert!(analysis.nodes[3].rows.is_none());
        assert_eq!(analysis.summary, PlanSummary::default());
    }

    #[test]
    fn mysql_analyse_unknown_tree_keeps_raw_without_fabricating_metrics() {
        for raw in [
            "future format",
            "-> Scan (actual time=no..1 rows=1 loops=1)",
            "-> Scan\nunknown continuation",
            " -> Scan",
            "-> Scan (actual time=NaN..1 rows=1 loops=1)",
            "-> Scan (actual time=0..1 rows=1 rows=2)",
            "-> Scan (actual rows=)",
            "-> Root\n    -> Child\n  -> Bad dedent",
            "-> Filter: (x = ' (actual time=0..1 rows=1 loops=1)')",
        ] {
            let plan = parse_mode(
                DriverKind::MySql,
                "SELECT".into(),
                json_result(raw),
                ExplainMode::Analyse,
            );
            assert_eq!(plan.raw, raw);
            assert_eq!(plan.mode, ExplainMode::Analyse);
            assert!(plan.warning.is_some(), "{raw}");
            assert!(plan.nodes.is_empty());
            assert!(plan.analysis.unwrap().nodes.is_empty());
        }
        let plan = parse_mode(
            DriverKind::MySql,
            "SELECT".into(),
            json_result("-> Rows fetched before execution (actual rows=1 loops=1 future=kept)"),
            ExplainMode::Analyse,
        );
        assert!(plan.warning.is_none());
        assert_eq!(plan.analysis.unwrap().nodes[0].startup_ms, None);
        assert_eq!(
            plan.nodes[0].details,
            vec![("Actual future".into(), "kept".into())]
        );
    }

    #[test]
    #[ignore = "manual release measurement of MySQL TREE normalization, no UI or GPU"]
    fn benchmark_mysql_tree_normalization() {
        use std::time::Instant;
        for count in [101, 10_001] {
            let mut raw =
                String::from("-> Nested loop inner join (actual time=0.1..20 rows=10 loops=1)\n");
            for _ in 1..count {
                raw.push_str("    -> Table scan on t  (cost=1.23 rows=10) (actual time=0.01..0.02 rows=10 loops=3)\n");
            }
            let data: Vec<_> = (0..20).map(|_| json_result(&raw)).collect();
            let start = Instant::now();
            for data in data {
                let plan = parse_mode(DriverKind::MySql, String::new(), data, ExplainMode::Analyse);
                assert_eq!(plan.nodes.len(), count);
                std::hint::black_box(plan);
            }
            eprintln!(
                "MySQL TREE {count} nodes: {:.3} ms parse + diagram",
                start.elapsed().as_secs_f64() * 1000.0 / 20.0
            );
        }
    }

    #[test]
    fn actual_metrics_keep_missing_zero_loops_and_native_number_text() {
        let raw = r#"[{"Plan":{"Node Type":"Nested Loop","Plan Rows":9,"Actual Startup Time":0.000,"Actual Total Time":1.5e-3,"Actual Rows":0,"Actual Loops":1,"Future Field":"kept","Plans":[{"Node Type":"Index Scan","Relation Name":"a","Actual Startup Time":0.012,"Actual Total Time":0.020,"Actual Rows":3,"Actual Loops":250},{"Node Type":"Seq Scan","Relation Name":"b","Actual Startup Time":0.0,"Actual Total Time":0.0,"Actual Rows":0,"Actual Loops":0},{"Node Type":"Result"}]},"Planning Time":0.081,"Triggers":[],"Execution Time":12.004}]"#;
        let plan = parse_mode(
            DriverKind::Postgres,
            "SELECT".into(),
            json_result(raw),
            ExplainMode::Analyse,
        );
        assert_eq!(plan.mode, ExplainMode::Analyse);
        assert!(plan.warning.is_none());
        let analysis = plan.analysis.as_ref().unwrap();
        assert_eq!(analysis.nodes.len(), plan.nodes.len());
        let root = &analysis.nodes[0];
        assert_eq!(root.startup_ms.as_deref(), Some("0.000"));
        assert_eq!(root.total_ms.as_deref(), Some("1.5e-3"));
        assert_eq!(root.rows.as_deref(), Some("0"));
        assert!(!root.never_executed);
        assert_eq!(analysis.nodes[1].loops.as_deref(), Some("250"));
        assert!(analysis.nodes[2].never_executed);
        assert_eq!(analysis.nodes[3], ActualMetrics::default());
        assert_eq!(
            analysis.summary,
            PlanSummary {
                planning_time_ms: Some("0.081".into()),
                execution_time_ms: Some("12.004".into()),
            }
        );
        // Actual values are promoted once; estimates and unknown fields remain.
        let keys: Vec<_> = plan.nodes[0]
            .details
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        assert!(keys.contains(&"Plan Rows") && keys.contains(&"Future Field"));
        assert!(!keys.iter().any(|key| key.starts_with("Actual")));
        assert_eq!(plan.raw, raw);

        let estimated = parse(DriverKind::Postgres, "SELECT".into(), json_result(raw));
        assert_eq!(estimated.mode, ExplainMode::Plan);
        assert!(estimated.analysis.is_none());
        assert!(
            estimated.nodes[0]
                .details
                .iter()
                .any(|(k, _)| k == "Actual Rows")
        );
    }

    #[test]
    fn analyse_summary_requires_one_document_and_unknown_formats_fall_back_to_raw() {
        let two = r#"[{"Plan":{"Node Type":"Result"},"Execution Time":1},{"Plan":{"Node Type":"Result"},"Execution Time":2}]"#;
        let plan = parse_mode(
            DriverKind::Postgres,
            String::new(),
            json_result(two),
            ExplainMode::Analyse,
        );
        let analysis = plan.analysis.unwrap();
        assert_eq!(analysis.nodes.len(), 2);
        assert_eq!(analysis.summary, PlanSummary::default());
        for raw in ["not json", r#"{"Plan":{}}"#, "[]"] {
            let plan = parse_mode(
                DriverKind::Postgres,
                String::new(),
                json_result(raw),
                ExplainMode::Analyse,
            );
            assert_eq!(plan.mode, ExplainMode::Analyse);
            assert_eq!(plan.raw, raw);
            assert!(plan.warning.is_some());
            assert!(plan.nodes.is_empty());
            assert!(plan.analysis.as_ref().unwrap().nodes.is_empty());
        }
    }

    #[test]
    fn mysql_structural_operators_and_native_cost_strings_are_retained() {
        let raw = r#"{"query_block":{"select_id":1,"cost_info":{"query_cost":"2.50"},"nested_loop":[{"table":{"table_name":"a","access_type":"ALL","rows_examined_per_scan":5,"cost_info":{"read_cost":"1.00","prefix_cost":"1.50"}}},{"table":{"table_name":"b","access_type":"ref","rows_produced_per_join":7}}]}}"#;
        let plan = parse(DriverKind::MySql, String::new(), json_result(raw));
        assert!(plan.warning.is_none());
        assert_eq!(plan.nodes.len(), 4);
        assert_eq!(plan.nodes[1].operation, "nested loop");
        assert_eq!(plan.nodes[2].operation, "table (ALL)");
        assert_eq!(plan.nodes[2].rows, "5");
        assert_eq!(plan.nodes[0].total_cost, "2.50");
        assert_eq!(plan.nodes[2].total_cost, "1.50");
        assert!(plan.nodes.iter().all(|node| node.startup_cost.is_empty()));
        let diagram = plan.diagram.as_ref().unwrap();
        assert_eq!(diagram.len(), 4);
        assert_eq!(diagram.parent(1), Some(0));
        assert_eq!(diagram.parent(2), Some(1));
        assert_eq!(diagram.parent(3), Some(1));
        for raw in [
            "{ invalid /* end query_block */",
            "{\"unknown_format\": 1}",
            "[]",
        ] {
            let plan = parse(DriverKind::MySql, String::new(), json_result(raw));
            assert_eq!(plan.raw, raw);
            assert!(plan.warning.is_some());
            assert!(plan.nodes.is_empty());
            assert_eq!(plan.diagram.unwrap_err(), diagram::DiagramError::Empty);
        }
    }

    #[test]
    fn sqlite_uses_identifiers_instead_of_row_order_and_detects_cycles() {
        let row = |id, parent, operation: &str| {
            vec![
                Value::Int(id),
                Value::Int(parent),
                Value::Int(0),
                Value::Text(operation.into()),
            ]
        };
        let data = QueryResult {
            rows: vec![
                row(9, 4, "SEARCH child"),
                row(4, 0, "SCAN parent"),
                row(12, 0, "USE TEMP B-TREE"),
            ],
            ..Default::default()
        };
        let plan = parse(DriverKind::Sqlite, String::new(), data);
        assert!(plan.warning.is_none());
        assert_eq!(
            plan.nodes
                .iter()
                .map(|node| (node.operation.as_str(), node.depth, node.subtree_end))
                .collect::<Vec<_>>(),
            [
                ("SCAN parent", 0, 2),
                ("SEARCH child", 1, 2),
                ("USE TEMP B-TREE", 0, 3)
            ]
        );
        assert!(
            plan.nodes
                .iter()
                .all(|node| node.total_cost.is_empty() && node.rows.is_empty())
        );
        let diagram = plan.diagram.as_ref().unwrap();
        assert_eq!(diagram.len(), 3);
        assert_eq!(diagram.parent(1), Some(0));
        assert_eq!(diagram.parent(2), None);
        assert_eq!(diagram.first_child(0), Some(1));
        let cyclic = QueryResult {
            rows: vec![row(4, 9, "cycle a"), row(9, 4, "cycle b")],
            ..Default::default()
        };
        let plan = parse(DriverKind::Sqlite, String::new(), cyclic);
        assert!(plan.warning.as_deref().unwrap().contains("Cyclic"));
        assert!(plan.raw.contains("cycle a"));
        assert_eq!(plan.diagram.unwrap_err(), diagram::DiagramError::Empty);
    }

    #[test]
    fn diagram_limit_preserves_the_complete_native_plan_and_tree() {
        let mut raw = String::from(r#"[{"Plan":{"Node Type":"Append","Plans":["#);
        for index in 0..diagram::MAX_DIAGRAM_NODES {
            if index > 0 {
                raw.push(',');
            }
            raw.push_str(r#"{"Node Type":"Scan"}"#);
        }
        raw.push_str("]}}]");
        let plan = parse(
            DriverKind::Postgres,
            "SELECT original".into(),
            json_result(&raw),
        );
        assert!(plan.warning.is_none());
        assert_eq!(plan.sql, "SELECT original");
        assert_eq!(plan.raw, raw);
        assert_eq!(plan.nodes.len(), diagram::MAX_DIAGRAM_NODES + 1);
        assert_eq!(plan.nodes[0].subtree_end, plan.nodes.len());
        assert_eq!(plan.nodes.last().unwrap().subtree_end, plan.nodes.len());
        assert_eq!(
            plan.diagram.unwrap_err(),
            diagram::DiagramError::TooManyNodes
        );
    }

    #[test]
    #[ignore = "manual release measurement of native JSON plan normalization, no UI or GPU"]
    #[allow(clippy::assertions_on_constants)]
    fn benchmark_query_plan_normalization() {
        use std::time::Instant;
        assert!(!cfg!(debug_assertions), "run with --release");
        for (mode, count) in [ExplainMode::Plan, ExplainMode::Analyse]
            .into_iter()
            .flat_map(|mode| [100, 1_000, 10_000].map(|count| (mode, count)))
        {
            let children: Vec<_> = (0..count).map(|index| serde_json::json!({"Node Type":"Index Scan","Relation Name":format!("table_{index}_Żółw"),"Total Cost":12.34,"Plan Rows":500,"Index Cond":"(id = 1)","Actual Startup Time":0.004,"Actual Total Time":0.021,"Actual Rows":3,"Actual Loops":index % 3})).collect();
            let raw = serde_json::json!([{"Plan":{"Node Type":"Append","Plans":children},"Planning Time":0.2,"Execution Time":42.1}]).to_string();
            let mut timings = Vec::new();
            for sample in 0..8 {
                let data = json_result(&raw);
                let started = Instant::now();
                let plan = parse_mode(DriverKind::Postgres, String::new(), data, mode);
                let elapsed = started.elapsed();
                assert_eq!(plan.nodes.len(), count + 1);
                assert!(plan.warning.is_none());
                if sample > 0 {
                    timings.push(elapsed);
                }
            }
            timings.sort_unstable();
            println!(
                "query_plan mode={mode:?} nodes={} bytes={} median_ms={:.3} no_ui=true no_gpu=true",
                count + 1,
                raw.len(),
                timings[3].as_secs_f64() * 1000.0
            );
        }
    }
}
