//! Database Explorer tool window: cached navigation over the lazily loaded tree.

pub(super) mod groups;
pub(super) mod navigation;

use super::dialog::DataSourceDialog;
use super::mcp::{ShareKey, ShareLevel, ShareTarget};
use super::{App, ConnStatus, ServicesNotice, TabKind, TreeAction, driver_badge, theme};
use crate::model::{DataSource, DbObject, DriverKind, ObjectKind};
use egui::{Color32, Id, Key, KeyboardShortcut, Modifiers, RichText, Sense, Ui};
use groups::{GroupDialog, GroupDialogMode, SourceDrop};
use navigation::{
    GROUP_KEY, HierarchyCommand, RevealState, RevealStep, RowKind, SelectionMove, SourceSlot, SourceTree,
    SpeedSearch, TreeIntent, VisibleIndex,
};
use std::collections::{HashMap, HashSet};
use std::ops::Range;

const ROW_HEIGHT: f32 = 22.0;

pub(super) fn tree_focus_id() -> Id {
    Id::new("database_explorer_tree")
}

pub(super) fn action_has_focus(ctx: &egui::Context) -> bool {
    ctx.memory(|memory| {
        memory.has_focus(tree_focus_id())
            || memory.has_focus(super::tool_windows::explorer_filter_id())
    })
}

/// One cached projection of the fetched, expanded tree. Invalidation is event driven.
pub(super) struct ExplorerState {
    index: VisibleIndex,
    search: SpeedSearch,
    dirty: bool,
    filtering: bool,
    focus_search: bool,
    select_query: bool,
    focus_tree: bool,
    reveal_focus_pending: bool,
    reveal_selection: bool,
    scroll_offset: f32,
    viewport_height: f32,
    badges: HashMap<usize, String>,
    status: HashMap<SourceSlot, (String, Color32)>,
    empty_loading: HashSet<Vec<String>>,
    reveal: RevealState,
    reveal_dirty: bool,
    /// Data sources per group path (counting subgroups), for the group
    /// badges; built once per index generation instead of per row and frame.
    group_counts: std::cell::RefCell<(Option<u64>, HashMap<String, usize>)>,
}

impl Default for ExplorerState {
    fn default() -> Self {
        Self {
            index: Default::default(),
            search: Default::default(),
            dirty: true,
            // Retain the existing Filter field's narrowing behavior.
            filtering: true,
            focus_search: false,
            select_query: false,
            focus_tree: false,
            reveal_focus_pending: false,
            reveal_selection: false,
            scroll_offset: 0.0,
            viewport_height: 0.0,
            badges: Default::default(),
            status: Default::default(),
            empty_loading: Default::default(),
            reveal: Default::default(),
            reveal_dirty: false,
            group_counts: Default::default(),
        }
    }
}

impl ExplorerState {
    pub(super) fn invalidate(&mut self) {
        self.dirty = true;
        self.reveal_dirty = true;
    }

    pub(super) fn cancel_reveal(&mut self) {
        self.reveal.cancel();
        self.reveal_dirty = false;
        if std::mem::take(&mut self.reveal_focus_pending) {
            self.focus_tree = false;
        }
    }

    /// A failed load must stop an explicit reveal, rather than retry every event.
    pub(super) fn cancel_reveal_for_source(&mut self, source_id: &str) {
        if self
            .reveal
            .pending()
            .is_some_and(|request| request.source_id == source_id)
        {
            self.cancel_reveal();
        }
    }

    pub(super) fn request_focus(&mut self) {
        self.reveal_focus_pending = false;
        self.focus_tree = true;
    }

    fn request_reveal_focus(&mut self) {
        self.focus_tree = true;
        self.reveal_focus_pending = true;
    }
}

impl App {
    pub(super) fn focus_explorer_search(&mut self, ctx: &egui::Context) {
        self.pending_editor_focus = false;
        self.pending_action_focus = None;
        self.show_explorer = true;
        self.show_files = false;
        self.explorer_ui.cancel_reveal();
        self.explorer_ui.focus_tree = false;
        self.explorer_ui.focus_search = true;
        self.explorer_ui.select_query = true;
        ctx.request_repaint();
    }

    fn explorer_source(&self, slot: SourceSlot) -> Option<&DataSource> {
        match slot {
            SourceSlot::Config(index) => self.config.data_sources.get(index),
            SourceSlot::File(index) => self.files.get(index).map(|file| &file.source),
        }
    }

    fn refresh_explorer_index(&self, state: &mut ExplorerState) {
        if !std::mem::take(&mut state.dirty) {
            return;
        }
        let groups = self.explorer_groups();
        let memberships: Vec<Vec<String>> = self
            .config
            .data_sources
            .iter()
            .map(|source| groups::segments(self.source_group_of(&source.id)))
            .collect();
        let sources = self
            .config
            .data_sources
            .iter()
            .enumerate()
            .map(|(index, source)| (SourceSlot::Config(index), source))
            .chain(
                self.files
                    .iter()
                    .enumerate()
                    .map(|(index, file)| (SourceSlot::File(index), &file.source)),
            )
            .map(|(slot, source)| {
                let current = self.sources.get(&source.id);
                SourceTree {
                    source,
                    group: match slot {
                        SourceSlot::Config(index) => memberships[index].as_slice(),
                        SourceSlot::File(_) => &[],
                    },
                    slot,
                    roots: current.and_then(|current| current.roots.as_deref()),
                    loading: current.map_or(&state.empty_loading, |current| &current.loading),
                    connecting: current
                        .is_some_and(|current| matches!(current.status, ConnStatus::Connecting)),
                }
            });
        state.index.rebuild(
            sources,
            &groups,
            &self.expanded,
            self.selected
                .as_ref()
                .map(|(id, path)| (id.as_str(), path.as_slice())),
        );
        state.badges.clear();
        state.status.clear();
        // Counters can inspect many schema names, so cache them with the index.
        for (row, current) in state.index.rows().iter().enumerate() {
            let Some(slot) = state.index.source_slot(row) else {
                continue;
            };
            let Some(source) = self.explorer_source(slot) else {
                continue;
            };
            let source_state = self.sources.get(&source.id);
            let roots = source_state.and_then(|current| current.roots.as_deref());
            match current.kind {
                RowKind::Source => {
                    let status = match source_state.map(|current| &current.status) {
                        Some(ConnStatus::Connected(info)) => (
                            info.split(" (").next().unwrap_or(info).to_string(),
                            theme::GREEN,
                        ),
                        Some(ConnStatus::Connecting) => ("connecting…".into(), theme::TEXT_MUTED),
                        Some(ConnStatus::Failed(error)) => (first_line(error), theme::RED),
                        _ => (String::new(), theme::TEXT_MUTED),
                    };
                    state.status.insert(slot, status);
                    if has_scope(source)
                        && let Some(roots) = roots
                    {
                        let shown = roots
                            .iter()
                            .filter(|node| root_visible(source, node))
                            .count();
                        state.badges.insert(row, count_badge(shown, roots.len()));
                    }
                }
                RowKind::Object(ObjectKind::Database) if source.driver == DriverKind::Postgres => {
                    if let Some(node) = roots.and_then(|roots| state.index.object(row, roots))
                        && let Some(children) = &node.children
                    {
                        let shown = children
                            .iter()
                            .filter(|child| source.shows_schema(&node.name, &child.name))
                            .count();
                        state.badges.insert(row, count_badge(shown, children.len()));
                    }
                }
                _ => {}
            }
        }
    }

