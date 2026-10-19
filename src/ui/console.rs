//! Query console and table editor tabs: toolbar, SQL editor with completion,
//! and the data grid of a table tab.

mod actions;
mod cache;
mod context;
mod history;
mod search_actions;
mod search_ui;
pub(super) use actions::{action_has_focus, request_action_focus};
pub(super) use cache::EditorCache;
pub(super) use context::{ContextIntent, ContextUiState};
pub(super) use history::EditorHistory;
pub(super) use search_ui::SearchState;

use super::{
    App, COMPLETE, CompletionPopup, Field, HistoryPopup, RUN_SCRIPT, RUN_STATEMENT, RunKind, STOP,
    Tab, TabKind, actions::ActionId, highlight, results, theme,
};
use crate::completion::{self, ItemKind};
use crate::config::HistoryEntry;
use crate::folding;
use crate::model::{DataSource, DbObject, DriverKind, SchemaSelection, TableMeta};
use crate::query_plan::ExplainMode;
use egui::text::{CCursor, CCursorRange};
use egui::{Align, Color32, Key, KeyboardShortcut, Layout, Modifiers, RichText, Ui};

#[derive(Default)]
struct ToolbarOutput {
    run: Option<bool>,
    format: bool,
    explain: Option<ExplainMode>,
    stop: bool,
    commit: Option<bool>,
    changed: bool,
    context: Option<ContextIntent>,
}

impl App {
    pub(super) fn editor(&mut self, ui: &mut Ui) {
        if ui.rect_contains_pointer(ui.max_rect()) && ui.input(|input| input.pointer.any_pressed())
        {
            self.explorer_ui.cancel_reveal();
        }
        let idx = self.active;
        if self.tabs.get(idx).is_none() {
            self.branding.splash(ui, super::branding::SplashStatus::Workspace);
            return;
        }
        let sources: Vec<(String, String)> = self
            .config
            .data_sources
            .iter()
            .chain(self.files.iter().map(|f| &f.source))
            .map(|d| (d.id.clone(), d.display_name()))
            .collect();
        let ds_id = self.tabs[idx].ds_id.clone();
        let driver = super::find_source(&self.config, &self.files, &ds_id)
            .map_or(DriverKind::Postgres, |d| d.driver);
        let page_size = self.config.max_rows;
        let metadata = self.meta.get(&self.tabs[idx].metadata_key);
        let meta: &[TableMeta] = metadata.map_or(&[], |metadata| metadata.tables.as_slice());
        let default_path = metadata.map_or(&[][..], |metadata| {
            metadata.default_context.schemas.as_slice()
        });
        let catalog = context::CatalogView {
            metadata,
            loading: self
                .metadata_loading
                .contains_key(&self.tabs[idx].metadata_key),
            error: self
                .metadata_errors
                .get(&self.tabs[idx].metadata_key)
                .map(String::as_str),
        };
        let context_reason = self.context_change_blocked_reason(self.tabs[idx].id);
        let explain_reason = ExplainReasons {
            plan: self.explain_blocked_reason(idx, ExplainMode::Plan),
            analyse: self.explain_blocked_reason(idx, ExplainMode::Analyse),
            driver,
        };
        let filter_history = match &self.tabs[idx].kind {
            TabKind::Table { schema, table, .. } => {
                let key = super::table_key(schema.as_deref(), table);
                [
                    self.config.filters(&ds_id, &key, "WHERE"),
                    self.config.filters(&ds_id, &key, "ORDER BY"),
                ]
            }
            TabKind::Console { .. } => Default::default(),
        };
        let databases = (driver == DriverKind::Postgres
            && matches!(self.tabs[idx].kind, TabKind::Console { .. }))
        .then(|| {
            let ds = super::find_source(&self.config, &self.files, &ds_id)?;
            let roots = self.sources.get(&ds_id)?.roots.as_ref()?;
            let default = ds.database.as_str();
            Some(DatabaseChoice {
                default,
                shown: roots,
                source: ds,
            })
        })
        .flatten();
        let tab = &mut self.tabs[idx];

        let tab_id = tab.id;
        let mut out = toolbar(
            ui,
            tab,
            &sources,
            &self.config.history,
            databases.as_ref(),
            driver,
            &catalog,
            context_reason,
            explain_reason,
        );
        let format_key = KeyboardShortcut::new(Modifiers::ALT.plus(Modifiers::COMMAND), Key::L);
        if matches!(tab.kind, TabKind::Console { .. })
            && (out.format || ui.input_mut(|i| i.consume_shortcut(&format_key)))
        {
            out.format = true;
        }
        ui.separator();
        let mut grid_action = None;
        match &tab.kind {
            TabKind::Console { .. } => {
                out.changed |= console_editor(
                    ui,
                    tab,
                    meta,
                    default_path,
                    driver,
                    out.changed,
                    explain_reason,
                    &mut out.explain,
                )
            }
            TabKind::Table { .. } => {
                let (run, action) = table_view(
                    ui,
                    tab,
                    meta,
                    default_path,
                    &filter_history,
                    driver,
                    page_size,
                    &mut self.tiles,
                );
                out.run = out.run.or(run);
                grid_action = action;
            }
        }

        if ui.input_mut(|i| i.consume_shortcut(&RUN_SCRIPT)) {
            out.run = Some(true);
        } else if ui.input_mut(|i| i.consume_shortcut(&RUN_STATEMENT)) {
            out.run = Some(false);
        }
        if ui.input_mut(|i| i.consume_shortcut(&STOP)) {
            out.stop = true;
        }
        if let Some(intent) = out.context.take() {
            self.apply_console_context(tab_id, intent);
        }
        // The grid gets first refusal of shared shortcuts (notably Record View).
        // Resolve commands only after drawing and releasing the mutable tab borrow.
        if out.format
            || out.explain.is_some()
            || out.stop
            || grid_action.is_some()
            || out.run.is_some()
            || out.commit.is_some()
        {
            let context = self.capture_action_context(ui.ctx());
            if out.format {
                self.dispatch_action(ActionId::ReformatCode, &context, ui.ctx());
            }
            if let Some(mode) = out.explain {
                let action = match mode {
                    ExplainMode::Plan => ActionId::ExplainPlan,
                    ExplainMode::Analyse => ActionId::ExplainAnalyse,
                };
                self.dispatch_action(action, &context, ui.ctx());
            }
            if out.stop {
                self.dispatch_action(ActionId::Stop, &context, ui.ctx());
            }
            if let Some(action) = grid_action {
                match action {
                    results::GridAction::Submit => {
                        self.dispatch_action(ActionId::Submit, &context, ui.ctx());
                    }
                    results::GridAction::Rerun => {
                        self.dispatch_action(ActionId::Reload, &context, ui.ctx());
                    }
                    action => self.grid_action(idx, 0, action),
                }
            } else if let Some(all) = out.run {
                self.dispatch_action(
                    if all {
                        ActionId::ExecuteScript
                    } else {
                        ActionId::Execute
                    },
                    &context,
                    ui.ctx(),
                );
            }
            if let Some(commit) = out.commit {
                self.dispatch_action(
                    if commit {
                        ActionId::Commit
                    } else {
                        ActionId::Rollback
                    },
                    &context,
                    ui.ctx(),
                );
            }
        }
        if out.changed {
            self.explorer_ui.cancel_reveal();
            self.save_config();
        }
    }
}

/// Execute/stop, transaction mode, history and data source switcher.
/// Databases a PostgreSQL console can switch between: the default one first.
struct DatabaseChoice<'a> {
    default: &'a str,
    shown: &'a [DbObject],
    source: &'a DataSource,
}

/// The tab owns its session, file writer and pending edits by source identity.
/// Reassigning it before those operations settle would orphan their state.
pub(super) fn source_change_blocked_reason(tab: &Tab) -> Option<&'static str> {
    if tab.bound_query.is_some() {
        Some("Related rows use a fixed query with bound parameters.")
    } else if tab.context_pending.is_some() {
        Some("Wait for the schema change to finish before changing the console context.")
    } else if tab.is_running() {
        Some("Wait for the running request to finish before changing the data source or database.")
    } else if tab.saving_file {
        Some("Wait for changes to finish saving to the source file.")
    } else if tab.file_save_queued {
        Some("Wait for the queued source file save to finish.")
    } else if tab.unsaved_writes {
        Some("This tab has changes that have not been written to its source file.")
    } else if tab.in_tx {
        Some(
            "Commit or roll back the open transaction before changing the data source or database.",
        )
    } else if tab
        .results
        .iter()
        .any(|result| result.action_state().has_changes)
    {
        Some("Submit or revert pending grid changes before changing the data source or database.")
    } else {
        None
    }
}

#[derive(Clone, Copy)]
struct ExplainReasons {
    driver: DriverKind,
    plan: Option<&'static str>,
    analyse: Option<&'static str>,
}

const MYSQL_ANALYSE_HINT: &str = "Run MySQL EXPLAIN ANALYZE and show actual iterator statistics. Functions and expressions can have side effects. MySQL may omit UPDATE/DELETE writes; this is not a read-only operation. Auto/Manual transaction mode applies; nontransactional changes cannot be rolled back";

