//! Capture immutable result pages only when the user opens Compare Data.

use super::{App, ServicesNotice, compare::CompareSource};
use std::sync::Arc;

impl App {
    pub(super) fn compare_blocked_reason(
        &self,
        tab_index: usize,
        result_index: usize,
    ) -> Option<&'static str> {
        let Some(tab) = self.tabs.get(tab_index) else {
            return Some("Open a result grid first");
        };
        let Some(result) = tab.results.get(result_index) else {
            return Some("Open a result grid first");
        };
        if tab.is_running() || tab.context_pending.is_some() || tab.saving_file {
            return Some("Wait for the source operation to finish");
        }
        if result.action_state().has_changes {
            return Some("Submit or revert pending changes before comparing data");
        }
        if result.data.columns.is_empty() {
            return Some("This result has no columns to compare");
        }
        None
    }

    pub(super) fn open_compare_data(&mut self, tab_index: usize, result_index: usize) {
        let Some(tab) = self.tabs.get(tab_index) else {
            return;
        };
        let Some(result) = tab.results.get(result_index) else {
            return;
        };
        let left = (tab.id, result.action_identity());
        // A text buffer belongs to the source result, never to the comparison.
        let sources = self.capture_compare_sources();
        if let Some(reason) = self.compare_blocked_reason(tab_index, result_index) {
            self.notice = Some(ServicesNotice::info(format!("Compare Data: {reason}")));
            return;
        }
        self.pending_editor_focus = false;
        self.pending_action_focus = None;
        self.compare.open(sources, left);
    }

    fn capture_compare_sources(&mut self) -> Vec<CompareSource> {
        for tab in &mut self.tabs {
            for result in &mut tab.results {
                result.stage_action_buffer();
            }
        }
        let mut snapshots = Vec::new();
        for (tab_index, tab) in self.tabs.iter().enumerate() {
            for (result_index, result) in tab.results.iter().enumerate() {
                if result.data.columns.is_empty() {
                    continue;
                }
                let (source, location) = self.compare_location(result);
                let count = result.data.rows.len();
                let range = if count == 0 {
                    "0 rows".into()
                } else {
                    format!(
                        "rows {}-{}",
                        result.page.skip.saturating_add(1),
                        result.page.skip.saturating_add(count)
                    )
                };
                let truncated = if result.data.truncated {
                    "; more rows available"
                } else {
                    ""
                };
                let limit = if result.page.limit == usize::MAX {
                    "All".into()
                } else {
                    result.page.limit.to_string()
                };
                snapshots.push(CompareSource {
                    id: (tab.id, result.action_identity()),
                    label: format!("{} / {} · {} [{}.{}]", tab.title, result.title, source, tab.id, result_index + 1),
                    sql: result.sql.clone(),
                    scope: format!("Fetched pages · {range} · offset {} · limit {limit}{truncated} · {location}", result.page.skip),
                    data: Arc::clone(&result.data),
                    key: result.primary_key_columns().to_vec(),
                    blocked: self.compare_blocked_reason(tab_index, result_index).map(str::to_owned),
                });
            }
        }
        snapshots
    }

    fn compare_location(&self, result: &super::ResultSet) -> (String, String) {
        use crate::model::{DriverKind, SchemaSelection};
        let origin = result.origin.as_ref();
        let captured = origin.and_then(|origin| origin.source.as_ref());
        let mut name = "Saved result".to_owned();
        let target = if let Some(captured) = captured {
            name = "Captured source".to_owned();
            if let Some(origin) = origin
                && let Some(source) = self.source(&origin.ds_id)
            {
                let mut current = source.identity();
                if let Some(database) = &origin.context.database {
                    current.database.clone_from(database);
                }
                if current == *captured {
                    name.clone_from(&source.name);
                }
            }
            if captured.driver == DriverKind::Sqlite {
                format!("SQLite {}", captured.file)
            } else {
                format!(
                    "{}:{} / {}",
                    captured.host, captured.port, captured.database
                )
            }
        } else {
            "original target unavailable".into()
        };
        let resolved = result.data.context.as_ref();
        let database = resolved
            .and_then(|context| context.database.as_deref())
            .or_else(|| origin.and_then(|origin| origin.context.database.as_deref()));
        let namespace = resolved
            .map(|context| {
                let schemas = context.schemas.join(", ");
                match &context.search_path {
                    Some(path) => format!("search_path {path} (resolved: {schemas})"),
                    None => format!("schemas {schemas}"),
                }
            })
            .or_else(|| {
                origin.and_then(|origin| match &origin.context.schema {
                    SchemaSelection::Named(name) => Some(format!("schema {name}")),
                    SchemaSelection::Default => None,
                })
            });
        let mut location = format!("{name} · {target}");
        if let Some(database) = database {
            location.push_str(&format!(" · database {database}"));
        }
        if let Some(namespace) = namespace {
            location.push_str(&format!(" · {namespace}"));
        }
        if let Some(temporary) = resolved.and_then(|context| context.temporary_schema.as_deref()) {
            location.push_str(&format!(" · temporary schema {temporary}"));
        }
        (name, location)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Column, Page, QueryResult, Value};
    use crate::ui::{ResultSet, TabKind, actions::tests::isolated_app};

    fn captured_result(source: &crate::model::DataSource) -> ResultSet {
        let mut result = ResultSet::new(
            "Result".into(),
            "select * from users".into(),
            QueryResult {
                columns: vec![Column {
                    name: "id".into(),
                    type_name: "INT".into(),
                }],
                ..Default::default()
            },
            Page::first(500),
        );
        result.origin = Some(crate::ui::results::ResultOrigin {
            ds_id: source.id.clone(),
            source: Some(source.identity()),
            context: Default::default(),
            bound_query: None,
        });
        result
    }

    #[test]
    fn source_reconfiguration_does_not_relabel_an_old_snapshot() {
        let mut app = isolated_app(&egui::Context::default());
        let mut source = crate::model::DataSource::new(crate::model::DriverKind::Sqlite);
        source.name = "Original".into();
        source.file = "/original.db".into();
        let result = captured_result(&source);
        source.name = "Replacement".into();
        source.file = "/replacement.db".into();
        app.config.data_sources.push(source);
        let (name, scope) = app.compare_location(&result);
        assert_eq!(name, "Captured source");
        assert!(scope.contains("/original.db"));
        assert!(!scope.contains("Replacement") && !scope.contains("/replacement.db"));
    }

    #[test]
    fn comparison_scope_keeps_the_captured_resolved_namespace() {
        let app = isolated_app(&egui::Context::default());
        let mut source = crate::model::DataSource::new(crate::model::DriverKind::Postgres);
        source.database = "shared".into();
        let mut result = captured_result(&source);
        for namespace in ["tenant_a", "tenant_b"] {
            Arc::make_mut(&mut result.data).context = Some(crate::model::ResolvedContext {
                database: Some("shared".into()),
                search_path: Some(namespace.into()),
                schemas: vec![namespace.into()],
                temporary_schema: Some("pg_temp_3".into()),
            });
            let (_, scope) = app.compare_location(&result);
            assert!(scope.contains(&format!("search_path {namespace}")));
            assert!(scope.contains(&format!("resolved: {namespace}")));
            assert!(scope.contains("temporary schema pg_temp_3"));
        }
    }

    #[test]
    fn capture_shares_pages_and_survives_source_close_without_serializing_rows() {
        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx);
        let index = app.add_tab(
            "Duplicate title".into(),
            "missing".into(),
            TabKind::Console { sql: String::new() },
        );
        app.tabs[index].results.push(ResultSet::new(
            "Result".into(),
            "select payload".into(),
            QueryResult {
                columns: vec![Column {
                    name: "payload".into(),
                    type_name: "BLOB".into(),
                }],
                rows: vec![vec![Value::Bytes(vec![7; 1_000_000])]],
                truncated: true,
                ..Default::default()
            },
            Page {
                skip: 1000,
                limit: 500,
            },
        ));
        let source = Arc::downgrade(&app.tabs[index].results[0].data);
        let snapshots = app.capture_compare_sources();
        assert!(Arc::ptr_eq(
            &snapshots[0].data,
            &app.tabs[index].results[0].data
        ));
        assert!(snapshots[0].scope.contains("offset 1000"));
        assert!(snapshots[0].scope.contains("more rows available"));
        assert!(
            snapshots[0].blocked.is_none(),
            "saved results need no live connection"
        );
        app.tabs.clear();
        assert!(source.upgrade().is_some());
        assert_eq!(
            snapshots[0].data.rows[0][0],
            Value::Bytes(vec![7; 1_000_000])
        );
        drop(snapshots);
        assert!(source.upgrade().is_none());
    }

    #[test]
    fn pending_changes_on_either_source_are_captured_as_blocked() {
        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx);
        for _ in 0..2 {
            let index = app.add_tab(
                "Same title".into(),
                "missing".into(),
                TabKind::Console { sql: String::new() },
            );
            let mut result = ResultSet::new(
                "Result".into(),
                "select 1".into(),
                QueryResult {
                    columns: vec![Column {
                        name: "id".into(),
                        type_name: "INT".into(),
                    }],
                    rows: vec![vec![Value::Int(1)]],
                    ..Default::default()
                },
                Page::first(500),
            );
            result.edits.deleted.insert(0);
            app.tabs[index].results.push(result);
        }
        let snapshots = app.capture_compare_sources();
        assert_ne!(snapshots[0].id, snapshots[1].id);
        assert_ne!(snapshots[0].label, snapshots[1].label);
        assert!(snapshots.iter().all(|source| {
            source
                .blocked
                .as_deref()
                .is_some_and(|reason| reason.contains("Submit or revert"))
        }));
    }
}