    pub(super) fn explorer(&mut self, ui: &mut Ui) {
        if !self.show_explorer {
            return;
        }
        self.advance_explorer_reveal();
        let mut state = std::mem::take(&mut self.explorer_ui);
        self.refresh_explorer_index(&mut state);
        let mut actions = Vec::new();
        let mut intents = Vec::new();
        let mut collapse_all = false;
        let mut from_editor = false;
        let mut return_editor = false;
        let mut hide = false;
        let mut manual_navigation = false;
        egui::Panel::left("explorer")
            .resizable(true)
            .default_size(320.0)
            .min_size(240.0)
            .frame(
                egui::Frame::new()
                    .fill(theme::BG_PANEL)
                    .inner_margin(egui::Margin::symmetric(6, 0)),
            )
            .show(ui, |ui| {
                theme::tool_window_header(ui, "Database Explorer", |ui| {
                    if theme::tool_button(ui, "−", "Hide").clicked() {
                        self.apply_tool_window_action(
                            super::actions::ActionId::DatabaseExplorer,
                            ui.ctx(),
                        );
                    }
                    if theme::tool_button(
                        ui,
                        RichText::new("QL").size(11.5).strong(),
                        &theme::keys("New Query Console (⌘⇧L)"),
                    )
                    .clicked()
                        && let Some(id) =
                            self.selected_source_id()
                                .map(str::to_owned)
                                .or_else(|| {
                                    self.config
                                        .data_sources
                                        .first()
                                        .map(|source| source.id.clone())
                                })
                    {
                        actions.push(TreeAction::NewConsole(id, None));
                    }
                    let more = theme::tool_menu(ui, "…", "More actions", |ui| {
                        if ui.button("Collapse All").clicked() {
                            collapse_all = true;
                            ui.close();
                        }
                        if ui
                            .add_enabled(
                                !self.tabs.is_empty(),
                                egui::Button::new("Scroll from Editor"),
                            )
                            .clicked()
                        {
                            from_editor = true;
                            ui.close();
                        }
                        ui.checkbox(&mut state.filtering, "Speed Search Filter");
                        ui.separator();
                        if ui.button("Data Source Properties…").clicked() {
                            actions.push(TreeAction::Properties(
                                self.selected_source_id().unwrap_or_default().to_owned(),
                            ));
                            ui.close();
                        }
                        if ui
                            .add_enabled(
                                self.selected_source_id().is_some(),
                                egui::Button::new("Disconnect"),
                            )
                            .clicked()
                            && let Some(id) = self.selected_source_id()
                        {
                            actions.push(TreeAction::Disconnect(id.to_owned()));
                            ui.close();
                        }
                    });
                    more.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "More actions")
                    });
                    if theme::tool_button_enabled(
                        ui,
                        self.selected_source_id().is_some(),
                        "⟳",
                        "Refresh",
                    )
                    .clicked()
                        && let Some(id) = self.selected_source_id()
                    {
                        actions.push(TreeAction::Refresh(id.to_owned()));
                    }
                    let new = theme::tool_menu(ui, RichText::new("+").size(15.0), "New", |ui| {
                        ui.label(
                            RichText::new("Data Source")
                                .small()
                                .color(theme::TEXT_MUTED),
                        );
                        for driver in DriverKind::ALL {
                            if ui
                                .button(format!("{}  {}", driver_badge(driver), driver.label()))
                                .clicked()
                            {
                                let mut dialog =
                                    DataSourceDialog::new(self.config.data_sources.clone(), None);
                                dialog.add(driver);
                                self.dialog = Some(dialog);
                                ui.close();
                            }
                        }
                        ui.separator();
                        if ui.button("Data Source Group…").clicked() {
                            actions.push(TreeAction::GroupDialog(GroupDialogMode::New {
                                parent: self.selected_group().unwrap_or_default(),
                                source: None,
                            }));
                            ui.close();
                        }
                    });
                    new.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "New")
                    });
                });
                let tree_id = tree_focus_id();
                let search_id = super::tool_windows::explorer_filter_id();
                let tree_focus = ui.memory(|memory| {
                    memory.has_focus(tree_id)
                        || (memory.focused().is_none() && memory.had_focus_last_frame(tree_id))
                });
                let search_focus = ui.memory(|memory| {
                    memory.has_focus(search_id)
                        || (memory.focused().is_none() && memory.had_focus_last_frame(search_id))
                });
                let popup = ui.ctx().any_popup_open();
                let mut keys = ExplorerKeys::default();
                if !popup && (tree_focus || search_focus) {
                    keys = explorer_keys(ui, tree_focus);
                    manual_navigation |= keys.any();
                    if keys.find {
                        state.focus_search = true;
                        state.select_query = true;
                    }
                    if keys.escape {
                        if self.filter.is_empty() {
                            return_editor = true;
                        } else {
                            self.filter.clear();
                            state.focus_tree = true;
                        }
                    }
                    hide |= keys.hide;
                    if tree_focus
                        && !ui.input(|input| {
                            input.modifiers.command || input.modifiers.ctrl || input.modifiers.alt
                        })
                    {
                        ui.input_mut(|input| {
                            input.events.retain(|event| {
                                if let egui::Event::Text(text) | egui::Event::Paste(text) = event {
                                    self.filter.push_str(text);
                                    state.focus_search = true;
                                    manual_navigation = true;
                                    false
                                } else {
                                    true
                                }
                            })
                        });
                    }
                }
                let search = ui.add(
                    egui::TextEdit::singleline(&mut self.filter)
                        .id(search_id)
                        .hint_text("🔍 Filter")
                        .desired_width(f32::INFINITY),
                );
                search.widget_info(|| {
                    let mut info =
                        egui::WidgetInfo::text_edit(true, &self.filter, &self.filter, "");
                    info.label = Some("Speed Search".into());
                    info
                });
                if ui.memory(|memory| memory.top_modal_layer().is_none())
                    && std::mem::take(&mut state.focus_search)
                {
                    search.request_focus();
                    let select_all = std::mem::take(&mut state.select_query);
                    if let Some(mut edit) =
                        egui::text_edit::TextEditState::load(ui.ctx(), search_id)
                    {
                        let end = self.filter.chars().count();
                        edit.cursor
                            .set_char_range(Some(egui::text::CCursorRange::two(
                                egui::text::CCursor::new(if select_all { 0 } else { end }),
                                egui::text::CCursor::new(end),
                            )));
                        edit.store(ui.ctx(), search_id);
                    }
                }
                lock_tree_navigation(ui, search_id);
                manual_navigation |= search.changed();
                let query_changed =
                    state
                        .search
                        .update(&state.index, &self.filter, state.filtering, |slot| {
                            self.explorer_source(slot)
                                .and_then(|source| self.sources.get(&source.id))
                                .and_then(|current| current.roots.as_deref())
                        });
                // Always refresh the projection before keyboard lookup; raw row indexes
                // from a previous generation must never be reinterpreted as new rows.
                if query_changed && !self.filter.is_empty() {
                    let selected = state.index.selected();
                    let target = selected
                        .filter(|&row| !state.search.highlights(row).is_empty())
                        .or_else(|| state.search.next_match(selected, false));
                    if let Some(token) = target.and_then(|row| state.index.token(row)) {
                        state.index.select(token);
                        intents.push(TreeIntent::Select(token));
                        state.reveal_selection = true;
                    }
                }
                let rows_per_page = (state.viewport_height / ROW_HEIGHT).floor().max(1.0) as usize;
                let target = if keys.previous || keys.next {
                    if search_focus && !self.filter.is_empty() {
                        state
                            .search
                            .next_match(state.index.selected(), keys.previous)
                    } else {
                        state.search.move_selection(
                            state.index.selected(),
                            if keys.previous {
                                SelectionMove::Previous
                            } else {
                                SelectionMove::Next
                            },
                        )
                    }
                } else if keys.first {
                    state
                        .search
                        .move_selection(state.index.selected(), SelectionMove::First)
                } else if keys.last {
                    state
                        .search
                        .move_selection(state.index.selected(), SelectionMove::Last)
                } else if keys.page_up {
                    state.search.move_selection(
                        state.index.selected(),
                        SelectionMove::PageUp(rows_per_page),
                    )
                } else if keys.page_down {
                    state.search.move_selection(
                        state.index.selected(),
                        SelectionMove::PageDown(rows_per_page),
                    )
                } else {
                    None
                };
                if let Some(token) = target.and_then(|row| state.index.token(row)) {
                    state.index.select(token);
                    intents.push(TreeIntent::Select(token));
                    state.reveal_selection = true;
                }
                let hierarchy = if keys.left {
                    Some(HierarchyCommand::Left)
                } else if keys.right {
                    Some(HierarchyCommand::Right)
                } else if keys.activate {
                    Some(HierarchyCommand::Activate)
                } else if keys.edit_data {
                    Some(HierarchyCommand::EditData)
                } else {
                    None
                };
                if let Some(command) = hierarchy
                    && let Some(row) = state.index.selected()
                    && let Some(intent) = state.search.hierarchy(&state.index, row, command)
                {
                    intents.push(intent);
                    state.reveal_selection = true;
                    state.focus_tree = true;
                }
                ui.add_space(4.0);
                if self.config.data_sources.is_empty() && self.files.is_empty() {
                    ui.add_space(20.0);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new("No data sources").color(theme::TEXT_MUTED));
                        if ui.link("Create a data source…").clicked() {
                            self.dialog = Some(DataSourceDialog::new(Vec::new(), None));
                        }
                    });
                    return;
                }
                if !self.filter.is_empty() && state.search.hits().is_empty() {
                    ui.label(
                        RichText::new("No matches in the expanded tree")
                            .small()
                            .color(theme::TEXT_MUTED),
                    );
                }
                let mut scroll = egui::ScrollArea::both()
                    .id_salt("database_explorer_rows")
                    .auto_shrink([false, false]);
                if std::mem::take(&mut state.reveal_selection)
                    && let Some(position) = state
                        .index
                        .selected()
                        .and_then(|row| state.search.display_position(row))
                {
                    let top = position as f32 * ROW_HEIGHT;
                    let bottom = top + ROW_HEIGHT;
                    let offset = if top < state.scroll_offset {
                        top
                    } else if bottom > state.scroll_offset + state.viewport_height {
                        (bottom - state.viewport_height).max(0.0)
                    } else {
                        state.scroll_offset
                    };
                    scroll = scroll.vertical_scroll_offset(offset);
                }
                // All rows have a fixed pitch, including spinners and the Files heading.
                let spacing = ui.spacing().item_spacing.y;
                ui.spacing_mut().item_spacing.y = 0.0;
                let output =
                    scroll.show_rows(ui, ROW_HEIGHT, state.search.rows().len(), |ui, range| {
                        for position in range {
                            let row = state.search.rows()[position];
                            let current = &state.index.rows()[row];
                            match current.kind {
                                RowKind::FilesHeader => {
                                    ui.allocate_ui(
                                        egui::vec2(ui.available_width(), ROW_HEIGHT),
                                        |ui| {
                                            ui.label(
                                                RichText::new("Opened files")
                                                    .small()
                                                    .color(theme::TEXT_MUTED),
                                            )
                                            .on_hover_text(
                                                "Temporary data sources, gone when closed",
                                            );
                                        },
                                    );
                                }
                                RowKind::Loading => {
                                    ui.allocate_ui(
                                        egui::vec2(ui.available_width(), ROW_HEIGHT),
                                        |ui| {
                                            ui.horizontal(|ui| {
                                                ui.add_space(16.0 * current.depth as f32 + 12.0);
                                                ui.spinner();
                                            });
                                        },
                                    );
                                }
                                RowKind::Group => {
                                    let Some(path) = state.index.group_path(row) else {
                                        continue;
                                    };
                                    let response = ui
                                        .scope_builder(
                                            egui::UiBuilder::new().id(tree_id.with((GROUP_KEY, path))),
                                            |ui| {
                                                self.group_row(
                                                    ui,
                                                    &state,
                                                    row,
                                                    path,
                                                    &mut actions,
                                                    &mut intents,
                                                )
                                            },
                                        )
                                        .inner;
                                    if response.clicked() || response.secondary_clicked() {
                                        state.focus_tree = true;
                                        manual_navigation = true;
                                    }
                                }
                                RowKind::Source | RowKind::Object(_) => {
                                    let Some(slot) = state.index.source_slot(row) else {
                                        continue;
                                    };
                                    let Some(source) = self.explorer_source(slot) else {
                                        continue;
                                    };
                                    let node = self
                                        .sources
                                        .get(&source.id)
                                        .and_then(|source| source.roots.as_deref())
                                        .and_then(|roots| state.index.object(row, roots));
                                    if matches!(current.kind, RowKind::Object(_)) && node.is_none()
                                    {
                                        continue;
                                    }
                                    let path = node.map_or(&[][..], |node| node.path.as_slice());
                                    let id = tree_id.with((&source.id, path));
                                    let response = ui
                                        .scope_builder(egui::UiBuilder::new().id(id), |ui| {
                                            self.indexed_tree_row(
                                                ui,
                                                &state,
                                                row,
                                                source,
                                                node,
                                                &mut actions,
                                                &mut intents,
                                            )
                                        })
                                        .inner;
                                    if response.clicked() || response.secondary_clicked() {
                                        state.focus_tree = true;
                                        manual_navigation = true;
                                    }
                                }
                            }
                        }
                    });
                ui.spacing_mut().item_spacing.y = spacing;
                state.scroll_offset = output.state.offset.y;
                state.viewport_height = output.inner_rect.height();
                let tree = ui.interact(
                    output.inner_rect,
                    tree_id,
                    Sense::focusable_noninteractive(),
                );
                tree.widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::WidgetType::Other,
                        true,
                        "Database Explorer Tree",
                    )
                });
                if !ui.ctx().any_popup_open()
                    && ui.memory(|memory| memory.top_modal_layer().is_none())
                    && std::mem::take(&mut state.focus_tree)
                {
                    tree.request_focus();
                    state.reveal_focus_pending = false;
                }
                lock_tree_navigation(ui, tree_id);
            });
        if manual_navigation {
            state.cancel_reveal();
        }
        self.explorer_ui = state;
        for intent in intents {
            self.apply_explorer_intent(intent);
        }
        for action in actions {
            self.tree_action(action);
        }
        if collapse_all {
            self.collapse_explorer();
        }
        if from_editor {
            self.scroll_from_editor();
        }
        if return_editor {
            self.focus_active_editor(ui.ctx());
        }
        if hide {
            self.apply_tool_window_action(super::actions::ActionId::DatabaseExplorer, ui.ctx());
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn indexed_tree_row(
        &self,
        ui: &mut Ui,
        state: &ExplorerState,
        row: usize,
        source: &DataSource,
        node: Option<&DbObject>,
        actions: &mut Vec<TreeAction>,
        intents: &mut Vec<TreeIntent>,
    ) -> egui::Response {
        let current = &state.index.rows()[row];
        let token = state.index.token(row).expect("indexed row");
        let status = state
            .index
            .source_slot(row)
            .and_then(|slot| state.status.get(&slot));
        let is_pg_database = node.is_some_and(|node| {
            source.driver == DriverKind::Postgres && node.kind == ObjectKind::Database
        });
        let (name, detail, color) = match node {
            Some(node) => (
                node.name.as_str(),
                if is_pg_database {
                    ""
                } else {
                    node.detail.as_str()
                },
                theme::TEXT_MUTED,
            ),
            None => (
                state.index.source_label(row).unwrap_or(""),
                status.map_or("", |(text, _)| text.as_str()),
                status.map_or(theme::TEXT_MUTED, |(_, color)| *color),
            ),
        };
        let driver_icon = match source.driver {
            DriverKind::Postgres => "🐘",
            DriverKind::MySql => "🐬",
            DriverKind::Sqlite => "🗄",
        };
        let response = tree_row_highlighted(
            ui,
            current.depth,
            current.expandable.then_some(current.expanded),
            node.map_or(TreeIcon::Text(driver_icon), |node| {
                TreeIcon::Kind(node.kind)
            }),
            name,
            state.badges.get(&row).map(String::as_str),
            detail,
            color,
            state.index.selected() == Some(row),
            state.search.highlights(row),
            if node.is_none() && !source.id.starts_with("file:") {
                Sense::click_and_drag()
            } else {
                Sense::click()
            },
        );
        if node.is_none() && !source.id.starts_with("file:") {
            source_drag_and_drop(ui, &response, source, actions);
        }
        if response.clicked() || response.secondary_clicked() {
            intents.push(TreeIntent::Select(token));
        }
        if response.clicked() {
            if badge_hit(&response) {
                actions.push(TreeAction::ChooseScope(source.id.clone()));
            } else if arrow_hit(&response) && current.expandable {
                intents.push(if current.expanded {
                    TreeIntent::Collapse(token)
                } else {
                    TreeIntent::Expand(token)
                });
            }
        }
        if response.double_clicked() {
            if node.is_some_and(|node| matches!(node.kind, ObjectKind::Table | ObjectKind::View)) {
                intents.push(TreeIntent::Open(token));
            } else if current.expandable {
                intents.push(if current.expanded {
                    TreeIntent::Collapse(token)
                } else {
                    TreeIntent::Expand(token)
                });
            }
        }
        response.context_menu(|ui| {
            if let Some(node) = node {
                if matches!(node.kind, ObjectKind::Table | ObjectKind::View)
                    && ui.button("Edit Data").clicked()
                {
                    intents.push(TreeIntent::Open(token));
                    ui.close();
                }
                if ui.button("New ▶ Query Console").clicked() {
                    actions.push(TreeAction::NewConsole(
                        source.id.clone(),
                        pg_database(source, node),
                    ));
                    ui.close();
                }
                if ui.button("Copy Name").clicked() {
                    actions.push(TreeAction::CopyName(node.name.clone()));
                    ui.close();
                }
                if matches!(
                    node.kind,
                    ObjectKind::Schema
                        | ObjectKind::Table
                        | ObjectKind::View
                        | ObjectKind::Routine
                        | ObjectKind::Sequence
                ) && ui.button("Generate DDL to Query Console").clicked()
                {
                    actions.push(TreeAction::Ddl(source.id.clone(), node_descriptor(node)));
                    ui.close();
                }
                if matches!(node.kind, ObjectKind::Table | ObjectKind::View)
                    && ui.button("Compare Structure With").clicked()
                {
                    actions.push(TreeAction::CompareStructure(
                        source.id.clone(),
                        node_descriptor(node),
                    ));
                    ui.close();
                }
                if matches!(node.kind, ObjectKind::Database | ObjectKind::Schema)
                    && ui.button("Show Database Graph").clicked()
                {
                    actions.push(TreeAction::DatabaseGraph(
                        source.id.clone(),
                        Some(node_descriptor_with_children(node)),
                    ));
                    ui.close();
                }
                super::export_dialog::transfer_menu(ui, source, Some(node), actions);
                if source.driver == DriverKind::Postgres
                    && node.kind == ObjectKind::Database
                    && ui.button("Activity Monitor").clicked()
                {
                    actions.push(TreeAction::ActivityMonitor(source.id.clone(), Some(node.name.clone())));
                    ui.close();
                }
                self.share_menu(ui, source, Some(node), actions);
                if is_pg_database && ui.button("Databases and Schemas…").clicked() {
                    actions.push(TreeAction::ChooseScope(source.id.clone()));
                    ui.close();
                }
                if node.expandable && ui.button("Refresh").clicked() {
                    let mut refresh = node_descriptor(node);
                    refresh.children = None;
                    actions.push(TreeAction::RefreshNode(source.id.clone(), refresh));
                    ui.close();
                }
            } else {
                if ui.button("New ▶ Query Console").clicked() {
                    actions.push(TreeAction::NewConsole(source.id.clone(), None));
                    ui.close();
                }
                if ui.button("Refresh").clicked() {
                    actions.push(TreeAction::Refresh(source.id.clone()));
                    ui.close();
                }
                if ui.button("Show Database Graph").clicked() {
                    actions.push(TreeAction::DatabaseGraph(source.id.clone(), None));
                    ui.close();
                }
                if source.driver == DriverKind::Postgres
                    && !source.id.starts_with("file:")
                    && ui.button("Activity Monitor").clicked()
                {
                    actions.push(TreeAction::ActivityMonitor(source.id.clone(), None));
                    ui.close();
                }
                if !source.id.starts_with("file:") || source.driver == DriverKind::Sqlite {
                    super::export_dialog::transfer_menu(ui, source, None, actions);
                }
                if has_scope(source) && ui.button("Databases and Schemas…").clicked() {
                    actions.push(TreeAction::ChooseScope(source.id.clone()));
                    ui.close();
                }
                if ui.button("Disconnect").clicked() {
                    actions.push(TreeAction::Disconnect(source.id.clone()));
                    ui.close();
                }
                self.share_menu(ui, source, None, actions);
                if !source.id.starts_with("file:") {
                    self.move_to_group_menu(ui, source, actions);
                }
                ui.separator();
                if source.id.starts_with("file:") {
                    if ui.button("Close File").clicked() {
                        actions.push(TreeAction::CloseFile(source.id.clone()));
                        ui.close();
                    }
                } else if ui.button("Properties…").clicked() {
                    actions.push(TreeAction::Properties(source.id.clone()));
                    ui.close();
                }
            }
        });
        if node.is_none()
            && let Some(ConnStatus::Failed(error)) =
                self.sources.get(&source.id).map(|source| &source.status)
        {
            response.clone().on_hover_text(error);
        }
        response
    }

    /// DBeast extension: standing agent access, chosen where the object is visible.
    fn share_menu(
        &self,
        ui: &mut Ui,
        source: &DataSource,
        node: Option<&DbObject>,
        actions: &mut Vec<TreeAction>,
    ) {
        let Some(target) = share_target(source, node) else {
            return;
        };
        let read_only = self.mcp_source_read_only(&source.id);
        let levels: &[ShareLevel] = match target {
            ShareTarget::Database(_) => &[
                ShareLevel::NotShared,
                ShareLevel::Ask,
                ShareLevel::Read,
                ShareLevel::ReadWrite,
            ],
            ShareTarget::Table(..) => &[
                ShareLevel::NotShared,
                ShareLevel::Read,
                ShareLevel::ReadWrite,
            ],
        };
        let audiences: Vec<(Option<String>, String)> =
            std::iter::once((None, "All Agents".to_owned()))
                .chain(
                    self.mcp_clients()
                        .into_iter()
                        .map(|(id, label)| (Some(id), label)),
                )
                .collect();
        ui.menu_button("Share with AI Agents", |ui| {
            for (client, label) in &audiences {
                let current = self.mcp_share_level(&target, client.as_deref());
                let title = if current == ShareLevel::NotShared {
                    label.clone()
                } else {
                    format!("{label} · {}", current.short_label())
                };
                ui.push_id(client, |ui| {
                    ui.menu_button(title, |ui| {
                        for &level in levels {
                            let enabled = !(read_only && level == ShareLevel::ReadWrite);
                            if ui
                                .add_enabled(
                                    enabled,
                                    egui::Button::selectable(current == level, level.label()),
                                )
                                .clicked()
                            {
                                actions.push(TreeAction::ShareWithAgents(
                                    target.clone(),
                                    client.clone(),
                                    level,
                                ));
                                ui.close();
                            }
                        }
                    });
                });
            }
            if audiences.len() == 1 {
                ui.label(
                    RichText::new("No agent connections yet")
                        .small()
                        .color(theme::TEXT_MUTED),
                );
            }
            ui.separator();
            if ui.button("Agent Connections…").clicked() {
                actions.push(TreeAction::McpSharingSettings);
                ui.close();
            }
        });
    }

    /// Selected data source, never a group or an empty selection.
    pub(super) fn selected_source_id(&self) -> Option<&str> {
        self.selected
            .as_ref()
            .map(|(id, _)| id.as_str())
            .filter(|id| *id != GROUP_KEY)
    }

    /// Selected group path, for creating a subgroup next to the selection.
    fn selected_group(&self) -> Option<String> {
        match &self.selected {
            Some((id, path)) if id == GROUP_KEY => Some(path.join("/")),
            Some((id, _)) => Some(self.source_group_of(id).to_owned()).filter(|g| !g.is_empty()),
            None => None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn group_row(
        &self,
        ui: &mut Ui,
        state: &ExplorerState,
        row: usize,
        path: &[String],
        actions: &mut Vec<TreeAction>,
        intents: &mut Vec<TreeIntent>,
    ) -> egui::Response {
        let current = &state.index.rows()[row];
        let Some(token) = state.index.token(row) else {
            return ui.label("");
        };
        let joined = path.join("/");
        let count = {
            let mut cache = state.group_counts.borrow_mut();
            let generation = state.index.generation();
            if cache.0 != Some(generation) {
                cache.1.clear();
                for source in &self.config.data_sources {
                    let group = self.source_group_of(&source.id);
                    if group.is_empty() {
                        continue;
                    }
                    // Each prefix of the group path counts the source once.
                    for (end, _) in group.match_indices('/').chain(std::iter::once((group.len(), ""))) {
                        *cache.1.entry(group[..end].to_owned()).or_default() += 1;
                    }
                }
                cache.0 = Some(generation);
            }
            cache.1.get(&joined).copied().unwrap_or(0)
        };
        let badge = count.to_string();
        let response = tree_row_highlighted(
            ui,
            current.depth,
            Some(current.expanded),
            TreeIcon::Kind(ObjectKind::Folder),
            path.last().map_or("", String::as_str),
            Some(&badge),
            "",
            theme::TEXT_MUTED,
            state.index.selected() == Some(row),
            state.search.highlights(row),
            Sense::click(),
        );
        if response.clicked() || response.secondary_clicked() {
            intents.push(TreeIntent::Select(token));
        }
        if (response.clicked() && arrow_hit(&response)) || response.double_clicked() {
            intents.push(if current.expanded {
                TreeIntent::Collapse(token)
            } else {
                TreeIntent::Expand(token)
            });
        }
        if let Some(dragged) = response.dnd_hover_payload::<DraggedSource>() {
            ui.painter()
                .rect_stroke(response.rect, 3.0, (1.5, theme::ACCENT), egui::StrokeKind::Inside);
            if let Some(dragged) = response.dnd_release_payload::<DraggedSource>() {
                actions.push(TreeAction::DropSource(dragged.0.clone(), SourceDrop::Into(joined.clone())));
            }
            let _ = dragged;
        }
        response.context_menu(|ui| {
            if ui.button("New Group…").clicked() {
                actions.push(TreeAction::GroupDialog(GroupDialogMode::New {
                    parent: joined.clone(),
                    source: None,
                }));
                ui.close();
            }
            if ui.button("Rename Group…").clicked() {
                actions.push(TreeAction::GroupDialog(GroupDialogMode::Rename(joined.clone())));
                ui.close();
            }
            if ui
                .button("Remove Group")
                .on_hover_text("Data sources and subgroups move to the parent group; nothing is deleted.")
                .clicked()
            {
                actions.push(TreeAction::RemoveGroup(joined.clone()));
                ui.close();
            }
        });
        response
    }

    fn move_to_group_menu(&self, ui: &mut Ui, source: &DataSource, actions: &mut Vec<TreeAction>) {
        let current = self.source_group_of(&source.id).to_owned();
        ui.menu_button("Move to Group", |ui| {
            if ui
                .add_enabled(!current.is_empty(), egui::Button::new("Top Level"))
                .clicked()
            {
                actions.push(TreeAction::MoveToGroup(source.id.clone(), String::new()));
                ui.close();
            }
            let groups = self.explorer_groups();
            if !groups.is_empty() {
                ui.separator();
            }
            for group in groups {
                let path = group.join("/");
                let label = format!("{}{}", "    ".repeat(group.len() - 1), group.last().unwrap());
                if ui
                    .add_enabled(path != current, egui::Button::selectable(path == current, label))
                    .clicked()
                {
                    actions.push(TreeAction::MoveToGroup(source.id.clone(), path));
                    ui.close();
                }
            }
            ui.separator();
            if ui.button("New Group…").clicked() {
                actions.push(TreeAction::GroupDialog(GroupDialogMode::New {
                    parent: String::new(),
                    source: Some(source.id.clone()),
                }));
                ui.close();
            }
        });
    }

    /// New Group / Rename Group, as a small modal input dialog.
    pub(super) fn group_dialog_ui(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.group_dialog else {
            return;
        };
        let (title, ok) = match &dialog.mode {
            GroupDialogMode::New { .. } => ("New Data Source Group", "Create"),
            GroupDialogMode::Rename(_) => ("Rename Group", "Rename"),
        };
        let mut submit = false;
        let mut cancel = false;
        let response = theme::Dialog::new(Id::new("explorer_group_dialog"), title)
            .resizable(false)
            .default_size([360.0, 140.0])
            .frame(egui::Frame::window(&ctx.global_style()).fill(theme::BG_PANEL).inner_margin(16))
            .show(ctx, |ui| {
                ui.set_width(360.0);
                if let GroupDialogMode::New { parent, .. } = &dialog.mode
                    && !parent.is_empty()
                {
                    ui.label(RichText::new(format!("In {parent}")).color(theme::TEXT_MUTED));
                }
                let label = ui.label("Name:");
                let edit = ui
                    .add(
                        egui::TextEdit::singleline(&mut dialog.name)
                            .desired_width(f32::INFINITY)
                            .char_limit(200)
                            .hint_text("Use / for nested groups"),
                    )
                    .labelled_by(label.id);
                if std::mem::take(&mut dialog.focus) {
                    edit.request_focus();
                }
                submit |= edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if let Some(error) = dialog.error {
                    ui.label(RichText::new(error).color(theme::RED));
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    submit |= ui.button(ok).clicked();
                    cancel |= ui.button("Cancel").clicked();
                });
            });
        if response.should_close() {
            cancel = true;
        }
        if cancel {
            self.group_dialog = None;
            return;
        }
        if submit {
            let dialog = self.group_dialog.take().unwrap();
            let result = match &dialog.mode {
                GroupDialogMode::New { parent, source } => {
                    self.create_group(parent, &dialog.name, source.as_deref())
                }
                GroupDialogMode::Rename(old) => self.rename_group(old, &dialog.name),
            };
            if let Err(error) = result {
                self.group_dialog = Some(GroupDialog {
                    error: Some(error),
                    focus: true,
                    ..dialog
                });
            }
        }
    }

    fn apply_explorer_intent(&mut self, intent: TreeIntent) {
        let token = match intent {
            TreeIntent::Select(token)
            | TreeIntent::Expand(token)
            | TreeIntent::Collapse(token)
            | TreeIntent::Open(token) => token,
        };
        if !self.explorer_ui.index.valid(token) {
            return;
        }
        let row = token.row;
        if let Some(path) = self.explorer_ui.index.group_path(row).map(<[String]>::to_vec) {
            let open = self.expanded.contains(&(GROUP_KEY.into(), path.clone()));
            match intent {
                TreeIntent::Select(_) => {
                    self.explorer_ui.index.select(token);
                    self.tree_action(TreeAction::SelectGroup(path));
                }
                TreeIntent::Expand(_) if !open => self.tree_action(TreeAction::ToggleGroup(path)),
                TreeIntent::Collapse(_) if open => self.tree_action(TreeAction::ToggleGroup(path)),
                _ => {}
            }
            return;
        }
        let Some(slot) = self.explorer_ui.index.source_slot(row) else {
            return;
        };
        let Some(source) = self.explorer_source(slot) else {
            return;
        };
        if self.explorer_ui.index.source_id(row) != Some(source.id.as_str()) {
            return;
        }
        let source_id = source.id.clone();
        let node = self
            .sources
            .get(&source_id)
            .and_then(|source| source.roots.as_deref())
            .and_then(|roots| self.explorer_ui.index.object(row, roots));
        let action = match intent {
            TreeIntent::Select(_) => TreeAction::Select(
                source_id,
                node.map_or_else(Vec::new, |node| node.path.clone()),
            ),
            TreeIntent::Open(_) => {
                let Some(node) =
                    node.filter(|node| matches!(node.kind, ObjectKind::Table | ObjectKind::View))
                else {
                    return;
                };
                TreeAction::OpenTable(source_id, node_descriptor(node))
            }
            TreeIntent::Expand(_) | TreeIntent::Collapse(_) => {
                let expand = matches!(intent, TreeIntent::Expand(_));
                let node = node.map_or_else(source_descriptor, node_descriptor);
                if self
                    .expanded
                    .contains(&(source_id.clone(), node.path.clone()))
                    == expand
                {
                    return;
                }
                TreeAction::Toggle(source_id, node)
            }
        };
        if matches!(intent, TreeIntent::Select(_)) {
            self.explorer_ui.index.select(token);
        }
        self.tree_action(action);
    }

    pub(super) fn collapse_explorer(&mut self) {
        self.pending_editor_focus = false;
        self.pending_action_focus = None;
        self.explorer_ui.cancel_reveal();
        self.explorer_ui.focus_search = false;
        self.explorer_ui.select_query = false;
        self.expanded.clear();
        if let Some((_, path)) = &mut self.selected {
            path.clear();
        }
        self.explorer_ui.invalidate();
        self.explorer_ui.request_focus();
        self.backend_ctx().request_repaint();
    }

    /// Explicit reveal is keyed by stable tab/source identity and progresses only
    /// after relevant events. It never changes the user's database/schema scope.
    pub(super) fn scroll_from_editor(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let Some(source) = self.source(&tab.ds_id) else {
            return;
        };
        let mut path = Vec::new();
        if let TabKind::Table { schema, table, .. } = &tab.kind {
            if source.driver == DriverKind::Postgres {
                let database = tab
                    .database
                    .as_ref()
                    .cloned()
                    .or_else(|| {
                        self.sources
                            .get(&source.id)
                            .and_then(|state| state.roots.as_deref())
                            .and_then(|roots| {
                                roots
                                    .iter()
                                    .find(|node| node.detail == crate::drivers::DEFAULT_DATABASE)
                            })
                            .map(|node| node.name.clone())
                    })
                    .unwrap_or_else(|| source.database.clone());
                if !database.is_empty() {
                    path.push(database);
                }
            }
            path.push(schema.clone().unwrap_or_else(|| {
                if source.driver == DriverKind::Sqlite {
                    "main".into()
                } else {
                    source.database.clone()
                }
            }));
            path.push(table.clone());
        }
        // A source inside a collapsed group has no row until its groups open.
        let mut open = Vec::new();
        for part in groups::segments(self.source_group_of(&tab.ds_id)) {
            open.push(part);
            self.expanded.insert((GROUP_KEY.into(), open.clone()));
        }
        self.explorer_ui
            .reveal
            .begin(tab.id, tab.ds_id.clone(), path);
        self.pending_editor_focus = false;
        self.pending_action_focus = None;
        self.explorer_ui.focus_search = false;
        self.explorer_ui.select_query = false;
        self.explorer_ui.focus_tree = false;
        self.explorer_ui.reveal_focus_pending = false;
        self.explorer_ui.reveal_dirty = true;
        self.show_explorer = true;
        self.show_files = false;
        self.filter.clear();
        self.backend_ctx().request_repaint();
    }

    pub(super) fn advance_explorer_reveal(&mut self) {
        if !std::mem::take(&mut self.explorer_ui.reveal_dirty) {
            return;
        }
        let Some(request) = self.explorer_ui.reveal.pending() else {
            return;
        };
        if self
            .tabs
            .get(self.active)
            .is_none_or(|tab| tab.id != request.tab_id || tab.ds_id != request.source_id)
        {
            self.explorer_ui.cancel_reveal();
            return;
        }
        let Some(source) = self.source(&request.source_id) else {
            self.explorer_ui.cancel_reveal();
            return;
        };
        let source_id = request.source_id.clone();
        let token = request.token;
        let current = self.sources.get(&source_id);
        let step = request.step(
            SourceTree {
                source,
                group: &[],
                slot: SourceSlot::Config(0),
                roots: current.and_then(|state| state.roots.as_deref()),
                loading: current.map_or(&self.explorer_ui.empty_loading, |state| &state.loading),
                connecting: current
                    .is_some_and(|state| matches!(state.status, ConnStatus::Connecting)),
            },
            &self.expanded,
        );
        match step {
            RevealStep::Expand(path) => {
                self.expanded.insert((source_id, path));
                self.explorer_ui.invalidate();
                self.backend_ctx().request_repaint();
            }
            RevealStep::Load(path) if path.is_empty() => self.connect(&source_id),
            RevealStep::Load(path) => {
                let node = self
                    .sources
                    .get(&source_id)
                    .and_then(|state| state.roots.as_deref())
                    .and_then(|roots| find_node(roots, &path))
                    .map(node_descriptor);
                if let Some(node) = node {
                    self.load_node(&source_id, node);
                } else {
                    self.explorer_ui.reveal.finish(token);
                }
            }
            RevealStep::Select(path) => {
                if self.explorer_ui.reveal.finish(token) {
                    self.selected = Some((source_id, path));
                    self.explorer_ui.invalidate();
                    self.explorer_ui.reveal_selection = true;
                    self.explorer_ui.request_reveal_focus();
                }
            }
            RevealStep::HiddenByScope => {
                self.explorer_ui.reveal.finish(token);
                self.notice = Some(ServicesNotice::info(
                    "The editor's object is hidden by the current database or schema scope".into(),
                ));
            }
            RevealStep::NotFound => {
                self.explorer_ui.reveal.finish(token);
                self.notice = Some(ServicesNotice::info(
                    "The editor's object is not in the fetched database tree".into(),
                ));
            }
            RevealStep::Waiting => {}
        }
    }
}

#[derive(Default)]
struct ExplorerKeys {
    previous: bool,
    next: bool,
    first: bool,
    last: bool,
    page_up: bool,
    page_down: bool,
    left: bool,
    right: bool,
    activate: bool,
    edit_data: bool,
    find: bool,
    escape: bool,
    hide: bool,
}
impl ExplorerKeys {
    fn any(&self) -> bool {
        self.previous
            || self.next
            || self.first
            || self.last
            || self.page_up
            || self.page_down
            || self.left
            || self.right
            || self.activate
            || self.edit_data
            || self.find
            || self.escape
            || self.hide
    }
}
fn explorer_keys(ui: &Ui, tree: bool) -> ExplorerKeys {
    ui.input_mut(|input| ExplorerKeys {
        previous: input.consume_key(Modifiers::NONE, Key::ArrowUp),
        next: input.consume_key(Modifiers::NONE, Key::ArrowDown),
        first: tree && input.consume_key(Modifiers::NONE, Key::Home),
        last: tree && input.consume_key(Modifiers::NONE, Key::End),
        page_up: tree && input.consume_key(Modifiers::NONE, Key::PageUp),
        page_down: tree && input.consume_key(Modifiers::NONE, Key::PageDown),
        left: tree && input.consume_key(Modifiers::NONE, Key::ArrowLeft),
        right: tree && input.consume_key(Modifiers::NONE, Key::ArrowRight),
        activate: input.consume_key(Modifiers::NONE, Key::Enter),
        edit_data: tree && input.consume_key(Modifiers::NONE, Key::F4),
        find: input.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::F)),
        hide: input.consume_key(Modifiers::SHIFT, Key::Escape),
        escape: input.consume_key(Modifiers::NONE, Key::Escape),
    })
}