const ANALYSE_HINT: &str = "Execute the statement and show actual execution statistics. Data changes are applied: Auto commits them, Manual keeps them in the open transaction";

fn explain_button(ui: &mut Ui, reasons: ExplainReasons, mode: ExplainMode) -> egui::Response {
    let (reason, hint) = match mode {
        ExplainMode::Plan => (reasons.plan, "Show the estimated execution plan"),
        ExplainMode::Analyse => (
            reasons.analyse,
            if reasons.driver == DriverKind::MySql {
                MYSQL_ANALYSE_HINT
            } else {
                ANALYSE_HINT
            },
        ),
    };
    ui.add_enabled(reason.is_none(), egui::Button::new(mode.label()))
        .on_hover_text(hint)
        .on_disabled_hover_text(reason.unwrap_or_default())
}

/// Both actions in menus; a click on one never becomes the default button.
fn explain_menu_items(ui: &mut Ui, reasons: ExplainReasons, explain: &mut Option<ExplainMode>) {
    for mode in [ExplainMode::Plan, ExplainMode::Analyse] {
        if explain_button(ui, reasons, mode).clicked() {
            *explain = Some(mode);
            ui.close();
        }
    }
}

fn toolbar_button_width(ui: &Ui, text: &str) -> f32 {
    let font = egui::TextStyle::Button.resolve(ui.style());
    ui.fonts_mut(|fonts| {
        fonts
            .layout_no_wrap(text.to_owned(), font, Color32::WHITE)
            .size()
            .x
    }) + 2.0 * ui.spacing().button_padding.x
}

#[allow(clippy::too_many_arguments)]
fn toolbar(
    ui: &mut Ui,
    tab: &mut Tab,
    sources: &[(String, String)],
    history: &[HistoryEntry],
    databases: Option<&DatabaseChoice<'_>>,
    driver: DriverKind,
    catalog: &context::CatalogView<'_>,
    context_reason: Option<&str>,
    explain_reason: ExplainReasons,
) -> ToolbarOutput {
    let mut out = ToolbarOutput::default();
    let running = tab.is_running();
    let is_console = matches!(tab.kind, TabKind::Console { .. });
    let source_change_reason = if is_console {
        context_reason
    } else {
        Some("A table editor keeps the data source it was opened from.")
    };
    let databases = databases.filter(|choice| choice.shown.len() > 1);
    let gap = ui.spacing().item_spacing.x;
    let primary_width = 2.0 * 26.0
        + 100.0
        + 5.0 * gap
        + 16.0
        + if tab.in_tx {
            toolbar_button_width(ui, "✔ Commit").max(26.0)
                + toolbar_button_width(ui, "Rollback").max(26.0)
                + 2.0 * gap
        } else {
            0.0
        };
    let secondary_width = ["Explain Plan", "Find", "Format", "🕘 History"]
        .iter()
        .map(|text| toolbar_button_width(ui, text).max(26.0))
        .sum::<f32>()
        + toolbar_button_width(ui, "⏷").max(18.0)
        + 26.0
        + 8.0 * gap
        + 24.0;
    let overflow_width = 32.0 + gap;
    let context_slots = 1 + usize::from(is_console) + usize::from(databases.is_some());
    let min_context_width = 120.0
        + if is_console { 110.0 + gap } else { 0.0 }
        + if databases.is_some() { 80.0 + gap } else { 0.0 };
    let max_context_width = 200.0
        + if is_console { 160.0 + gap } else { 0.0 }
        + if databases.is_some() {
            110.0 + gap
        } else {
            0.0
        };
    let width = ui.available_width();
    // Reserve real space for both groups. At very narrow sizes selectors get
    // their own row; ordinary widths keep primary actions and use overflow.
    let two_rows = width < primary_width + min_context_width + overflow_width + gap;
    let reserve_secondary =
        is_console && width >= primary_width + secondary_width + min_context_width + gap;
    let reserved_tools = primary_width
        + if reserve_secondary {
            secondary_width
        } else {
            overflow_width
        };
    let context_width = if two_rows {
        width.min(max_context_width)
    } else {
        (width - reserved_tools - gap).clamp(min_context_width, max_context_width)
    };
    let tools_width = if two_rows {
        width
    } else {
        width - context_width - gap
    };
    let show_secondary = is_console && tools_width >= primary_width + secondary_width;
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(width, if two_rows { 56.0 } else { 28.0 }),
        egui::Sense::hover(),
    );
    let tools_rect = egui::Rect::from_min_size(rect.min, egui::vec2(tools_width, 28.0));
    let context_rect = egui::Rect::from_min_size(
        egui::pos2(
            rect.right() - context_width,
            rect.top() + if two_rows { 28.0 } else { 0.0 },
        ),
        egui::vec2(context_width, 28.0),
    );
    ui.scope_builder(
        egui::UiBuilder::new()
            .id_salt(("console_tools", tab.id))
            .max_rect(tools_rect)
            .layout(Layout::left_to_right(Align::Center)),
        |ui| {
            theme::flat_widgets(ui);
            let run_color = if running {
                theme::TEXT_MUTED
            } else {
                theme::GREEN
            };
            let run_enabled = !running && tab.context_pending.is_none();
            if theme::tool_button_enabled(
                ui,
                run_enabled,
                RichText::new("▶").color(run_color),
                &theme::keys("Execute (⌘↵)"),
            )
            .clicked()
            {
                out.run = Some(false);
            }
            if show_secondary && is_console {
                if explain_button(ui, explain_reason, ExplainMode::Plan).clicked() {
                    out.explain = Some(ExplainMode::Plan);
                }
                let response = theme::tool_menu(ui, "⏷", "Explain Actions", |ui| {
                    explain_menu_items(ui, explain_reason, &mut out.explain);
                });
                response.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Explain Actions")
                });
            }
            if show_secondary
                && theme::tool_button_enabled(
                    ui,
                    run_enabled && tab.bound_query.is_none(),
                    RichText::new("▶▶").color(run_color),
                    &theme::keys("Execute script (⌘⇧↵)"),
                )
                .clicked()
            {
                out.run = Some(true);
            }
            let stop_color = if running {
                theme::RED
            } else {
                theme::TEXT_MUTED
            };
            if theme::tool_button_enabled(
                ui,
                running,
                RichText::new("■").color(stop_color),
                &theme::keys("Cancel running statement (⌘F2)"),
            )
            .clicked()
            {
                out.stop = true;
            }
            ui.separator();
            let mode = if tab.manual_tx {
                "Tx: Manual"
            } else {
                "Tx: Auto"
            };
            ui.add_enabled_ui(tab.context_pending.is_none(), |ui| {
                egui::ComboBox::from_id_salt(("tx_mode", tab.id))
                    .selected_text(mode)
                    .width(100.0)
                    .truncate()
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut tab.manual_tx, false, "Auto-commit");
                        ui.selectable_value(&mut tab.manual_tx, true, "Manual");
                    })
                    .response
                    .on_hover_text(if tab.in_tx {
                        "Transaction mode: transaction open"
                    } else {
                        "Transaction mode"
                    });
            });
            if tab.in_tx {
                if theme::tool_button_enabled(
                    ui,
                    run_enabled,
                    RichText::new("✔ Commit").color(theme::GREEN),
                    "Commit transaction",
                )
                .clicked()
                {
                    out.commit = Some(true);
                }
                if theme::tool_button_enabled(
                    ui,
                    run_enabled,
                    RichText::new("Rollback").color(theme::RED),
                    "Roll back transaction",
                )
                .clicked()
                {
                    out.commit = Some(false);
                }
            }
            if is_console {
                ui.separator();
                if show_secondary {
                    theme::tool_menu(ui, "Find", "Find and navigate in the console", |ui| {
                        console_find_menu(ui, tab);
                    });
                    if theme::tool_button_enabled(
                        ui,
                        tab.bound_query.is_none(),
                        "Format",
                        &theme::keys("Reformat code (⌥⌘L): the selection, or the whole console"),
                    )
                    .clicked()
                    {
                        out.format = true;
                    }
                    theme::tool_menu(
                        ui,
                        "🕘 History",
                        "Statements executed on this data source",
                        |ui| {
                            console_history_menu(ui, tab, history, &mut out);
                        },
                    );
                } else {
                    let response = theme::tool_menu(ui, "…", "More Console Actions", |ui| {
                        explain_menu_items(ui, explain_reason, &mut out.explain);
                        if ui
                            .add_enabled(
                                run_enabled && tab.bound_query.is_none(),
                                egui::Button::new("Execute Script").shortcut_text(theme::keys("⌘⇧↵")),
                            )
                            .clicked()
                        {
                            out.run = Some(true);
                            ui.close();
                        }
                        ui.separator();
                        console_find_menu(ui, tab);
                        if ui
                            .add_enabled(tab.bound_query.is_none(), egui::Button::new("Format"))
                            .clicked()
                        {
                            out.format = true;
                            ui.close();
                        }
                        ui.menu_button("🕘 History", |ui| {
                            console_history_menu(ui, tab, history, &mut out);
                        });
                    });
                    response.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::Button,
                            true,
                            "More Console Actions",
                        )
                    });
                }
            }
            if tab.in_tx
                && ui.available_width() > toolbar_button_width(ui, "transaction open") + gap
            {
                ui.label(
                    RichText::new("transaction open")
                        .small()
                        .color(theme::SQL_KEYWORD),
                );
            }
        },
    );
    ui.scope_builder(
        egui::UiBuilder::new()
            .id_salt(("console_context", tab.id))
            .max_rect(context_rect)
            .layout(Layout::right_to_left(Align::Center)),
        |ui| {
            theme::flat_widgets(ui);
            let spare = (context_width - min_context_width).max(0.0) / context_slots as f32;
            let current = sources
                .iter()
                .find(|(id, _)| *id == tab.ds_id)
                .map(|(_, name)| name.as_str())
                .unwrap_or("<choose data source>");
            ui.add_enabled_ui(source_change_reason.is_none(), |ui| {
                if let Some(choice) = databases {
                    let current_db = tab
                        .database
                        .as_deref()
                        .or_else(|| {
                            tab.resolved_context
                                .as_ref()
                                .and_then(|c| c.database.as_deref())
                        })
                        .unwrap_or(choice.default);
                    let control_width = 80.0 + spare;
                    let response = ui
                        .allocate_ui_with_layout(
                            egui::vec2(control_width, 24.0),
                            Layout::right_to_left(Align::Center),
                            |ui| {
                                egui::ComboBox::from_id_salt(("db_switch", tab.id))
                                    .selected_text(if current_db.is_empty() {
                                        "<default>"
                                    } else {
                                        current_db
                                    })
                                    .width(control_width)
                                    .truncate()
                                    .show_ui(ui, |ui| {
                                        for node in choice.shown.iter().filter(|node| {
                                            super::explorer::root_visible(choice.source, node)
                                        }) {
                                            let db = &node.name;
                                            if ui.selectable_label(db == current_db, db).clicked()
                                                && db != current_db
                                                && source_change_reason.is_none()
                                            {
                                                out.context = Some(ContextIntent::Database(
                                                    (db != choice.default).then(|| db.clone()),
                                                ));
                                                ui.close();
                                            }
                                        }
                                    })
                                    .response
                            },
                        )
                        .inner
                        .on_hover_text(format!("Database: {current_db}"));
                    response.widget_info(|| {
                        let mut info = egui::WidgetInfo::labeled(
                            egui::WidgetType::ComboBox,
                            source_change_reason.is_none(),
                            "Database",
                        );
                        info.current_text_value = Some(current_db.to_owned());
                        info
                    });
                    if let Some(reason) = source_change_reason {
                        response.on_disabled_hover_text(reason);
                    }
                }
                let control_width = 120.0 + spare;
                let response = ui
                    .allocate_ui_with_layout(
                        egui::vec2(control_width, 24.0),
                        Layout::right_to_left(Align::Center),
                        |ui| {
                            egui::ComboBox::from_id_salt(("ds_switch", tab.id))
                                .selected_text(format!("🗄 {current}"))
                                .width(control_width)
                                .truncate()
                                .show_ui(ui, |ui| {
                                    for (id, name) in sources {
                                        if ui.selectable_label(*id == tab.ds_id, name).clicked()
                                            && *id != tab.ds_id
                                            && source_change_reason.is_none()
                                        {
                                            out.context = Some(ContextIntent::Source(id.clone()));
                                            ui.close();
                                        }
                                    }
                                })
                                .response
                        },
                    )
                    .inner
                    .on_hover_text(current);
                response.widget_info(|| {
                    let mut info = egui::WidgetInfo::labeled(
                        egui::WidgetType::ComboBox,
                        source_change_reason.is_none(),
                        "Data source",
                    );
                    info.current_text_value = Some(current.to_owned());
                    info
                });
                if let Some(reason) = source_change_reason {
                    response.on_disabled_hover_text(reason);
                }
            });
            if is_console {
                ui.allocate_ui_with_layout(
                    egui::vec2(110.0 + spare, 24.0),
                    Layout::right_to_left(Align::Center),
                    |ui| {
                        out.context =
                            context::schema_selector(ui, tab, driver, catalog, context_reason)
                                .or(out.context.take());
                    },
                );
            }
        },
    );
    out
}

