//! The working session kept between runs: open tabs with their
//! text, filters and last results, the active tab and the explorer tree. Nothing
//! connects on start; saved data is shown until a tab is run again.

use super::results::{LogLine, ResultSet};
use super::{App, ServicesView, SourceState, TabKind};
use crate::config::Config;
use crate::model::{DbObject, Page, QueryResult, SchemaSelection};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::collections::BTreeMap;
use std::ops::Range;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Output lines kept per tab.
const LOG_LIMIT: usize = 500;
/// How often changes are written while the application runs.
const SAVE_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub(super) struct Session {
    tabs: Vec<SavedTab>,
    active: usize,
    expanded: Vec<(String, Vec<String>)>,
    selected: Option<(String, Vec<String>)>,
    /// Explorer tree of each data source as last loaded.
    trees: BTreeMap<String, Vec<DbObject>>,
    /// Database Graph settings and pinned objects; never data.
    graph_layouts: BTreeMap<String, super::schema_graph::SavedGraphLayout>,
}

#[derive(Serialize, Deserialize)]
struct SavedTab {
    title: String,
    ds_id: String,
    #[serde(default)]
    database: Option<String>,
    #[serde(default)]
    schema: SchemaSelection,
    kind: TabKind,
    #[serde(default)]
    bound_query: Option<std::sync::Arc<crate::model::CapturedRelatedQuery>>,
    #[serde(default)]
    transposed: bool,
    #[serde(default)]
    record_view: bool,
    #[serde(default)]
    aggregate_view: bool,
    #[serde(default = "value_editor_default")]
    show_value_editor: bool,
    #[serde(default)]
    cursor: Option<Range<usize>>,
    #[serde(default)]
    folds: Vec<Range<usize>>,
    #[serde(default)]
    results: Vec<SavedResult>,
    /// Result shown in the Services panel; `None` for the output log.
    #[serde(default)]
    view: Option<usize>,
    #[serde(default)]
    log: Vec<LogLine>,
}

#[derive(Serialize, Deserialize)]
struct SavedResult {
    #[serde(default)]
    origin: Option<super::results::ResultOrigin>,
    title: String,
    sql: String,
    page: Page,
    #[serde(default)]
    sort: Option<(usize, bool)>,
    #[serde(default)]
    transposed: bool,
    #[serde(default)]
    record_view: bool,
    #[serde(default)]
    aggregate_view: bool,
    #[serde(default = "value_editor_default")]
    show_value_editor: bool,
    data: Box<RawValue>,
}

fn value_editor_default() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DataSource, DriverKind, ExecutionContext, RelatedDirection, Value};
    use std::sync::Arc;

    #[test]
    fn related_session_restores_typed_parameters_but_never_runtime_authority() {
        let (mut data, relation, identity, context) = crate::related::tests::fixture();
        data.rows[0] = vec![
            Value::Decimal("18446744073709551615".into()),
            Value::Bytes(vec![0, 255]),
        ];
        let query = Arc::new(
            crate::related::capture(
                &data,
                0,
                &relation,
                RelatedDirection::Referenced,
                identity,
                context,
            )
            .unwrap(),
        );
        let mut ds = DataSource::new(DriverKind::Sqlite);
        ds.file = query.source_identity.file.clone();
        ds.read_only = true;
        let ctx = egui::Context::default();
        let mut app = super::super::actions::tests::isolated_app(&ctx);
        app.config.data_sources.push(ds.clone());
        let index = app.add_tab(
            "Related: main.accounts".into(),
            ds.id.clone(),
            TabKind::Console {
                sql: "stale preview text".into(),
            },
        );
        app.tabs[index].bound_query = Some(query.clone());
        let mut result = ResultSet::new(
            "accounts".into(),
            crate::sqlgen::related_query(DriverKind::Sqlite, &query)
                .unwrap()
                .sql,
            data,
            Page { skip: 5, limit: 5 },
        );
        result.origin = Some(super::super::results::ResultOrigin {
            ds_id: ds.id.clone(),
            source: Some(ds.identity()),
            context: ExecutionContext::default(),
            bound_query: Some(query.clone()),
        });
        app.tabs[index].results.push(result);
        app.tabs[index].view = ServicesView::Result(0);
        let serialized = serde_json::to_string(&app.session()).unwrap();
        assert!(!serialized.contains("session_epoch"));
        let mut restored = super::super::actions::tests::isolated_app(&ctx);
        restored.config.data_sources.push(ds);
        restored.restore_session(serde_json::from_str(&serialized).unwrap());
        let tab = &restored.tabs[0];
        assert_eq!(tab.bound_query.as_ref().unwrap().values, query.values);
        let TabKind::Console { sql } = &tab.kind else {
            panic!("related console")
        };
        assert_eq!(
            *sql,
            crate::sqlgen::related_query(DriverKind::Sqlite, &query)
                .unwrap()
                .sql
        );
        let result = &tab.results[0];
        assert_eq!(result.page.skip, 5);
        assert_eq!(
            result
                .origin
                .as_ref()
                .unwrap()
                .bound_query
                .as_ref()
                .unwrap()
                .values,
            query.values
        );
        assert!(result.data.source.is_none());
        assert!(result.edit_metadata().is_none());
        assert!(restored.result_replay_blocked_reason(0, 0).is_none());
        assert!(tab.running.is_none());
    }
}

/// When the session was last written and what was written.
#[derive(Default)]
pub(super) struct SessionWriter {
    written: String,
    at: Option<Instant>,
}

fn path() -> PathBuf {
    Config::data_dir().join("session.json")
}

/// The saved session, or `None` before the first one is written.
pub(super) fn load() -> Option<Session> {
    let text = std::fs::read_to_string(path()).ok()?;
    match serde_json::from_str(&text) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("session not restored: {e}");
            None
        }
    }
}