fn lock_tree_navigation(ui: &Ui, id: Id) {
    ui.memory_mut(|memory| {
        memory.set_focus_lock_filter(
            id,
            egui::EventFilter {
                horizontal_arrows: true,
                vertical_arrows: true,
                ..Default::default()
            },
        );
    });
}
/// Maps an explorer node to the database or table the MCP broker can share.
/// Paths are `[db,] schema, table`; MySQL databases are its schemas.
pub(super) fn share_target(source: &DataSource, node: Option<&DbObject>) -> Option<ShareTarget> {
    let key = |database: Option<&String>| ShareKey {
        source_id: source.id.clone(),
        database: database
            .filter(|database| **database != source.database)
            .cloned(),
    };
    let Some(node) = node else {
        return Some(ShareTarget::Database(key(None)));
    };
    match (source.driver, node.kind) {
        (DriverKind::Postgres, ObjectKind::Database) => {
            Some(ShareTarget::Database(key(Some(&node.name))))
        }
        (DriverKind::MySql, ObjectKind::Schema) if node.path.len() == 1 => {
            Some(ShareTarget::Database(key(Some(&node.name))))
        }
        (_, ObjectKind::Table | ObjectKind::View) if node.path.len() >= 2 => {
            let schema = node.path[node.path.len() - 2].clone();
            let database = match source.driver {
                DriverKind::Postgres | DriverKind::MySql => node.path.first(),
                DriverKind::Sqlite => None,
            };
            Some(ShareTarget::Table(
                key(database),
                crate::mcp::TableRef {
                    schema,
                    name: node.name.clone(),
                },
            ))
        }
        _ => None,
    }
}