fn console_find_menu(ui: &mut Ui, tab: &mut Tab) {
    for (label, replace) in [(theme::keys("Find…  ⌘F"), false), (theme::keys("Replace…  ⌘R"), true)] {
        if ui
            .add_enabled(
                !replace || tab.bound_query.is_none(),
                egui::Button::new(label),
            )
            .clicked()
        {
            search_actions::open(ui.ctx(), tab, replace);
            ui.close();
        }
    }
    if ui.button(theme::keys("Go to Line/Column…  ⌘L")).clicked() {
        search_actions::open_goto(tab);
        ui.close();
    }
}

fn console_history_menu(
    ui: &mut Ui,
    tab: &mut Tab,
    history: &[HistoryEntry],
    out: &mut ToolbarOutput,
) {
    if tab.bound_query.is_some() {
        ui.label("Related rows use a fixed query with bound parameters.");
        return;
    }
    let mut history = history
        .iter()
        .rev()
        .filter(|entry| entry.ds_id == tab.ds_id)
        .take(50)
        .peekable();
    if history.peek().is_none() {
        ui.label(RichText::new("No statements executed yet").color(theme::TEXT_MUTED));
    }
    egui::ScrollArea::vertical()
        .max_height(360.0)
        .show(ui, |ui| {
            ui.set_min_width(420.0);
            for entry in history {
                let sql = &entry.sql;
                let line: String = sql.lines().next().unwrap_or("").chars().take(70).collect();
                let more = if sql.lines().count() > 1 || sql.chars().count() > 70 {
                    " …"
                } else {
                    ""
                };
                if ui
                    .button(RichText::new(format!("{line}{more}")).monospace())
                    .on_hover_text(&**sql)
                    .clicked()
                {
                    tab.editor_history.checkpoint();
                    if let TabKind::Console { sql: text } = &mut tab.kind {
                        append_statement(ui.ctx(), tab.id, text, sql);
                        tab.folds.clear();
                        out.changed = true;
                    }
                    ui.close();
                }
            }
        });
}

/// Reformats the selection, or the whole console without one. Folds are
/// dropped, since their ranges no longer match the new text.
fn format_console(tab: &mut Tab, driver: DriverKind) -> Result<(), String> {
    let TabKind::Console { sql } = &mut tab.kind else {
        return Ok(());
    };
    let byte = |c: usize| sql.char_indices().nth(c).map_or(sql.len(), |(b, _)| b);
    let selection = tab
        .cursor
        .clone()
        .filter(|c| c.start != c.end)
        .map(|c| byte(c.start)..byte(c.end));
    let range = selection.clone().unwrap_or(0..sql.len());
    let mut formatted = crate::formatter::format(&sql[range.clone()], driver)?;
    if selection.is_some() {
        // Keep the text around the selection as it was.
        formatted.truncate(formatted.trim_end().len());
    }
    if sql[range.clone()] == formatted {
        return Ok(());
    }
    sql.replace_range(range, &formatted);
    tab.folds.clear();
    Ok(())
}

fn field_id(tab_id: u64, field: Field) -> egui::Id {
    egui::Id::new(("sql", tab_id, field))
}

/// Moves the caret of a text field to a char index and focuses it.
fn set_caret(ctx: &egui::Context, id: egui::Id, char_index: usize) {
    let mut state = egui::text_edit::TextEditState::load(ctx, id).unwrap_or_default();
    state
        .cursor
        .set_char_range(Some(CCursorRange::one(CCursor::new(char_index))));
    state.store(ctx, id);
    ctx.memory_mut(|m| m.request_focus(id));
}

/// Appends a statement from history at the end of the console.
fn append_statement(ctx: &egui::Context, tab_id: u64, text: &mut String, sql: &str) {
    let trimmed = text.trim_end().len();
    text.truncate(trimmed);
    if !text.is_empty() {
        text.push_str("\n\n");
    }
    text.push_str(sql);
    text.push(';');
    set_caret(
        ctx,
        field_id(tab_id, Field::Console),
        text.chars().count() - 1,
    );
}

/// Code completion for one text field. The field's text is completed as if it
/// followed `context`, e.g. "SELECT * FROM t WHERE ", so a filter field knows
/// which table's columns to offer.
struct FieldCompletion<'a> {
    tab_id: u64,
    field: Field,
    driver: DriverKind,
    meta: &'a [TableMeta],
    search_path: &'a [String],
    context: String,
}

