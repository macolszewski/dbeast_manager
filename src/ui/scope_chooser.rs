//! Database and schema chooser of a data source, a popup next to
//! the counter of a data source: which databases (PostgreSQL) or schemas
//! (MySQL) the explorer shows, and which schemas of each database.

use super::explorer::root_visible;
use super::{App, ConnStatus, TreeAction, theme};
use crate::model::{DbObject, DriverKind, ObjectKind};
use egui::{RichText, Ui};

enum Change {
    AllDatabases(bool),
    Database(String, bool),
    Schema(String, String, bool),
    Load(DbObject),
    Refresh,
}

impl App {
    pub(super) fn scope_chooser_window(&mut self, ctx: &egui::Context) {
        let Some(id) = self.scope_chooser.clone() else {
            return;
        };
        let Some(ds) = self.source(&id).cloned() else {
            self.scope_chooser = None;
            return;
        };
        let st = self.sources.get(&id);
        let roots = st.and_then(|s| s.roots.as_deref());
        let connecting = matches!(st.map(|s| &s.status), Some(ConnStatus::Connecting));
        let empty_loading = Default::default();
        let loading = st.map_or(&empty_loading, |s| &s.loading);
        let mut changes = Vec::new();
        let mut open = true;
        let what = if ds.driver == DriverKind::MySql {
            "Schemas"
        } else {
            "Databases and Schemas"
        };
        theme::show_window(ctx, RichText::new(format!("{what} · {}", ds.display_name())).font(theme::semibold(13.0)), &mut open, |window| window
        .id(egui::Id::new(("scope_chooser", &id)))
        
        .collapsible(false)
        .resizable(true)
        .default_size([400.0, 460.0])
        .default_pos(egui::pos2(340.0, 110.0)), |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Shown in Database Explorer").color(theme::TEXT_MUTED));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::tool_button(ui, "⟳", "Reload the list from the server").clicked() {
                        changes.push(Change::Refresh);
                    }
                });
            });
            theme::hairline(ui);
            let Some(roots) = &roots else {
                ui.add_space(8.0);
                if connecting {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Connecting…");
                    });
                } else {
                    ui.label(
                        RichText::new("Connect to list the databases of the server.")
                            .color(theme::TEXT_MUTED),
                    );
                    if ui.button("Connect").clicked() {
                        changes.push(Change::Refresh);
                    }
                }
                return;
            };
            let mut all = ds.scope.all_databases;
            let label = if ds.driver == DriverKind::MySql {
                "All schemas"
            } else {
                "All databases"
            };
            if ui
                .checkbox(&mut all, RichText::new(label).strong())
                .changed()
            {
                changes.push(Change::AllDatabases(all));
            }
            ui.add_space(2.0);
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for node in roots
                        .iter()
                        .filter(|r| matches!(r.kind, ObjectKind::Database | ObjectKind::Schema))
                    {
                        database_row(ui, &ds, node, loading, &mut changes);
                    }
                });
        });
        if !open {
            self.scope_chooser = None;
        }
        self.apply_scope_changes(&id, changes);
    }

    fn apply_scope_changes(&mut self, id: &str, changes: Vec<Change>) {
        if changes.is_empty() {
            return;
        }
        let names: Vec<String> = self
            .sources
            .get(id)
            .and_then(|s| s.roots.as_ref())
            .map(|r| r.iter().map(|n| n.name.clone()).collect())
            .unwrap_or_default();
        let mut save = false;
        for change in changes {
            match change {
                Change::Refresh => self.tree_action(TreeAction::Refresh(id.to_string())),
                Change::Load(node) => self.load_node(id, node),
                change => {
                    let Some(ds) = self.config.data_sources.iter_mut().find(|d| d.id == id) else {
                        continue;
                    };
                    let scope = &mut ds.scope;
                    match change {
                        Change::AllDatabases(on) => {
                            scope.all_databases = on;
                            if !on {
                                scope.databases.clear();
                            }
                        }
                        Change::Database(name, on) => {
                            if !on && scope.all_databases {
                                // Leaving "all" keeps every other database selected.
                                scope.all_databases = false;
                                scope.databases =
                                    names.iter().filter(|n| **n != name).cloned().collect();
                            } else if on {
                                if !scope.databases.contains(&name) {
                                    scope.databases.push(name);
                                }
                            } else {
                                scope.databases.retain(|d| *d != name);
                            }
                        }
                        Change::Schema(db, schema, on) => {
                            let hidden = scope.hidden_schemas.entry(db.clone()).or_default();
                            hidden.retain(|s| *s != schema);
                            if !on {
                                hidden.push(schema);
                            }
                            if hidden.is_empty() {
                                scope.hidden_schemas.remove(&db);
                            }
                        }
                        Change::Refresh | Change::Load(_) => {}
                    }
                    save = true;
                }
            }
        }
        if save {
            self.explorer_ui.invalidate();
            self.save_config();
        }
    }
}

fn database_row(
    ui: &mut Ui,
    ds: &crate::model::DataSource,
    node: &DbObject,
    loading: &std::collections::HashSet<Vec<String>>,
    changes: &mut Vec<Change>,
) {
    let is_default = node.detail == crate::drivers::DEFAULT_DATABASE
        || (ds.driver == DriverKind::MySql && ds.database == node.name);
    let shown = root_visible(ds, node);
    let partial = shown
        && ds
            .scope
            .hidden_schemas
            .get(&node.name)
            .is_some_and(|h| !h.is_empty());
    // Returns the new checked state when the user toggled the database.
    let checkbox = |ui: &mut Ui| -> Option<bool> {
        let mut on = shown;
        let mut text = RichText::new(&node.name).strong();
        if is_default {
            text = text.color(theme::TEXT);
        }
        let r = ui
            .add_enabled(
                !is_default,
                egui::Checkbox::new(&mut on, text).indeterminate(partial),
            )
            .on_disabled_hover_text("The default database of the data source is always shown");
        if is_default {
            ui.label(RichText::new("(Default database)").color(theme::TEXT_MUTED));
        }
        r.changed().then_some(on)
    };
    // Only PostgreSQL databases have schemas below them.
    if ds.driver != DriverKind::Postgres {
        let toggled = ui
            .horizontal(|ui| {
                ui.add_space(18.0);
                checkbox(ui)
            })
            .inner;
        if let Some(on) = toggled {
            changes.push(Change::Database(node.name.clone(), on));
        }
        return;
    }
    let state = egui::collapsing_header::CollapsingState::load_with_default_open(
        ui.ctx(),
        ui.id().with(("scope_db", &node.name)),
        false,
    );
    let was_open = state.is_open();
    let (_, header, body) = state.show_header(ui, checkbox).body(|ui| {
        let mut toggled = Vec::new();
        match &node.children {
            Some(schemas) => {
                for s in schemas {
                    let mut on = ds.shows_schema(&node.name, &s.name);
                    if ui.checkbox(&mut on, &s.name).changed() {
                        toggled.push((s.name.clone(), on));
                    }
                }
            }
            None if loading.contains(&node.path) => {
                ui.spinner();
            }
            None => {}
        }
        toggled
    });
    if let Some(on) = header.inner {
        changes.push(Change::Database(node.name.clone(), on));
    }
    for (schema, on) in body.as_ref().map(|b| b.inner.clone()).unwrap_or_default() {
        changes.push(Change::Schema(node.name.clone(), schema, on));
    }
    // Schemas of a database are read when it is first expanded here.
    if !was_open && body.is_some() && node.children.is_none() && !loading.contains(&node.path) {
        changes.push(Change::Load(node.clone()));
    }
}