fn node_descriptor(node: &DbObject) -> DbObject {
    DbObject {
        kind: node.kind,
        name: node.name.clone(),
        detail: node.detail.clone(),
        path: node.path.clone(),
        children: node.children.as_ref().map(|_| Vec::new()),
        expandable: node.expandable,
    }
}
/// One level of children, enough to know which schemas a database shows.
fn node_descriptor_with_children(node: &DbObject) -> DbObject {
    DbObject {
        children: node.children.as_ref().map(|children| children.iter().map(node_descriptor).collect()),
        ..node_descriptor(node)
    }
}
fn source_descriptor() -> DbObject {
    DbObject {
        kind: ObjectKind::Folder,
        name: String::new(),
        detail: String::new(),
        path: Vec::new(),
        children: None,
        expandable: true,
    }
}
fn find_node<'a>(nodes: &'a [DbObject], path: &[String]) -> Option<&'a DbObject> {
    for node in nodes {
        if node.path == path {
            return Some(node);
        }
        if let Some(children) = &node.children
            && let Some(found) = find_node(children, path)
        {
            return Some(found);
        }
    }
    None
}

impl App {
    pub(super) fn tree_action(&mut self, action: TreeAction) {
        if matches!(
            &action,
            TreeAction::Toggle(..)
                | TreeAction::Refresh(..)
                | TreeAction::RefreshNode(..)
                | TreeAction::Disconnect(..)
                | TreeAction::CloseFile(..)
        ) {
            self.explorer_ui.invalidate();
        }
        match action {
            TreeAction::Select(id, path) => self.selected = Some((id, path)),
            TreeAction::Toggle(id, node) => {
                let key = (id.clone(), node.path.clone());
                if node.path.is_empty() {
                    // Data source row: connect on first expand.
                    if self.expanded.remove(&key) {
                        return;
                    }
                    self.expanded.insert(key);
                    if self
                        .sources
                        .get(&id)
                        .is_none_or(|source| source.roots.is_none())
                    {
                        self.connect(&id);
                    }
                    return;
                }
                if self.expanded.remove(&key) {
                    return;
                }
                self.expanded.insert(key);
                if node.children.is_none() && node.kind != ObjectKind::Folder {
                    self.load_node(&id, node);
                }
            }
            TreeAction::OpenTable(id, node) => self.open_table(id, &node),
            TreeAction::NewConsole(id, database) => self.new_console_in(id, database),
            TreeAction::ChooseScope(id) => self.scope_chooser = Some(id),
            TreeAction::Refresh(id) => {
                self.expanded.retain(|(i, p)| *i != id || p.is_empty());
                self.connect(&id);
            }
            TreeAction::RefreshNode(id, node) => {
                self.expanded.insert((id.clone(), node.path.clone()));
                self.load_node(&id, node);
            }
            TreeAction::Disconnect(id) => self.disconnect(&id),
            TreeAction::Properties(id) => {
                let sel = (!id.is_empty()).then_some(id);
                self.dialog = Some(DataSourceDialog::new(
                    self.config.data_sources.clone(),
                    sel.as_deref(),
                ));
            }
            TreeAction::CopyName(name) => self.backend_ctx().copy_text(name),
            TreeAction::CloseFile(id) => self.close_file(&id),
            TreeAction::SelectGroup(path) => self.selected = Some((GROUP_KEY.into(), path)),
            TreeAction::ToggleGroup(path) => {
                let key = (GROUP_KEY.to_string(), path);
                if !self.expanded.remove(&key) {
                    self.expanded.insert(key);
                }
                self.explorer_ui.invalidate();
            }
            TreeAction::MoveToGroup(source, group) => self.move_source_to_group(&source, &group),
            TreeAction::DropSource(source, target) => self.drop_source(&source, target),
            TreeAction::GroupDialog(mode) => {
                let name = match &mode {
                    GroupDialogMode::Rename(path) => {
                        groups::segments(path).pop().unwrap_or_default()
                    }
                    GroupDialogMode::New { .. } => String::new(),
                };
                self.group_dialog = Some(GroupDialog {
                    mode,
                    name,
                    error: None,
                    focus: true,
                });
            }
            TreeAction::RemoveGroup(path) => self.remove_group(&path),
            TreeAction::ShareWithAgents(target, client, level) => {
                self.set_mcp_share(target, client, level)
            }
            TreeAction::McpSharingSettings => self.open_mcp_sharing(),
            TreeAction::CompareStructure(id, node) => self.open_structure_compare(&id, &node),
            TreeAction::DatabaseGraph(id, node) => self.open_database_graph(&id, node.as_ref()),
            TreeAction::Transfer(id, node, entry) => self.open_transfer(&id, node.as_ref(), entry),
            TreeAction::ActivityMonitor(id, database) => self.open_activity_monitor(&id, database),
            TreeAction::Ddl(id, node) => {
                if let Some(ds) = self.source(&id).cloned() {
                    let ds = match pg_database(&ds, &node) {
                        Some(db) => ds.with_database(&db),
                        None => ds,
                    };
                    self.backend.generate_ddl(ds, node);
                }
            }
        }
    }