impl FieldCompletion<'_> {
    fn id(&self) -> egui::Id {
        field_id(self.tab_id, self.field)
    }

    fn popup_area_id(&self) -> egui::Id {
        egui::Id::new(("completion", self.tab_id, self.field))
    }

    /// Popup navigation; must run before the text field so it wins over the
    /// field's own handling of arrows, Enter and Tab. Returns the accepted item.
    fn handle_keys(
        &self,
        ui: &mut Ui,
        popup: &mut Option<CompletionPopup>,
    ) -> (Option<usize>, bool) {
        let focused = ui.ctx().memory(|m| m.has_focus(self.id()));
        let force = focused && ui.input_mut(|i| i.consume_shortcut(&COMPLETE));
        let Some(p) = popup.as_mut().filter(|p| p.field == self.field) else {
            return (None, force);
        };
        let len = p.items.len();
        let mut accept = None;
        let mut close = false;
        ui.input_mut(|i| {
            if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                p.selected = (p.selected + 1).min(len.saturating_sub(1));
            }
            if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                p.selected = p.selected.saturating_sub(1);
            }
            if i.consume_key(Modifiers::NONE, Key::Enter)
                || i.consume_key(Modifiers::NONE, Key::Tab)
            {
                accept = Some(p.selected);
            }
            close = i.consume_key(Modifiers::NONE, Key::Escape);
        });
        if close {
            *popup = None;
        }
        (accept, force)
    }

    /// Refreshes, draws and applies the popup after the field was shown.
    /// Returns true when a completion changed the text.
    #[allow(clippy::too_many_arguments)]
    fn update(
        &self,
        ui: &Ui,
        text: &mut String,
        out: &egui::text_edit::TextEditOutput,
        popup: &mut Option<CompletionPopup>,
        accept: Option<usize>,
        force: bool,
    ) -> bool {
        let ctx = ui.ctx().clone();
        let mine = |p: &Option<CompletionPopup>| p.as_ref().is_some_and(|p| p.field == self.field);
        if let Some(i) = accept
            && mine(popup)
        {
            return self.apply(&ctx, text, popup.take(), i);
        }

        let caret = out.cursor_range.map(|r| r.primary);
        let typed = ui.input(|i| {
            i.events.iter().any(|e| {
                matches!(e, egui::Event::Text(t)
                    if t.chars().last().is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.'))
            })
        });
        let changed = out.response.changed();
        if let Some(c) = caret
            && (force || (changed && typed) || (changed && mine(popup)))
        {
            let cursor = text
                .char_indices()
                .nth(c.index.0)
                .map_or(text.len(), |(b, _)| b);
            let full = format!("{}{text}", self.context);
            let offset = self.context.len();
            let found = completion::complete_in_context(
                &full,
                offset + cursor,
                self.driver,
                self.meta,
                self.search_path,
            );
            let has_prefix = !found.replace.is_empty() || full[..offset + cursor].ends_with('.');
            let usable =
                !found.items.is_empty() && (has_prefix || force) && found.replace.start >= offset;
            if usable {
                *popup = Some(CompletionPopup {
                    field: self.field,
                    items: found.items,
                    replace: found.replace.start - offset..found.replace.end - offset,
                    selected: 0,
                });
            } else if mine(popup) {
                *popup = None;
            }
        }

        let over_popup = ctx
            .memory(|m| m.area_rect(self.popup_area_id()))
            .zip(ctx.pointer_hover_pos())
            .is_some_and(|(r, p)| r.contains(p));
        if mine(popup) && !out.response.has_focus() && !over_popup {
            *popup = None;
        }

        let (Some(p), Some(c)) = (popup.as_ref().filter(|p| p.field == self.field), caret) else {
            return false;
        };
        let caret_rect = out
            .galley
            .pos_from_cursor(c)
            .translate(out.galley_pos.to_vec2());
        let mut clicked = None;
        egui::Area::new(self.popup_area_id())
            .order(egui::Order::Foreground)
            .fixed_pos(caret_rect.left_bottom() + egui::vec2(0.0, 2.0))
            .show(&ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .inner_margin(4)
                    .show(ui, |ui| {
                        ui.set_width(380.0);
                        egui::ScrollArea::vertical()
                            .max_height(240.0)
                            .show(ui, |ui| {
                                for (i, item) in p.items.iter().enumerate() {
                                    let r = completion_row(ui, item, i == p.selected);
                                    if i == p.selected {
                                        r.scroll_to_me(None);
                                    }
                                    if r.clicked() {
                                        clicked = Some(i);
                                    }
                                }
                            });
                    });
            });
        match clicked {
            Some(i) => self.apply(&ctx, text, popup.take(), i),
            None => false,
        }
    }

    fn apply(
        &self,
        ctx: &egui::Context,
        text: &mut String,
        popup: Option<CompletionPopup>,
        index: usize,
    ) -> bool {
        let Some(popup) = popup else { return false };
        let Some(item) = popup.items.get(index) else {
            return false;
        };
        if popup.replace.end > text.len() {
            return false;
        }
        text.replace_range(popup.replace.clone(), &item.insert);
        let caret = text[..popup.replace.start].chars().count() + item.insert.chars().count();
        set_caret(ctx, self.id(), caret);
        true
    }
}

/// TextEdit remains selectable and copyable while every mutation route is denied.
/// Borrow the cached text without allocating a second copy for the pinned view.
struct ConsoleBuffer<'a> {
    text: &'a mut dyn egui::TextBuffer,
    read_only: bool,
}
impl egui::TextBuffer for ConsoleBuffer<'_> {
    fn is_mutable(&self) -> bool {
        !self.read_only && self.text.is_mutable()
    }
    fn as_str(&self) -> &str {
        self.text.as_str()
    }
    fn insert_text(&mut self, text: &str, index: egui::text::CharIndex) -> usize {
        if self.read_only {
            0
        } else {
            self.text.insert_text(text, index)
        }
    }
    fn delete_char_range(&mut self, range: std::ops::Range<egui::text::CharIndex>) {
        if !self.read_only {
            self.text.delete_char_range(range);
        }
    }
    fn clear(&mut self) {
        if !self.read_only {
            self.text.clear();
        }
    }
    fn replace_with(&mut self, text: &str) {
        if !self.read_only {
            self.text.replace_with(text);
        }
    }
    fn take(&mut self) -> String {
        if self.read_only {
            self.text.as_str().to_owned()
        } else {
            self.text.take()
        }
    }
    fn type_id(&self) -> std::any::TypeId {
        std::any::TypeId::of::<ConsoleBuffer<'static>>()
    }
}