/// Session data is local to this machine and may hold query results, so the
/// file is readable only by the owner, like the config.
fn write(text: &str) -> std::io::Result<()> {
    let path = path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    // Renaming keeps the previous session intact if writing is interrupted.
    std::fs::rename(&tmp, &path)
}

/// Opened files are temporary data sources and are not restored.
fn kept(ds_id: &str) -> bool {
    !ds_id.starts_with("file:")
}

impl App {
    fn session(&self) -> Session {
        let tabs = self
            .tabs
            .iter()
            .filter(|t| kept(&t.ds_id))
            .map(|t| SavedTab {
                title: t.title.clone(),
                ds_id: t.ds_id.clone(),
                database: t.database.clone(),
                schema: t.schema.clone(),
                kind: t.kind.clone(),
                bound_query: t.bound_query.clone(),
                transposed: t.transposed,
                record_view: t.record_view,
                aggregate_view: t.aggregate_view,
                show_value_editor: t.show_value_editor,
                cursor: t.cursor.clone(),
                folds: t.folds.clone(),
                results: t
                    .results
                    .iter()
                    .filter_map(|rs| {
                        Some(SavedResult {
                            origin: rs.origin.clone(),
                            title: rs.title.clone(),
                            sql: rs.sql.clone(),
                            page: rs.page,
                            sort: rs.sort,
                            transposed: rs.transposed,
                            record_view: rs.record_view,
                            aggregate_view: rs.aggregate_view,
                            show_value_editor: rs.show_value_editor,
                            data: RawValue::from_string(rs.data_json().to_string()).ok()?,
                        })
                    })
                    .collect(),
                view: match t.view {
                    ServicesView::Result(i) => Some(i),
                    ServicesView::Output | ServicesView::Plan => None,
                },
                log: t.log[t.log.len().saturating_sub(LOG_LIMIT)..].to_vec(),
            })
            .collect();
        let active = self
            .tabs
            .iter()
            .take(self.active)
            .filter(|t| kept(&t.ds_id))
            .count();
        let trees = self
            .sources
            .iter()
            .filter(|(id, _)| kept(id))
            .filter_map(|(id, st)| Some((id.clone(), st.roots.clone()?)))
            .collect();
        Session {
            tabs,
            active,
            expanded: self
                .expanded
                .iter()
                .filter(|(id, _)| kept(id))
                .cloned()
                .collect(),
            selected: self.selected.clone().filter(|(id, _)| kept(id)),
            trees,
            graph_layouts: self
                .graph_layouts
                .iter()
                .filter(|(key, _)| kept(key))
                .map(|(key, layout)| (key.clone(), layout.clone()))
                .collect(),
        }
    }

    pub(super) fn restore_session(&mut self, session: Session) {
        for (id, roots) in session.trees {
            if self.source(&id).is_some() {
                self.sources.insert(
                    id,
                    SourceState {
                        roots: Some(roots),
                        ..Default::default()
                    },
                );
            }
        }
        self.expanded = session
            .expanded
            .into_iter()
            .filter(|(id, _)| self.sources.contains_key(id))
            .collect();
        self.selected = session.selected;
        self.graph_layouts = session.graph_layouts;
        for saved in session.tabs {
            let i = self.add_tab(saved.title, saved.ds_id, saved.kind);
            self.tabs[i].database = saved.database;
            self.tabs[i].schema = saved.schema;
            self.tabs[i].bound_query = saved.bound_query;
            if let Some(query) = &self.tabs[i].bound_query
                && let Ok(generated) =
                    crate::sqlgen::related_query(query.source_identity.driver, query)
            {
                self.tabs[i].kind = TabKind::Console { sql: generated.sql };
            }
            self.update_metadata_key(i);
            let tab = &mut self.tabs[i];
            tab.refresh_on_activate = matches!(tab.kind, TabKind::Table { .. });
            tab.transposed = saved.transposed;
            tab.record_view = saved.record_view && !saved.aggregate_view;
            tab.aggregate_view = saved.aggregate_view;
            tab.show_value_editor = saved.show_value_editor;
            tab.cursor = saved.cursor;
            tab.folds = saved.folds;
            tab.log = saved.log;
            for r in saved.results {
                let Ok(data) = serde_json::from_str::<QueryResult>(r.data.get()) else {
                    continue;
                };
                let mut rs = ResultSet::new(r.title, r.sql, data, r.page);
                rs.origin = r.origin;
                rs.transposed = r.transposed;
                rs.record_view = r.record_view && !r.aggregate_view;
                rs.aggregate_view = r.aggregate_view;
                rs.show_value_editor = r.show_value_editor;
                rs.set_sort(r.sort);
                tab.results.push(rs);
            }
            tab.view = match saved.view {
                Some(v) if v < tab.results.len() => ServicesView::Result(v),
                _ => ServicesView::Output,
            };
        }
        self.active = session.active.min(self.tabs.len().saturating_sub(1));
    }

    /// Writes the session when it changed, at most every [`SAVE_INTERVAL`]
    /// unless `now` is set.
    pub(super) fn save_session(&mut self, now: bool) {
        if !self.config_loaded {
            return;
        }
        if !now
            && self
                .session_writer
                .at
                .is_some_and(|at| at.elapsed() < SAVE_INTERVAL)
        {
            return;
        }
        self.session_writer.at = Some(Instant::now());
        let Ok(text) = serde_json::to_string(&self.session()) else {
            return;
        };
        if text == self.session_writer.written {
            return;
        }
        match write(&text) {
            Ok(()) => self.session_writer.written = text,
            Err(e) => eprintln!("session save failed: {e}"),
        }
    }
}