    /// Loads the children of a node without expanding it in the tree.
    pub(super) fn load_node(&mut self, ds_id: &str, node: DbObject) {
        if let Some(ds) = self.source(ds_id).cloned() {
            if !self
                .sources
                .entry(ds_id.to_string())
                .or_default()
                .loading
                .insert(node.path.clone())
            {
                return;
            }
            self.explorer_ui.invalidate();
            self.backend.load_children(ds, node);
        }
    }

    fn backend_ctx(&self) -> &egui::Context {
        self.backend.ctx()
    }
}

/// Drag payload of a data source row.
pub(super) struct DraggedSource(pub String);

/// Data source rows can be dragged to reorder them or into a group.
fn source_drag_and_drop(
    ui: &Ui,
    response: &egui::Response,
    source: &DataSource,
    actions: &mut Vec<TreeAction>,
) {
    // The payload is built only when a drag starts, not for every row and frame.
    if response.drag_started() {
        egui::DragAndDrop::set_payload(ui.ctx(), DraggedSource(source.id.clone()));
    }
    if response.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        if let Some(pointer) = ui.ctx().pointer_interact_pos() {
            let layer = egui::LayerId::new(egui::Order::Tooltip, Id::new("dragged_source"));
            ui.ctx().layer_painter(layer).text(
                pointer + egui::vec2(14.0, 0.0),
                egui::Align2::LEFT_CENTER,
                source.display_name(),
                egui::FontId::proportional(13.0),
                theme::TEXT,
            );
        }
    }
    let Some(dragged) = response.dnd_hover_payload::<DraggedSource>() else {
        return;
    };
    if dragged.0 == source.id {
        return;
    }
    let after = ui
        .ctx()
        .pointer_interact_pos()
        .is_some_and(|pointer| pointer.y > response.rect.center().y);
    let y = if after {
        response.rect.bottom()
    } else {
        response.rect.top()
    };
    ui.painter().hline(
        response.rect.x_range(),
        y,
        egui::Stroke::new(2.0, theme::ACCENT),
    );
    if let Some(dragged) = response.dnd_release_payload::<DraggedSource>() {
        actions.push(TreeAction::DropSource(
            dragged.0.clone(),
            SourceDrop::Beside {
                target: source.id.clone(),
                after,
            },
        ));
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn tree_row(
    ui: &mut Ui,
    depth: usize,
    arrow: Option<bool>,
    icon: TreeIcon<'_>,
    name: &str,
    detail: &str,
    detail_color: Color32,
    selected: bool,
) -> egui::Response {
    tree_row_with_badge(
        ui,
        depth,
        arrow,
        icon,
        name,
        None,
        detail,
        detail_color,
        selected,
    )
}

/// Tree row with an optional counter after the name, e.g. "1 of 3".
/// A click on the counter is reported by [`badge_hit`].
#[allow(clippy::too_many_arguments)]
pub(super) fn tree_row_with_badge(
    ui: &mut Ui,
    depth: usize,
    arrow: Option<bool>,
    icon: TreeIcon<'_>,
    name: &str,
    badge: Option<&str>,
    detail: &str,
    detail_color: Color32,
    selected: bool,
) -> egui::Response {
    tree_row_highlighted(
        ui,
        depth,
        arrow,
        icon,
        name,
        badge,
        detail,
        detail_color,
        selected,
        &[],
        Sense::click(),
    )
}

#[allow(clippy::too_many_arguments)]
fn tree_row_highlighted(
    ui: &mut Ui,
    depth: usize,
    arrow: Option<bool>,
    icon: TreeIcon<'_>,
    name: &str,
    badge: Option<&str>,
    detail: &str,
    detail_color: Color32,
    selected: bool,
    highlights: &[Range<usize>],
    sense: Sense,
) -> egui::Response {
    let h = ROW_HEIGHT;
    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(ui.available_width().max(260.0), h),
        sense,
    );
    resp.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, name)
    });
    let painter = ui.painter_at(rect);
    if selected {
        painter.rect_filled(rect, 3.0, theme::BG_SELECTED);
    } else if resp.hovered() {
        painter.rect_filled(rect, 3.0, theme::BG_HOVER);
    }
    let mut x = rect.left() + 4.0 + depth as f32 * 16.0;
    let cy = rect.center().y;
    if let Some(open) = arrow {
        painter.text(
            egui::pos2(x + 5.0, cy),
            egui::Align2::CENTER_CENTER,
            if open { "⏷" } else { "⏵" },
            egui::FontId::proportional(10.0),
            theme::TEXT_MUTED,
        );
    }
    x += 14.0;
    match icon {
        TreeIcon::Text(t) => {
            painter.text(
                egui::pos2(x, cy),
                egui::Align2::LEFT_CENTER,
                t,
                egui::FontId::proportional(13.0),
                theme::SQL_FUNCTION,
            );
        }
        TreeIcon::Kind(k) => paint_icon(&painter, egui::pos2(x + 7.0, cy), k),
    }
    x += 20.0;
    let g = if highlights.is_empty() {
        painter.text(
            egui::pos2(x, cy),
            egui::Align2::LEFT_CENTER,
            name,
            egui::FontId::proportional(13.0),
            theme::TEXT,
        )
    } else {
        let mut job = egui::text::LayoutJob::default();
        let format = egui::TextFormat {
            font_id: egui::FontId::proportional(13.0),
            color: theme::TEXT,
            ..Default::default()
        };
        let mut previous = 0;
        for range in highlights {
            job.append(&name[previous..range.start], 0.0, format.clone());
            job.append(
                &name[range.clone()],
                0.0,
                egui::TextFormat {
                    background: theme::SEARCH_MATCH,
                    ..format.clone()
                },
            );
            previous = range.end;
        }
        job.append(&name[previous..], 0.0, format);
        let galley = painter.layout_job(job);
        let rect =
            egui::Rect::from_min_size(egui::pos2(x, cy - galley.size().y / 2.0), galley.size());
        painter.galley(rect.min, galley, theme::TEXT);
        rect
    };
    let mut after_name = g.right();
    if let Some(b) = badge {
        let galley = painter.layout_no_wrap(
            b.to_string(),
            egui::FontId::proportional(11.5),
            theme::TEXT_MUTED,
        );
        // Counters are plain muted text that underlines on hover.
        let r = egui::Rect::from_min_size(
            egui::pos2(g.right() + 4.0, cy - galley.size().y / 2.0 - 2.0),
            galley.size() + egui::vec2(8.0, 4.0),
        );
        let text_pos = r.min + egui::vec2(4.0, 2.0);
        let hovered = resp
            .hover_pos()
            .is_some_and(|p| r.contains(p));
        if hovered {
            let y = text_pos.y + galley.size().y - 0.5;
            painter.line_segment(
                [egui::pos2(text_pos.x, y), egui::pos2(text_pos.x + galley.size().x, y)],
                egui::Stroke::new(1.0, theme::TEXT_MUTED),
            );
        }
        painter.galley(text_pos, galley, theme::TEXT_MUTED);
        ui.ctx()
            .data_mut(|d| d.insert_temp(resp.id.with("badge"), r));
        after_name = r.right();
    }
    if !detail.is_empty() {
        painter.text(
            egui::pos2(after_name + 8.0, cy),
            egui::Align2::LEFT_CENTER,
            detail,
            egui::FontId::proportional(12.0),
            detail_color,
        );
    }
    // Remember where the arrow is so a click on it toggles the node.
    let arrow_rect = egui::Rect::from_min_size(
        egui::pos2(rect.left() + depth as f32 * 16.0, rect.top()),
        egui::vec2(20.0, h),
    );
    ui.ctx()
        .data_mut(|d| d.insert_temp(resp.id.with("arrow"), arrow_rect));
    resp
}