/// SQL editor with completion. Returns true when the text changed.
#[allow(clippy::too_many_arguments)]
fn console_editor(
    ui: &mut Ui,
    tab: &mut Tab,
    meta: &[TableMeta],
    default_path: &[String],
    driver: DriverKind,
    source_changed: bool,
    explain_reason: ExplainReasons,
    explain: &mut Option<ExplainMode>,
) -> bool {
    let tab_id = tab.id;
    let read_only = tab.bound_query.is_some();
    if read_only {
        ui.label(RichText::new("Related rows · bound parameters").small().color(theme::TEXT_MUTED))
            .on_hover_text("This generated query is read-only. Execute, paging and export keep its typed parameters.");
    }
    let editor_id = field_id(tab_id, Field::Console);
    let time = ui.input(|i| i.time);
    let TabKind::Console { sql } = &tab.kind else {
        return false;
    };
    tab.editor_history.sync(
        sql,
        tab.cursor.clone().unwrap_or(0..0),
        time,
        source_changed,
    );
    if source_changed {
        tab.editor_history.checkpoint();
    }
    let mut changed = false;
    let mut history_scroll = None;
    if ui.memory(|m| m.has_focus(editor_id)) {
        let redo_key = KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::Z);
        let undo_key = KeyboardShortcut::new(Modifiers::COMMAND, Key::Z);
        let redo = ui.input_mut(|i| i.consume_shortcut(&redo_key));
        let undo = !redo && ui.input_mut(|i| i.consume_shortcut(&undo_key));
        if !read_only
            && (undo || redo)
            && let Some((text, selection)) = tab.editor_history.undo(redo)
        {
            if let TabKind::Console { sql } = &mut tab.kind {
                *sql = text;
            }
            tab.folds.clear();
            tab.cursor = Some(selection.clone());
            search_actions::set_selection(ui.ctx(), editor_id, selection.clone());
            history_scroll = Some(selection);
            changed = true;
        }
    }
    let (search_changed, scroll) = search_actions::prepare(ui, tab, source_changed || changed);
    changed |= search_changed;
    if source_changed || changed {
        tab.editor_cache.invalidate();
    }
    let scroll = scroll.or(history_scroll);
    let fc = FieldCompletion {
        tab_id,
        field: Field::Console,
        driver,
        meta,
        search_path: tab
            .resolved_context
            .as_ref()
            .map(|context| context.schemas.as_slice())
            .unwrap_or_else(|| match &tab.schema {
                SchemaSelection::Named(name) => std::slice::from_ref(name),
                SchemaSelection::Default => default_path,
            }),
        context: String::new(),
    };
    let (accept, force) = if read_only {
        tab.completion = None;
        (None, false)
    } else {
        fc.handle_keys(ui, &mut tab.completion)
    };
    let fold_command = fold_shortcut(ui, fc.id());
    let TabKind::Console { sql } = &mut tab.kind else {
        return false;
    };
    let completion = &mut tab.completion;
    let cursor = &mut tab.cursor;
    let folds = &mut tab.folds;
    let cache = &mut tab.editor_cache;

    // Native history stores folding placeholders. Keep source SQL history instead.
    let selection_before = if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), fc.id()) {
        let selection = state.cursor.char_range();
        state.clear_undoer();
        state.store(ui.ctx(), fc.id());
        selection
    } else {
        None
    };

    // Source SQL and folds remain borrowed until edits are mapped back. The
    // visible buffer and its derived data survive idle frames without copies.
    cache.prepare(ui, sql, folds, driver);
    let revision_before = cache.text.revision.get();
    let char_count_before = cache.projection.char_count();
    let gutter_w = cache.line_count.to_string().len().max(2) as f32 * 8.5 + 40.0;
    let mut toggle_line = None;
    let mut unfold = None;
    egui::Frame::new()
        .fill(theme::BG_EDITOR)
        .inner_margin(egui::Margin { left: 0, right: 6, top: 4, bottom: 4 })
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt(("editor", tab_id))
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.horizontal_top(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        let (gutter, _) = ui.allocate_exact_size(egui::vec2(gutter_w, 1.0), egui::Sense::hover());
                        // Reserved below the text, filled once the caret position is known.
                        let current_line_bg = ui.painter().add(egui::Shape::Noop);
                        let search_bg = ui.painter().add(egui::Shape::Noop);
                        let revision = std::rc::Rc::clone(&cache.text.revision);
                        let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, wrap: f32| {
                            cache.layout.layout(ui, buf, revision.get(), &cache.summaries, wrap)
                        };
                        let mut buffer = ConsoleBuffer { text: &mut cache.text, read_only };
                        let out = egui::TextEdit::multiline(&mut buffer)
                            .id(fc.id())
                            .code_editor()
                            .frame(egui::Frame::NONE)
                            .desired_width(f32::INFINITY)
                            .desired_rows(20)
                            .lock_focus(true)
                            .hint_text(theme::keys("-- write SQL here: ⌘↵ runs the statement under the caret, Ctrl+Space completes"))
                            .layouter(&mut layouter)
                            .show(ui);
                        let layout_current = cache.layout.matches_output(&out.galley);
                        if read_only {
                            ui.ctx().accesskit_node_builder(out.response.id, |node| node.set_read_only());
                        }
                        out.response.context_menu(|ui| {
                            explain_menu_items(ui, explain_reason, explain);
                        });
                        // TextEdit collapses selections when another field owns IME.
                        // Find keeps typing focus while the SQL selection stays ready.
                        if !out.response.has_focus() && let Some(selection) = selection_before {
                            let mut state = out.state.clone();
                            state.cursor.set_char_range(Some(selection));
                            state.store(ui.ctx(), fc.id());
                        }
                        if layout_current && !out.response.changed() {
                            search_actions::paint(ui, &out, &tab.search, search_bg, out.cursor_range.or(selection_before), &cache.layout.row_offsets);
                        }
                        if layout_current && let Some(range) = &scroll {
                            let rect = cache::cursor_rect(&out.galley, &cache.layout.row_offsets, CCursor::new(range.start))
                                .translate(out.galley_pos.to_vec2());
                            ui.scroll_to_rect(rect, Some(Align::Center));
                        }
                        cache.projection.refresh(&cache.text, sql, folds, driver);
                        if let Some(r) = out.cursor_range {
                            let (a, b) = (cache.projection.real_char(r.primary.index.0), cache.projection.real_char(r.secondary.index.0));
                            *cursor = Some(a.min(b)..a.max(b));
                        }
                        if layout_current {
                            toggle_line = paint_gutter(ui, &out, gutter, current_line_bg, &cache.projection, &cache.markers, &cache.layout.row_offsets);
                            paint_current_statement(ui, &out, &cache.projection, gutter, &cache.layout.row_offsets);
                            unfold = paint_fold_boxes(ui, &out, &cache.projection.placeholders, &cache.summaries, sql, folds, &cache.layout.row_offsets);
                        } else {
                            ui.ctx().request_repaint();
                        }
                        if !read_only {
                            cache.text.edit(|text| fc.update(ui, text, &out, completion, accept, force));
                        }
                    });
                });
        });

    // Copied or cut text gets the folded parts back.
    ui.ctx().output_mut(|o| {
        for command in &mut o.commands {
            if let egui::OutputCommand::CopyText(t) = command
                && t.chars().any(|c| folding::placeholder_index(c).is_some())
            {
                *t = folding::expand(t, sql, folds).0;
            }
        }
    });

    if cache.text.revision.get() != revision_before {
        let visible = cache.text.as_str();
        let lost: Vec<usize> = (0..folds.len())
            .filter(|&i| !visible.contains(folding::placeholder(i)))
            .collect();
        let one_char_deleted = visible.chars().count() + 1 == char_count_before;
        if let ([i], true) = (lost.as_slice(), one_char_deleted) {
            // Backspace or Delete on a fold opens it instead of deleting hidden text.
            let opened = folds.remove(*i);
            let caret = folding::visible(&sql[..opened.start], &folds[..*i])
                .chars()
                .count();
            set_caret(ui.ctx(), fc.id(), caret);
        } else {
            let (real, kept) = folding::expand(visible, sql, folds);
            *sql = real;
            *folds = kept;
            changed = true;
        }
        cache.invalidate();
    }

    let caret_byte = fold_command.and(cursor.as_ref()).map(|c| {
        sql.char_indices()
            .nth(c.start)
            .map_or(sql.len(), |(b, _)| b)
    });
    if unfold.is_some() || toggle_line.is_some() || fold_command.is_some() {
        cache.invalidate_folds();
    }
    if let Some(i) = unfold {
        folds.remove(i);
    }
    if let Some(line) = toggle_line {
        match folds
            .iter()
            .position(|f| sql[..f.start].matches('\n').count() == line)
        {
            Some(i) => {
                folds.remove(i);
            }
            None => {
                if let Some(r) = cache.regions.iter().find(|r| r.line == line) {
                    folding::fold(folds, r.range.clone());
                }
            }
        }
    }
    if let (Some(command), Some(at)) = (fold_command, caret_byte) {
        let caret_line = sql[..at].matches('\n').count();
        match command {
            FoldCommand::Fold => {
                let inner = cache
                    .regions
                    .iter()
                    .filter(|r| r.range.start <= at && at <= r.range.end)
                    .min_by_key(|r| r.range.len());
                if let Some(r) = inner {
                    folding::fold(folds, r.range.clone());
                }
            }
            FoldCommand::Unfold => folds.retain(|f| {
                !(sql[..f.start].matches('\n').count() == caret_line
                    || f.start == at
                    || f.end == at)
            }),
            FoldCommand::FoldAll => {
                let mut all: Vec<_> = cache.regions.iter().map(|r| r.range.clone()).collect();
                all.sort_by_key(|r| (r.start, std::cmp::Reverse(r.end)));
                folds.clear();
                for r in all {
                    if folds.last().is_none_or(|previous| previous.end <= r.start) {
                        folds.push(r);
                    }
                }
            }
            FoldCommand::UnfoldAll => folds.clear(),
        }
    }
    tab.search.dirty |= changed;
    tab.editor_history
        .sync(sql, cursor.clone().unwrap_or(0..0), time, changed);
    changed
}

#[derive(Clone, Copy)]
enum FoldCommand {
    Fold,
    Unfold,
    FoldAll,
    UnfoldAll,
}

/// ⌘- / ⌘= fold and unfold at the caret, with ⇧ for all regions, as in the IDE.
fn fold_shortcut(ui: &mut Ui, editor: egui::Id) -> Option<FoldCommand> {
    if !ui.ctx().memory(|m| m.has_focus(editor)) {
        return None;
    }
    let shift = Modifiers::COMMAND.plus(Modifiers::SHIFT);
    ui.input_mut(|i| {
        if i.consume_key(shift, Key::Minus) {
            Some(FoldCommand::FoldAll)
        } else if i.consume_key(shift, Key::Equals) || i.consume_key(shift, Key::Plus) {
            Some(FoldCommand::UnfoldAll)
        } else if i.consume_key(Modifiers::COMMAND, Key::Minus) {
            Some(FoldCommand::Fold)
        } else if i.consume_key(Modifiers::COMMAND, Key::Equals)
            || i.consume_key(Modifiers::COMMAND, Key::Plus)
        {
            Some(FoldCommand::Unfold)
        } else {
            None
        }
    })
}

/// Grey summary boxes over fold placeholders; clicking one unfolds it.
fn paint_fold_boxes(
    ui: &Ui,
    out: &egui::text_edit::TextEditOutput,
    placeholders: &[(usize, usize)],
    summaries: &[cache::FoldSummary],
    real: &str,
    folds: &[std::ops::Range<usize>],
    row_offsets: &[usize],
) -> Option<usize> {
    let mut clicked = None;
    let first_row = out
        .galley
        .rows
        .partition_point(|row| row.rect().bottom() + out.galley_pos.y < ui.clip_rect().top());
    let last_row = out
        .galley
        .rows
        .partition_point(|row| row.rect().top() + out.galley_pos.y <= ui.clip_rect().bottom());
    let first_char = row_offsets.get(first_row).copied().unwrap_or(usize::MAX);
    let last_char = row_offsets.get(last_row).copied().unwrap_or(usize::MAX);
    let first = placeholders.partition_point(|&(index, _)| index < first_char);
    for &(k, i) in placeholders[first..]
        .iter()
        .take_while(|&&(index, _)| index <= last_char)
    {
        let (Some(summary), Some(range)) = (summaries.get(i), folds.get(i)) else {
            continue;
        };
        let at = cache::cursor_rect(&out.galley, row_offsets, CCursor::new(k))
            .translate(out.galley_pos.to_vec2());
        let rect = egui::Rect::from_min_size(
            egui::pos2(at.left() + 1.0, at.top() + 1.0),
            egui::vec2(summary.width - 2.0, at.height() - 2.0),
        );
        if !ui.clip_rect().intersects(rect) {
            continue;
        }
        let text = &summary.text;
        let resp = ui
            .interact(
                rect,
                egui::Id::new(("fold", out.response.id, i)),
                egui::Sense::click(),
            )
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, text));
        let resp = resp.on_hover_ui(|ui| {
            let preview = summary.preview.get_or_init(|| {
                let text = &real[range.clone()];
                let end = text
                    .char_indices()
                    .nth(4096)
                    .map_or(text.len(), |(byte, _)| byte);
                let mut preview = text[..end].lines().take(20).collect::<Vec<_>>().join("\n");
                if end < text.len() {
                    preview.push_str("\n…");
                }
                preview
            });
            ui.label(egui::RichText::new(preview).monospace());
        });
        let fill = if resp.hovered() {
            theme::BG_HOVER
        } else {
            Color32::from_rgb(0x33, 0x35, 0x3A)
        };
        ui.painter().rect_filled(rect, 3.0, fill);
        ui.painter().text(
            egui::pos2(rect.left() + 7.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            text,
            theme::mono(12.5),
            theme::TEXT_MUTED,
        );
        if resp.clicked() {
            clicked = Some(i);
        }
    }
    clicked
}

/// Outline color of the statement that ⌘↵ would run.
const STATEMENT_OUTLINE: Color32 = Color32::from_rgb(0x4E, 0x8F, 0x55);

/// Green outline around the statement under the caret:
/// it shows which statement ⌘↵ runs. Not drawn while text is selected,
/// because then the selection runs.
fn paint_current_statement(
    ui: &Ui,
    out: &egui::text_edit::TextEditOutput,
    projection: &cache::Projection,
    gutter: egui::Rect,
    row_offsets: &[usize],
) {
    let Some(range) = out
        .cursor_range
        .filter(|r| r.primary.index == r.secondary.index)
    else {
        return;
    };
    let Some(statement) = projection.statement(range.primary.index.0) else {
        return;
    };
    let origin = out.galley_pos.to_vec2();
    let at =
        |index| cache::cursor_rect(&out.galley, row_offsets, CCursor::new(index)).translate(origin);
    let (start, end) = (at(statement.start), at(statement.end));
    let left = gutter.right() - 2.0;
    let right = ui.clip_rect().right() - 4.0;
    let points = statement_outline(start, end, left, right);
    ui.painter().add(egui::Shape::closed_line(
        points,
        egui::Stroke::new(1.0, STATEMENT_OUTLINE),
    ));
}

/// Outline polygon from the first character of a statement to the last one:
/// the first line from its start column, middle lines in full, the last line
/// up to its end column. `start` and `end` are the caret rects at both ends.
fn statement_outline(start: egui::Rect, end: egui::Rect, left: f32, right: f32) -> Vec<egui::Pos2> {
    use egui::pos2;
    let (sx, ex) = (start.left() - 1.0, end.left() + 1.0);
    if (start.top() - end.top()).abs() < 1.0 {
        return vec![
            pos2(sx, start.top()),
            pos2(ex, start.top()),
            pos2(ex, end.bottom()),
            pos2(sx, end.bottom()),
        ];
    }
    vec![
        pos2(sx, start.top()),
        pos2(right, start.top()),
        pos2(right, end.top()),
        pos2(ex, end.top()),
        pos2(ex, end.bottom()),
        pos2(left, end.bottom()),
        pos2(left, start.bottom()),
        pos2(sx, start.bottom()),
    ]
}

/// Line numbers (of the real text, so folded lines are skipped), fold markers
/// and the current line highlight, as in the IDE editor. Returns the real line
/// whose fold marker was clicked.
fn paint_gutter(
    ui: &Ui,
    out: &egui::text_edit::TextEditOutput,
    gutter: egui::Rect,
    current_line_bg: egui::layers::ShapeIdx,
    projection: &cache::Projection,
    markers: &std::collections::HashMap<usize, bool>,
    row_offsets: &[usize],
) -> Option<usize> {
    let painter = ui.painter();
    let origin = out.galley_pos;
    let caret_y = out.cursor_range.map(|r| {
        cache::cursor_rect(&out.galley, row_offsets, r.primary)
            .center()
            .y
            + origin.y
    });
    let clip = ui.clip_rect();
    let mut clicked = None;
    let first = out
        .galley
        .rows
        .partition_point(|row| row.rect().bottom() + origin.y < clip.top());
    for (index, row) in out.galley.rows.iter().enumerate().skip(first) {
        if row.rect().top() + origin.y > clip.bottom() {
            break;
        }
        let char_index = row_offsets.get(index).copied().unwrap_or(0);
        let starts_line = index == 0 || out.galley.rows[index - 1].ends_with_newline;
        let rect = row.rect().translate(origin.to_vec2());
        let current = caret_y.is_some_and(|y| rect.top() <= y && y < rect.bottom());
        if current {
            let full = egui::Rect::from_x_y_ranges(gutter.left()..=clip.right(), rect.y_range());
            painter.set(
                current_line_bg,
                egui::Shape::rect_filled(full, 0.0, theme::BG_CURRENT_LINE),
            );
        }
        let line = projection.real_line(char_index);
        if starts_line {
            let color = if current {
                theme::TEXT_MUTED
            } else {
                theme::LINE_NUMBER
            };
            painter.text(
                egui::pos2(gutter.right() - 26.0, rect.center().y),
                egui::Align2::RIGHT_CENTER,
                (line + 1).to_string(),
                theme::mono(12.0),
                color,
            );
            if let Some(&folded) = markers.get(&line) {
                let at = egui::pos2(gutter.right() - 13.0, rect.center().y);
                let hit = egui::Rect::from_center_size(at, egui::vec2(16.0, rect.height()));
                let resp = ui
                    .interact(
                        hit,
                        egui::Id::new(("fold_marker", out.response.id, line)),
                        egui::Sense::click(),
                    )
                    .on_hover_text(if folded { "Expand" } else { "Collapse" });
                resp.widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::WidgetType::Button,
                        true,
                        format!(
                            "{} line {}",
                            if folded { "Expand" } else { "Collapse" },
                            line + 1
                        ),
                    )
                });
                let color = if resp.hovered() {
                    theme::TEXT
                } else {
                    theme::TEXT_MUTED
                };
                paint_chevron(painter, at, folded, color);
                if resp.clicked() {
                    clicked = Some(line);
                }
            }
        }
    }
    clicked
}

/// ˅ for an open region, › for a folded one.
fn paint_chevron(painter: &egui::Painter, c: egui::Pos2, folded: bool, color: Color32) {
    let stroke = egui::Stroke::new(1.3, color);
    let d = 3.5;
    let points = if folded {
        vec![
            c + egui::vec2(-d / 2.0, -d),
            c + egui::vec2(d / 2.0, 0.0),
            c + egui::vec2(-d / 2.0, d),
        ]
    } else {
        vec![
            c + egui::vec2(-d, -d / 2.0),
            c + egui::vec2(0.0, d / 2.0),
            c + egui::vec2(d, -d / 2.0),
        ]
    };
    painter.add(egui::Shape::line(points, stroke));
}

fn completion_row(ui: &mut Ui, item: &completion::Item, selected: bool) -> egui::Response {
    let (badge, color) = match item.kind {
        ItemKind::Keyword => ("k", theme::SQL_KEYWORD),
        ItemKind::Schema => ("s", theme::TEXT_MUTED),
        ItemKind::Table => ("t", Color32::from_rgb(0x54, 0x8A, 0xF7)),
        ItemKind::View => ("v", Color32::from_rgb(0xB1, 0x89, 0xF5)),
        ItemKind::Column => ("c", theme::SQL_NUMBER),
    };
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 20.0), egui::Sense::click());
    resp.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::SelectableLabel,
            true,
            selected,
            &item.label,
        )
    });
    let p = ui.painter_at(rect);
    if selected {
        p.rect_filled(rect, 3.0, theme::BG_SELECTED);
    } else if resp.hovered() {
        p.rect_filled(rect, 3.0, theme::BG_HOVER);
    }
    let cy = rect.center().y;
    p.text(
        egui::pos2(rect.left() + 10.0, cy),
        egui::Align2::CENTER_CENTER,
        badge,
        theme::mono(11.0),
        color,
    );
    p.text(
        egui::pos2(rect.left() + 22.0, cy),
        egui::Align2::LEFT_CENTER,
        &item.label,
        theme::mono(13.0),
        theme::TEXT,
    );
    p.text(
        egui::pos2(rect.right() - 6.0, cy),
        egui::Align2::RIGHT_CENTER,
        &item.detail,
        egui::FontId::proportional(11.0),
        theme::TEXT_MUTED,
    );
    resp
}