pub(super) enum TreeIcon<'a> {
    Text(&'a str),
    Kind(ObjectKind),
}

/// Small vector icons for object kinds; the default
/// fonts lack most of the needed glyphs.
fn paint_icon(p: &egui::Painter, c: egui::Pos2, kind: ObjectKind) {
    use egui::{Rect, Stroke, StrokeKind, pos2, vec2};
    let blue = Color32::from_rgb(0x54, 0x8A, 0xF7);
    let violet = Color32::from_rgb(0xB1, 0x89, 0xF5);
    let amber = Color32::from_rgb(0xE5, 0xB5, 0x4A);
    let grey = theme::TEXT_MUTED;
    let r = Rect::from_center_size(c, vec2(12.0, 11.0));
    match kind {
        ObjectKind::Table | ObjectKind::View => {
            let col = if kind == ObjectKind::Table {
                blue
            } else {
                violet
            };
            p.rect_stroke(r, 1.5, Stroke::new(1.2, col), StrokeKind::Inside);
            p.rect_filled(
                Rect::from_min_max(r.min, pos2(r.max.x, r.min.y + 3.5)),
                1.5,
                col,
            );
            p.line_segment(
                [pos2(r.min.x, c.y + 1.5), pos2(r.max.x, c.y + 1.5)],
                Stroke::new(1.0, col),
            );
            p.line_segment(
                [pos2(c.x, r.min.y + 3.0), pos2(c.x, r.max.y)],
                Stroke::new(1.0, col),
            );
        }
        ObjectKind::Schema | ObjectKind::Database => {
            let col = if kind == ObjectKind::Database {
                blue
            } else {
                grey
            };
            for dy in [-3.5, 0.0, 3.5] {
                p.rect_stroke(
                    Rect::from_center_size(pos2(c.x, c.y + dy), vec2(11.0, 3.5)),
                    1.5,
                    Stroke::new(1.1, col),
                    StrokeKind::Inside,
                );
            }
        }
        ObjectKind::Column => {
            p.rect_filled(Rect::from_center_size(c, vec2(4.0, 11.0)), 1.0, blue);
            p.rect_stroke(
                Rect::from_center_size(c, vec2(11.0, 11.0)),
                1.5,
                Stroke::new(1.0, grey),
                StrokeKind::Inside,
            );
        }
        ObjectKind::Key => {
            p.circle_stroke(pos2(c.x - 2.5, c.y), 3.0, Stroke::new(1.4, amber));
            p.line_segment(
                [pos2(c.x + 0.5, c.y), pos2(c.x + 6.0, c.y)],
                Stroke::new(1.4, amber),
            );
            p.line_segment(
                [pos2(c.x + 4.5, c.y), pos2(c.x + 4.5, c.y + 3.0)],
                Stroke::new(1.4, amber),
            );
        }
        ObjectKind::Index => {
            for (i, w) in [11.0, 8.0, 5.0].iter().enumerate() {
                let y = c.y - 4.0 + i as f32 * 4.0;
                p.line_segment(
                    [pos2(c.x - 5.5, y), pos2(c.x - 5.5 + w, y)],
                    Stroke::new(1.6, amber),
                );
            }
        }
        ObjectKind::Routine => {
            p.text(
                c,
                egui::Align2::CENTER_CENTER,
                "ƒ",
                egui::FontId::proportional(14.0),
                violet,
            );
        }
        ObjectKind::Sequence => {
            p.text(
                c,
                egui::Align2::CENTER_CENTER,
                "#",
                egui::FontId::monospace(12.0),
                grey,
            );
        }
        ObjectKind::Folder => {
            let col = Color32::from_rgb(0x8C, 0x9B, 0xB0);
            p.rect_filled(
                Rect::from_min_max(pos2(r.min.x, r.min.y + 2.0), r.max),
                1.5,
                col,
            );
            p.rect_filled(
                Rect::from_min_max(r.min, pos2(r.min.x + 5.0, r.min.y + 3.0)),
                1.0,
                col,
            );
        }
    }
}