/// Keys of the recent filters list; run before the field so arrows and Enter
/// reach the list. Arrow down in a field with no open popup opens the list.
fn history_keys(
    ui: &mut Ui,
    fc: &FieldCompletion<'_>,
    history: &[String],
    popup: &mut Option<HistoryPopup>,
    completion: &Option<CompletionPopup>,
) -> Option<String> {
    let focused = ui.ctx().memory(|m| m.has_focus(fc.id()));
    let completing = completion.as_ref().is_some_and(|p| p.field == fc.field);
    let open = popup.as_ref().is_some_and(|p| p.field == fc.field);
    if !focused || history.is_empty() || (completing && !open) {
        return None;
    }
    if !open {
        if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowDown)) {
            *popup = Some(HistoryPopup {
                field: fc.field,
                selected: 0,
            });
        }
        return None;
    }
    let p = popup.as_mut().unwrap();
    let mut picked = None;
    let mut close = false;
    ui.input_mut(|i| {
        if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
            p.selected = (p.selected + 1).min(history.len() - 1);
        }
        if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
            p.selected = p.selected.saturating_sub(1);
        }
        if i.consume_key(Modifiers::NONE, Key::Enter) {
            picked = history.get(p.selected).cloned();
        }
        close = i.consume_key(Modifiers::NONE, Key::Escape);
    });
    if close || picked.is_some() {
        *popup = None;
    }
    picked
}

/// Recent filters under a table editor field: shown when an empty field gets
/// focus, on arrow down or with the clock button; typing hides it.
fn history_list(
    ui: &Ui,
    fc: &FieldCompletion<'_>,
    out: &egui::text_edit::TextEditOutput,
    history: &[String],
    popup: &mut Option<HistoryPopup>,
    toggle: bool,
) -> Option<String> {
    let ctx = ui.ctx().clone();
    let field = fc.field;
    let mine = |p: &Option<HistoryPopup>| p.as_ref().is_some_and(|p| p.field == field);
    let area_id = egui::Id::new(("filter_history", fc.tab_id, field));
    if toggle {
        if mine(popup) {
            *popup = None;
        } else {
            *popup = Some(HistoryPopup { field, selected: 0 });
            ctx.memory_mut(|m| m.request_focus(fc.id()));
        }
    } else if out.response.gained_focus() && out.galley.text().is_empty() && !history.is_empty() {
        *popup = Some(HistoryPopup { field, selected: 0 });
    }
    if out.response.changed() && mine(popup) {
        *popup = None;
    }
    let over = ctx
        .memory(|m| m.area_rect(area_id))
        .zip(ctx.pointer_hover_pos())
        .is_some_and(|(r, p)| r.contains(p));
    if mine(popup)
        && !out.response.has_focus()
        && !over
        && !toggle
        && !ctx.memory(|m| m.has_focus(fc.id()))
    {
        *popup = None;
    }
    let p = popup.as_ref().filter(|p| p.field == field)?;

    let rect = out.response.rect;
    let mut picked = None;
    egui::Area::new(area_id)
        .order(egui::Order::Foreground)
        .fixed_pos(rect.left_bottom() + egui::vec2(0.0, 2.0))
        .show(&ctx, |ui| {
            egui::Frame::popup(ui.style())
                .inner_margin(4)
                .show(ui, |ui| {
                    ui.set_width(rect.width().max(260.0));
                    ui.label(
                        RichText::new("Recent filters")
                            .small()
                            .color(theme::TEXT_MUTED),
                    );
                    egui::ScrollArea::vertical()
                        .max_height(220.0)
                        .show(ui, |ui| {
                            for (i, entry) in history.iter().enumerate() {
                                let selected = i == p.selected;
                                let (row, resp) = ui.allocate_exact_size(
                                    egui::vec2(ui.available_width(), 22.0),
                                    egui::Sense::click(),
                                );
                                resp.widget_info(|| {
                                    egui::WidgetInfo::selected(
                                        egui::WidgetType::SelectableLabel,
                                        true,
                                        selected,
                                        entry,
                                    )
                                });
                                let painter = ui.painter_at(row);
                                if selected {
                                    painter.rect_filled(row, 3.0, theme::BG_SELECTED);
                                } else if resp.hovered() {
                                    painter.rect_filled(row, 3.0, theme::BG_HOVER);
                                }
                                painter.text(
                                    egui::pos2(row.left() + 8.0, row.center().y),
                                    egui::Align2::LEFT_CENTER,
                                    entry,
                                    theme::mono(12.5),
                                    theme::TEXT,
                                );
                                if selected {
                                    resp.scroll_to_me(None);
                                }
                                if resp.clicked() {
                                    picked = Some(entry.clone());
                                }
                            }
                        });
                });
        });
    if picked.is_some() {
        *popup = None;
    }
    picked
}

/// WHERE / ORDER BY filters and the editable grid of a table tab.
#[allow(clippy::too_many_arguments)]
fn table_view(
    ui: &mut Ui,
    tab: &mut Tab,
    meta: &[TableMeta],
    default_path: &[String],
    filter_history: &[Vec<String>; 2],
    driver: DriverKind,
    page_size: usize,
    tiles: &mut super::map::TileCache,
) -> (Option<bool>, Option<results::GridAction>) {
    let mut run = None;
    let mut action = None;
    let running = tab.running;
    let tab_id = tab.id;
    let search_path = tab
        .resolved_context
        .as_ref()
        .map(|context| context.schemas.as_slice())
        .unwrap_or_else(|| match &tab.schema {
            SchemaSelection::Named(name) => std::slice::from_ref(name),
            SchemaSelection::Default => default_path,
        });
    if let TabKind::Table {
        schema,
        table,
        filter,
        order,
        ..
    } = &mut tab.kind
    {
        let from = crate::sqlgen::qualified(driver, schema.as_deref(), table);
        let completion = &mut tab.completion;
        let history_popup = &mut tab.filter_history;
        let mut submit = false;
        ui.horizontal(|ui| {
            let fields = [
                (
                    Field::Where,
                    "WHERE",
                    filter,
                    &filter_history[0],
                    Some(ui.available_width() * 0.5 - 30.0),
                ),
                (
                    Field::OrderBy,
                    "ORDER BY",
                    order,
                    &filter_history[1],
                    // ORDER BY takes the rest of the row, leaving room for its history button.
                    None,
                ),
            ];
            for (field, keyword, text, history, width) in fields {
                let fc = FieldCompletion {
                    tab_id,
                    field,
                    driver,
                    meta,
                    search_path,
                    context: format!("SELECT * FROM {from} {keyword} "),
                };
                ui.label(RichText::new(keyword).color(theme::SQL_KEYWORD).monospace());
                let picked = history_keys(ui, &fc, history, history_popup, completion);
                // Enter picks a completion while the popup is open, otherwise it runs the query.
                let popup_open = completion.as_ref().is_some_and(|p| p.field == field)
                    || history_popup.as_ref().is_some_and(|p| p.field == field);
                let (accept, force) = fc.handle_keys(ui, completion);
                let out = egui::TextEdit::singleline(text)
                    .id(fc.id())
                    .font(egui::TextStyle::Monospace)
                    .hint_text(if history.is_empty() {
                        ""
                    } else {
                        "↓ recent filters"
                    })
                    .desired_width(width.unwrap_or(ui.available_width() - 34.0).max(80.0))
                    .show(ui);
                fc.update(ui, text, &out, completion, accept, force);
                let toggle = theme::tool_button_enabled(
                    ui,
                    !history.is_empty(),
                    "🕘",
                    &format!("Recent {keyword} filters"),
                )
                .clicked();
                let picked = picked.or(history_list(ui, &fc, &out, history, history_popup, toggle));
                if let Some(entry) = picked {
                    *text = entry;
                    set_caret(ui.ctx(), fc.id(), text.chars().count());
                    // Choosing a recent filter applies it right away.
                    submit = true;
                }
                submit |= !popup_open
                    && out.response.lost_focus()
                    && ui.input(|i| i.key_pressed(Key::Enter));
            }
        });
        if submit {
            run = Some(false);
        }
    }
    ui.separator();
    if let Some(rs) = tab.results.first_mut() {
        action = results::grid_toolbar(ui, rs, driver, page_size, true);
        results::value_panel(ui, egui::Id::new(("table_value", tab.id)), rs);
        if let Some(err) = tab.log.last().filter(|l| l.kind == results::LogKind::Error) {
            ui.label(RichText::new(&err.text).color(theme::RED).monospace());
        }
        results::grid(ui, rs, tiles);
        let transposed = rs.transposed;
        tab.transposed = transposed;
        tab.record_view = rs.record_view;
        tab.aggregate_view = rs.aggregate_view;
        tab.show_value_editor = rs.show_value_editor;
    } else if running == Some(RunKind::Query) {
        ui.spinner();
    } else if let Some(err) = tab
        .log
        .iter()
        .rev()
        .find(|l| l.kind == results::LogKind::Error)
    {
        ui.label(RichText::new(&err.text).color(theme::RED).monospace());
    }
    (run, action)
}

#[cfg(test)]
mod tests {
    use super::{source_change_blocked_reason, statement_outline};
    use crate::ui::{RunKind, TabKind, actions::ActionId, actions::tests::isolated_app};
    use egui::{Rect, pos2};

    #[test]
    fn pinned_text_buffer_rejects_every_native_mutation_without_losing_copy() {
        use egui::TextBuffer;
        use egui::text::CharIndex;
        let mut sql = String::from("SELECT Łódź WHERE id = ?");
        let mut buffer = super::ConsoleBuffer {
            text: &mut sql,
            read_only: true,
        };
        assert!(!buffer.is_mutable());
        assert_eq!(buffer.insert_text("bad", CharIndex(0)), 0);
        buffer.delete_char_range(CharIndex(0)..CharIndex(6));
        buffer.replace_with("bad");
        buffer.clear();
        assert_eq!(buffer.take(), "SELECT Łódź WHERE id = ?");
        assert_eq!(buffer.as_str(), "SELECT Łódź WHERE id = ?");
    }

    struct ToolbarHarness {
        tab: crate::ui::Tab,
        source: crate::model::DataSource,
        sources: Vec<(String, String)>,
        databases: Vec<crate::model::DbObject>,
        bounds: Rect,
        formatted: bool,
    }

    #[test]
    fn transaction_toolbar_keeps_context_and_actions_separate_at_narrow_widths() {
        use egui::accesskit::Role;
        use egui_kittest::kittest::{NodeT, Queryable};
        let mut app = isolated_app(&egui::Context::default());
        let index = app.add_tab(
            "Console".into(),
            "source".into(),
            TabKind::Console { sql: String::new() },
        );
        let mut tab = app.tabs.remove(index);
        tab.in_tx = true;
        tab.schema = crate::model::SchemaSelection::Named("a_very_long_schema_name_".repeat(8));
        let mut source = crate::model::DataSource::new(crate::model::DriverKind::Postgres);
        source.id = "source".into();
        source.name = "A very long production database source name ".repeat(12);
        source.database = "a_very_long_database_name_".repeat(8);
        let databases = [&source.database, "another_database"]
            .into_iter()
            .map(|name| crate::model::DbObject::new(crate::model::ObjectKind::Database, name, &[]))
            .collect();
        let sources = vec![(source.id.clone(), source.name.clone())];
        let mut h = egui_kittest::Harness::builder()
            .with_size([1440.0, 720.0])
            .build_ui_state(
                |ui, state: &mut ToolbarHarness| {
                    ui.horizontal(|ui| {
                        // Match the Explorer and tool stripe beside the editor.
                        ui.allocate_space(egui::vec2(350.0, 56.0));
                        ui.vertical(|ui| {
                            state.bounds = ui.available_rect_before_wrap();
                            let databases = super::DatabaseChoice {
                                default: &state.source.database,
                                shown: &state.databases,
                                source: &state.source,
                            };
                            let catalog = super::context::CatalogView {
                                metadata: None,
                                loading: false,
                                error: None,
                            };
                            let reason = super::source_change_blocked_reason(&state.tab);
                            let out = super::toolbar(
                                ui,
                                &mut state.tab,
                                &state.sources,
                                &[],
                                Some(&databases),
                                crate::model::DriverKind::Postgres,
                                &catalog,
                                reason,
                                super::ExplainReasons {
                                    plan: None,
                                    analyse: None,
                                    driver: crate::model::DriverKind::Postgres,
                                },
                            );
                            state.formatted |= out.format;
                        });
                    });
                },
                ToolbarHarness {
                    tab,
                    source,
                    sources,
                    databases,
                    bounds: Rect::NOTHING,
                    formatted: false,
                },
            );
        crate::ui::theme::apply(&h.ctx);
        for width in [1440.0, 1100.0] {
            h.set_size(egui::vec2(width, 720.0));
            h.run_steps(4);
            for label in [
                "▶",
                "■",
                "✔ Commit",
                "Rollback",
                "Default schema",
                "Data source",
                "Database",
            ] {
                assert!(
                    h.query_by_label(label).is_some(),
                    "primary control missing: {label} at {width}"
                );
            }
            let controls: Vec<_> = h
                .query_all_by_role(Role::Button)
                .chain(h.query_all_by_role(Role::ComboBox))
                .map(|node| {
                    (
                        node.accesskit_node().label().unwrap_or_default(),
                        node.rect(),
                    )
                })
                .collect();
            for (index, (label, rect)) in controls.iter().enumerate() {
                assert!(
                    h.state().bounds.expand(1.0).contains_rect(*rect),
                    "{label} must fit its allocated toolbar at {width}: {rect:?}, {:?}",
                    h.state().bounds
                );
                for (other_label, other) in &controls[index + 1..] {
                    let overlap = rect.intersect(*other);
                    assert!(
                        overlap.width() <= 1.0 || overlap.height() <= 1.0,
                        "{label} overlaps {other_label} at {width}: {rect:?}, {other:?}"
                    );
                }
            }
        }
        h.get_by_label("More Console Actions").click();
        h.run_steps(3);
        h.get_by_label("Format").click();
        h.run_steps(3);
        assert!(h.state().formatted, "overflow actions remain usable");
        h.state_mut().tab.in_tx = false;
        h.state_mut().databases.clear();
        h.run_steps(3);
        h.set_size(egui::vec2(1440.0, 720.0));
        h.run_steps(3);
        assert!(
            h.query_by_label("Format").is_some(),
            "secondary actions use spare space before widening context selectors"
        );
        assert!(h.query_by_label("Explain Plan").is_some());
    }

    #[test]
    fn source_reassignment_waits_for_session_file_and_grid_ownership() {
        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx);
        app.add_tab(
            "test".into(),
            "original".into(),
            TabKind::Console { sql: String::new() },
        );
        let tab = &mut app.tabs[0];
        tab.database = Some("original_database".into());
        assert_eq!(source_change_blocked_reason(tab), None);
        tab.context_pending = Some(crate::model::ExecutionContext::default());
        assert!(
            source_change_blocked_reason(tab)
                .unwrap()
                .contains("schema change")
        );
        assert!(
            !tab.is_running(),
            "a context request is not a cancellable SQL query"
        );
        tab.context_pending = None;
        tab.running = Some(RunKind::Query);
        assert!(
            source_change_blocked_reason(tab)
                .unwrap()
                .contains("running request")
        );
        tab.running = None;
        for kind in 0..4 {
            tab.saving_file = kind == 0;
            tab.file_save_queued = kind == 1;
            tab.unsaved_writes = kind == 2;
            tab.in_tx = kind == 3;
            assert!(source_change_blocked_reason(tab).is_some(), "state {kind}");
        }
        tab.in_tx = false;
        let mut result = crate::ui::ResultSet::new(
            "Result".into(),
            "select 1".into(),
            crate::model::QueryResult::default(),
            crate::model::Page::first(500),
        );
        result.edits.deleted.insert(0);
        tab.results.push(result);
        assert!(
            source_change_blocked_reason(tab)
                .unwrap()
                .contains("grid changes")
        );
        tab.results[0].edits.deleted.clear();
        assert_eq!(source_change_blocked_reason(tab), None);
        // Starting a new row creates the inline editing buffer. Even before a
        // staged value remains, that buffer still belongs to the original source.
        let editable = tab.results.remove(0).with_test_table();
        tab.results.insert(0, editable);
        tab.results[0].apply_local_action(ActionId::AddRow, &ctx);
        tab.results[0].edits.inserted.clear();
        assert!(tab.results[0].edits.is_empty());
        assert!(
            source_change_blocked_reason(tab)
                .unwrap()
                .contains("grid changes")
        );
        assert_eq!(tab.ds_id, "original");
        assert_eq!(tab.database.as_deref(), Some("original_database"));
    }

    fn caret(x: f32, line: usize) -> Rect {
        let top = line as f32 * 20.0;
        Rect::from_min_max(pos2(x, top), pos2(x + 2.0, top + 20.0))
    }

    #[test]
    fn single_line_statement_is_a_box_around_its_text() {
        let pts = statement_outline(caret(40.0, 2), caret(140.0, 2), 0.0, 500.0);
        assert_eq!(
            pts,
            [
                pos2(39.0, 40.0),
                pos2(141.0, 40.0),
                pos2(141.0, 60.0),
                pos2(39.0, 60.0)
            ]
        );
    }

    #[test]
    fn multi_line_statement_follows_first_and_last_columns() {
        // Starts at x=60 on line 1, ends at x=200 on line 4.
        let pts = statement_outline(caret(60.0, 1), caret(200.0, 4), 10.0, 500.0);
        assert_eq!(
            pts,
            [
                pos2(59.0, 20.0),
                pos2(500.0, 20.0),
                pos2(500.0, 80.0),
                pos2(201.0, 80.0),
                pos2(201.0, 100.0),
                pos2(10.0, 100.0),
                pos2(10.0, 40.0),
                pos2(59.0, 40.0),
            ]
        );
    }
}