/// Whether the click of a row landed on its counter badge.
pub(super) fn badge_hit(resp: &egui::Response) -> bool {
    let rect: Option<egui::Rect> = resp.ctx.data(|d| d.get_temp(resp.id.with("badge")));
    match (rect, resp.interact_pointer_pos()) {
        (Some(r), Some(p)) => r.contains(p),
        _ => false,
    }
}

pub(super) fn arrow_hit(resp: &egui::Response) -> bool {
    let rect: Option<egui::Rect> = resp.ctx.data(|d| d.get_temp(resp.id.with("arrow")));
    match (rect, resp.interact_pointer_pos()) {
        (Some(r), Some(p)) => r.contains(p),
        _ => false,
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").chars().take(60).collect()
}

/// Database of a PostgreSQL tree node (the first element of its path).
pub(super) fn pg_database(ds: &DataSource, node: &DbObject) -> Option<String> {
    (ds.driver == DriverKind::Postgres)
        .then(|| node.path.first().cloned())
        .flatten()
}

/// Data sources with several databases (PostgreSQL) or schemas (MySQL) to choose from.
pub(super) fn has_scope(ds: &DataSource) -> bool {
    matches!(ds.driver, DriverKind::Postgres | DriverKind::MySql)
}

/// Whether a top-level node (database or MySQL schema) is chosen to be shown.
pub(super) fn root_visible(ds: &DataSource, node: &DbObject) -> bool {
    match (ds.driver, node.kind) {
        (DriverKind::Postgres, ObjectKind::Database) => {
            ds.shows_database(&node.name, node.detail == crate::drivers::DEFAULT_DATABASE)
        }
        (DriverKind::MySql, ObjectKind::Schema) => ds.shows_database(
            &node.name,
            ds.database.is_empty() || ds.database == node.name,
        ),
        _ => true,
    }
}

/// "12" when everything is shown, "1 of 3" otherwise.
fn count_badge(shown: usize, total: usize) -> String {
    if shown == total {
        total.to_string()
    } else {
        format!("{shown} of {total}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cached_app() -> (App, String) {
        let mut app = crate::ui::actions::tests::isolated_app(&egui::Context::default());
        let mut source = DataSource::new(DriverKind::Sqlite);
        source.name = "Cached source".into();
        let id = source.id.clone();
        app.config.data_sources.push(source);
        app.sources.insert(
            id.clone(),
            crate::ui::SourceState {
                roots: Some(vec![DbObject::new(ObjectKind::Schema, "main", &[])]),
                ..Default::default()
            },
        );
        (app, id)
    }

    #[test]
    fn collapsing_an_unloaded_node_does_not_reopen_or_start_a_load() {
        let (mut app, id) = cached_app();
        let node = DbObject::new(ObjectKind::Schema, "main", &[]);
        let key = (id.clone(), node.path.clone());
        app.expanded.insert(key.clone());
        app.tree_action(TreeAction::Toggle(id.clone(), node));
        assert!(!app.expanded.contains(&key));
        assert!(app.sources[&id].loading.is_empty());
    }

    #[test]
    fn opening_a_cached_source_does_not_connect_or_discard_metadata() {
        let (mut app, id) = cached_app();
        app.tree_action(TreeAction::Toggle(id.clone(), source_descriptor()));
        assert!(app.expanded.contains(&(id.clone(), Vec::new())));
        assert!(!matches!(app.sources[&id].status, ConnStatus::Connecting));
        assert_eq!(app.sources[&id].roots.as_ref().unwrap()[0].name, "main");
    }

    #[test]
    fn index_and_search_are_unchanged_on_idle_and_simple_selection() {
        let (app, id) = cached_app();
        let mut state = ExplorerState::default();
        app.refresh_explorer_index(&mut state);
        let token = state.index.token(0).unwrap();
        state.index.select(token);
        state.search.update(&state.index, "", true, |_| None);
        for _ in 0..100 {
            app.refresh_explorer_index(&mut state);
            assert!(state.index.valid(token));
            assert!(
                !state
                    .search
                    .update(&state.index, "", true, |_| panic!("idle tree scan"))
            );
            assert_eq!(state.index.source_id(0), Some(id.as_str()));
            assert_eq!(state.index.selected(), Some(0));
        }
        state.invalidate();
        app.refresh_explorer_index(&mut state);
        assert!(!state.index.valid(token));
    }

    #[test]
    fn metadata_failure_cancels_only_the_affected_reveal() {
        let mut state = ExplorerState::default();
        state
            .reveal
            .begin(1, "source-a".into(), vec!["main".into()]);
        state.cancel_reveal_for_source("source-b");
        assert!(state.reveal.pending().is_some());
        state.cancel_reveal_for_source("source-a");
        state.invalidate();
        assert!(state.reveal.pending().is_none());
    }

    #[test]
    fn cancelled_completed_reveal_does_not_restore_focus_after_a_modal() {
        let mut state = ExplorerState::default();
        let token = state.reveal.begin(1, "source".into(), Vec::new());
        state.reveal.finish(token);
        state.request_reveal_focus();
        // The modal kept this request pending after Select completed.
        state.cancel_reveal();
        assert!(!state.focus_tree);
        state.request_focus();
        state.cancel_reveal();
        assert!(
            state.focus_tree,
            "ordinary tree clicks keep their requested focus"
        );
    }

    #[test]
    fn collapse_all_survives_late_default_database_metadata() {
        let (mut app, id) = cached_app();
        app.config.data_sources[0].driver = DriverKind::Postgres;
        app.expanded.insert((id.clone(), Vec::new()));
        app.collapse_explorer();
        let database = DbObject::new(ObjectKind::Database, "default_db", &[])
            .with_detail(crate::drivers::DEFAULT_DATABASE);
        app.children_loaded(id.clone(), None, Ok(vec![database]));
        assert!(app.expanded.is_empty());
        assert_eq!(
            app.sources[&id].roots.as_ref().unwrap()[0].name,
            "default_db"
        );
        assert!(app.sources[&id].loading.is_empty());
    }

    #[test]
    fn failed_children_stop_reveal_without_queuing_an_automatic_retry() {
        let (mut app, id) = cached_app();
        let tab = app.add_tab(
            "Console".into(),
            id.clone(),
            TabKind::Console { sql: String::new() },
        );
        app.explorer_ui.reveal.begin(
            app.tabs[tab].id,
            id.clone(),
            vec!["main".into(), "target".into()],
        );
        app.explorer_ui.reveal_dirty = true;
        app.sources
            .get_mut(&id)
            .unwrap()
            .loading
            .insert(vec!["main".into()]);
        app.children_loaded(
            id.clone(),
            Some(vec!["main".into()]),
            Err("fixture metadata failure".into()),
        );
        // Assert cancellation before advancing, so a broken guard never starts I/O in this test.
        assert!(app.explorer_ui.reveal.pending().is_none());
        assert!(app.sources[&id].loading.is_empty());
        app.advance_explorer_reveal();
        assert!(app.sources[&id].loading.is_empty());
        assert!(matches!(app.sources[&id].status, ConnStatus::Failed(_)));
    }

    #[test]
    fn disconnected_sources_ignore_late_children() {
        let (mut app, id) = cached_app();
        app.disconnect(&id);
        assert!(!app.sources.contains_key(&id));
        app.children_loaded(
            id.clone(),
            None,
            Ok(vec![DbObject::new(ObjectKind::Schema, "late", &[])]),
        );
        assert!(!app.sources.contains_key(&id));
        assert!(!app.expanded.iter().any(|(source, _)| source == &id));
    }

    #[test]
    fn collapsing_a_loading_schema_stays_collapsed_after_children_arrive() {
        let (mut app, id) = cached_app();
        let node = DbObject::new(ObjectKind::Schema, "main", &[]);
        let key = (id.clone(), node.path.clone());
        app.expanded.insert((id.clone(), Vec::new()));
        app.expanded.insert(key.clone());
        app.sources
            .get_mut(&id)
            .unwrap()
            .loading
            .insert(node.path.clone());
        app.tree_action(TreeAction::Toggle(id.clone(), node));
        assert!(!app.expanded.contains(&key));
        let table = DbObject::new(ObjectKind::Table, "target", &key.1);
        app.children_loaded(
            id.clone(),
            Some(key.1.clone()),
            Ok(vec![DbObject::folder("tables", &key.1, vec![table])]),
        );
        assert!(!app.expanded.contains(&key));
        assert!(app.sources[&id].loading.is_empty());
        assert!(
            app.sources[&id].roots.as_ref().unwrap()[0]
                .children
                .is_some()
        );
    }
}
