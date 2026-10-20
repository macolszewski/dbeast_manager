//! Client-side search indexes the fetched page and pending edits only.

use super::{GridRow, ResultSet, Shown, theme};
use crate::model::Value;
use crate::search::{SearchOptions, SearchPattern};
use crate::sqlgen::CellEdit;
use egui::{Id, Key, KeyboardShortcut, Modifiers, RichText, Ui};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

const FIND: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::F);
const NEXT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::G);
const PREVIOUS: KeyboardShortcut =
    KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::G);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CellMatch {
    cell: (GridRow, usize),
    row_position: usize,
}

impl CellMatch {
    fn position(self, transposed: bool) -> (usize, usize) {
        if transposed {
            (self.cell.1, self.row_position)
        } else {
            (self.row_position, self.cell.1)
        }
    }
}

pub(super) struct FindState {
    pub(super) visible: bool,
    query: String,
    focus_query: bool,
    match_case: bool,
    whole_words: bool,
    filter_rows: bool,
    dirty: bool,
    matches: Vec<CellMatch>,
    matched_cells: HashSet<(GridRow, usize)>,
    active: Option<usize>,
    last_selection: Option<(GridRow, usize)>,
    filtered: Vec<GridRow>,
    filtered_positions: HashMap<GridRow, usize>,
    transposed: bool,
    focusable_buttons: Vec<Id>,
    #[cfg(test)]
    rebuilds: usize,
}

impl Default for FindState {
    fn default() -> Self {
        Self {
            visible: false,
            query: String::new(),
            focus_query: false,
            match_case: false,
            whole_words: false,
            filter_rows: false,
            dirty: true,
            matches: Vec::new(),
            matched_cells: HashSet::new(),
            active: None,
            last_selection: None,
            filtered: Vec::new(),
            filtered_positions: HashMap::new(),
            transposed: false,
            focusable_buttons: Vec::new(),
            #[cfg(test)]
            rebuilds: 0,
        }
    }
}

impl FindState {
    pub(super) fn action_has_focus(&self, grid_id: Id, focused: Id) -> bool {
        self.visible
            && (focused == grid_id.with("find_current_page")
                || self.focusable_buttons.contains(&focused))
    }

    pub(super) fn invalidate(&mut self) {
        self.dirty = true;
    }

    pub(super) fn filtered_rows(&self) -> Option<&[GridRow]> {
        (self.visible && self.filter_rows && !self.query.is_empty())
            .then_some(self.filtered.as_slice())
    }

    pub(super) fn filtered_index(&self, row: GridRow) -> Option<usize> {
        self.filtered_positions.get(&row).copied()
    }

    pub(super) fn matches_cell(&self, row: GridRow, column: usize) -> bool {
        self.visible && self.matched_cells.contains(&(row, column))
    }

    fn owns_focus(&self, ui: &Ui, input_id: Id) -> bool {
        self.visible
            && ui.memory(|memory| {
                memory.focused().is_some_and(|focused| {
                    focused == input_id || self.focusable_buttons.contains(&focused)
                })
            })
    }

    /// Reuse all index allocations; idle frames do not revisit cell values.
    fn refresh(&mut self, rs: &ResultSet) -> bool {
        if !self.visible || (!self.dirty && self.transposed == rs.transposed) {
            return false;
        }
        self.matches.clear();
        self.matched_cells.clear();
        self.filtered.clear();
        self.filtered_positions.clear();
        self.transposed = rs.transposed;
        if !self.query.is_empty() {
            let pattern = SearchPattern::new(
                &self.query,
                SearchOptions {
                    match_case: self.match_case,
                    whole_words: self.whole_words,
                },
            );
            let rows = rs
                .order
                .iter()
                .copied()
                .map(GridRow::Data)
                .chain((0..rs.edits.inserted.len()).map(GridRow::New));
            for (row_position, row) in rows.enumerate() {
                if !rs.local_filter.accepts(row) {
                    continue;
                }
                let before = self.matches.len();
                for column in 0..rs.data.columns.len() {
                    if pattern.is_match(&search_text(rs.cell(row, column))) {
                        let cell = (row, column);
                        self.matches.push(CellMatch { cell, row_position });
                        self.matched_cells.insert(cell);
                    }
                }
                // New rows must remain editable even before they contain the
                // search text. They do not increase the matching-cell count.
                if before != self.matches.len() || matches!(row, GridRow::New(_)) {
                    self.filtered_positions.insert(row, self.filtered.len());
                    self.filtered.push(row);
                }
            }
            if rs.transposed {
                self.matches.sort_unstable_by_key(|m| m.position(true));
            }
        }
        self.active = self.choose_match(rs);
        self.last_selection = rs.selected;
        self.dirty = false;
        #[cfg(test)]
        {
            self.rebuilds += 1;
        }
        true
    }

    fn choose_match(&self, rs: &ResultSet) -> Option<usize> {
        if self.matches.is_empty() {
            return None;
        }
        if let Some(selected) = rs.selected {
            if let Some(index) = self.matches.iter().position(|m| m.cell == selected) {
                return Some(index);
            }
            let row_position = match selected.0 {
                GridRow::Data(row) => rs.order.iter().position(|&value| value == row),
                GridRow::New(row) => {
                    (row < rs.edits.inserted.len()).then_some(rs.order.len() + row)
                }
            };
            if let Some(row_position) = row_position {
                let position = CellMatch {
                    cell: selected,
                    row_position,
                }
                .position(rs.transposed);
                return Some(
                    self.matches
                        .partition_point(|m| m.position(rs.transposed) < position)
                        % self.matches.len(),
                );
            }
        }
        Some(0)
    }

    fn sync_selection(&mut self, rs: &ResultSet) {
        if self.last_selection != rs.selected {
            self.active = self.choose_match(rs);
            self.last_selection = rs.selected;
        }
    }

    fn navigate(&mut self, selected: Option<(GridRow, usize)>, backwards: bool) {
        if self.matches.is_empty() {
            return;
        }
        let current = self
            .active
            .filter(|&index| Some(self.matches[index].cell) == selected);
        self.active = Some(match current {
            Some(index) if backwards => (index + self.matches.len() - 1) % self.matches.len(),
            Some(index) => (index + 1) % self.matches.len(),
            None if backwards => {
                (self.active.unwrap_or(0) + self.matches.len() - 1) % self.matches.len()
            }
            None => self.active.unwrap_or(0),
        });
    }
}

impl ResultSet {
    pub(super) fn open_find(&mut self) {
        self.commit_find_buffer();
        if let Some((row, column)) = self.valid_selection() {
            self.find.query = search_text(self.cell(row, column)).into_owned();
        }
        self.find.visible = true;
        self.find.focus_query = true;
        self.find.invalidate();
    }

    /// Search may hide the edited row before its widget can report lost focus.
    /// Commit only in response to a search action, never on ordinary repaints.
    pub(in crate::ui) fn commit_find_buffer(&mut self) -> bool {
        let Some((row, column, text)) = self.edits.editing.take() else {
            return false;
        };
        self.edits.editing_needs_focus = false;
        let valid_row = match row {
            GridRow::Data(index) => index < self.data.rows.len(),
            GridRow::New(index) => index < self.edits.inserted.len(),
        };
        if valid_row && column < self.data.columns.len() {
            if !self.set_cell(row, column, CellEdit::Text(text.clone())) {
                self.edits.editing = Some((row, column, text));
                self.edits.editing_needs_focus = true;
                return false;
            }
        }
        true
    }

    /// Carry controls across a fetched-page replacement, never old row indexes.
    pub fn take_find_from(&mut self, old: &mut Self) {
        self.find = FindState {
            visible: old.find.visible,
            query: std::mem::take(&mut old.find.query),
            match_case: old.find.match_case,
            whole_words: old.find.whole_words,
            filter_rows: old.find.filter_rows,
            ..Default::default()
        };
    }

    pub(super) fn find_bar(&mut self, ui: &mut Ui, focus_id: Id) {
        let input_id = focus_id.with("find_current_page");
        let focused = ui.memory(|memory| memory.focused());
        let owns_focus = focused == Some(focus_id) || self.find.owns_focus(ui, input_id);
        if owns_focus && ui.input_mut(|input| input.consume_shortcut(&FIND)) {
            self.open_find();
        }
        let mut state = std::mem::take(&mut self.find);
        if !state.visible {
            self.find = state;
            return;
        }
        let mut previous = owns_focus && ui.input_mut(|input| input.consume_shortcut(&PREVIOUS));
        let mut next =
            owns_focus && !previous && ui.input_mut(|input| input.consume_shortcut(&NEXT));
        // Escape can clear egui focus before widgets run. Only use last-frame
        // ownership in that case, never dismiss another field's search bar.
        let lost_our_focus = focused.is_none()
            && ui.memory(|memory| {
                memory.had_focus_last_frame(focus_id)
                    || memory.had_focus_last_frame(input_id)
                    || state
                        .focusable_buttons
                        .iter()
                        .any(|id| memory.had_focus_last_frame(*id))
            });
        let mut close = (owns_focus || lost_our_focus)
            && ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape));
        if focused == Some(input_id) {
            previous |= ui.input_mut(|input| input.consume_key(Modifiers::SHIFT, Key::Enter));
            next |=
                !previous && ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Enter));
        }
        let mut changed = state.refresh(self);
        state.sync_selection(self);
        let mut select_match = state.focus_query;
        let mut filter_changed = false;
        let filtered_before = state.filtered_rows().is_some();
        state.focusable_buttons.clear();
        egui::Frame::new()
            .fill(theme::BG_PANEL)
            .inner_margin(egui::Margin::symmetric(6, 4))
            .show(ui, |ui| {
                // An overflowing toolbar can widen the parent's layout. Keep
                // search controls within the actual visible viewport.
                let width = ui
                    .available_width()
                    .min(ui.clip_rect().right() - ui.cursor().left());
                ui.set_width(width.max(0.0));
                ui.spacing_mut().item_spacing.x = 4.0;
                let wide = ui.available_width() >= 800.0;
                let field_width =
                    (ui.available_width() - if wide { 515.0 } else { 315.0 }).max(80.0);
                ui.horizontal(|ui| {
                    let field = ui.add(
                        egui::TextEdit::singleline(&mut state.query)
                            .id(input_id)
                            .desired_width(field_width)
                            .hint_text("Find on Current Page"),
                    );
                    field.widget_info(|| {
                        let mut info =
                            egui::WidgetInfo::text_edit(true, &state.query, &state.query, "");
                        info.label = Some("Find on Current Page".into());
                        info
                    });
                    if state.focus_query {
                        focus_and_select_all(ui, &field, &state.query);
                        state.focus_query = false;
                    }
                    if field.changed() {
                        state.invalidate();
                        select_match = true;
                    }
                    for (symbol, label, selected) in [
                        ("Cc", "Match Case", &mut state.match_case),
                        ("W", "Words", &mut state.whole_words),
                    ] {
                        let button = icon_button(ui, symbol, label, true, *selected);
                        state.focusable_buttons.push(button.id);
                        if button.clicked() {
                            *selected = !*selected;
                            state.dirty = true;
                            select_match = true;
                        }
                    }
                    let count = if state.query.is_empty() {
                        String::new()
                    } else if state.matches.is_empty() {
                        "No matches on this page".into()
                    } else {
                        format!(
                            "{} of {} cells",
                            state.active.map_or(1, |i| i + 1),
                            state.matches.len()
                        )
                    };
                    ui.add_sized(
                        [132.0, 24.0],
                        egui::Label::new(RichText::new(count).small().color(theme::TEXT_MUTED)),
                    );
                    for (symbol, label, request) in [
                        ("↑", "Previous Matching Cell", &mut previous),
                        ("↓", "Next Matching Cell", &mut next),
                    ] {
                        let button =
                            icon_button(ui, symbol, label, !state.matches.is_empty(), false);
                        state.focusable_buttons.push(button.id);
                        *request |= button.clicked();
                    }
                    if wide {
                        filter_changed |= filter_controls(ui, &mut state);
                    }
                    let button = icon_button(ui, "×", "Close Grid Find", true, false);
                    state.focusable_buttons.push(button.id);
                    close |= button.clicked();
                });
                if !wide {
                    filter_changed |= ui.horizontal(|ui| filter_controls(ui, &mut state)).inner;
                }
            });
        if (select_match || filter_changed || previous || next) && self.commit_find_buffer() {
            state.invalidate();
        }
        changed |= state.refresh(self);
        if select_match {
            state.active = state.choose_match(self);
        }
        if previous || next {
            state.navigate(self.selected, previous);
            select_match = true;
        }
        if select_match && let Some(index) = state.active {
            self.select_cell(state.matches[index].cell, false);
            self.pending_reveal = true;
            ui.ctx().request_repaint();
        }
        if close {
            state.visible = false;
            ui.memory_mut(|memory| memory.request_focus(focus_id));
            self.pending_reveal = true;
        }
        if changed || filtered_before != state.filtered_rows().is_some() {
            self.layout.clear_order();
            if filtered_before || state.filtered_rows().is_some() {
                self.invalidate_selection_order();
            }
            if state.filtered_rows().is_some() {
                let selection_visible = self
                    .selected
                    .is_some_and(|(row, _)| state.filtered_index(row).is_some());
                if !selection_visible {
                    self.selected =
                        state
                            .active
                            .map(|index| state.matches[index].cell)
                            .or_else(|| {
                                state
                                    .filtered
                                    .first()
                                    .copied()
                                    .filter(|_| !self.data.columns.is_empty())
                                    .map(|row| (row, 0))
                            });
                }
            }
            self.pending_reveal |= self.selected.is_some();
            ui.ctx().request_repaint();
        }
        state.last_selection = self.selected;
        self.find = state;
        theme::hairline(ui);
    }
}

fn filter_controls(ui: &mut Ui, state: &mut FindState) -> bool {
    let filter = ui.checkbox(&mut state.filter_rows, "Filter rows");
    state.focusable_buttons.push(filter.id);
    ui.label(RichText::new("Current page only").small().color(theme::TEXT_MUTED))
        .on_hover_text("Searches fetched values and pending edits. Newly added rows stay visible. Binary values use their displayed hexadecimal prefix.");
    filter.changed()
}

fn search_text(shown: Shown<'_>) -> Cow<'_, str> {
    match shown {
        Shown::Value(Value::Text(text) | Value::Decimal(text))
        | Shown::Edit(CellEdit::Text(text)) => Cow::Borrowed(text),
        Shown::Value(Value::Null) | Shown::Edit(CellEdit::Null) => Cow::Borrowed("<null>"),
        Shown::Edit(CellEdit::Default) => Cow::Borrowed("<default>"),
        Shown::Value(Value::Bool(true)) => Cow::Borrowed("true"),
        Shown::Value(Value::Bool(false)) => Cow::Borrowed("false"),
        Shown::Value(value) => Cow::Owned(value.to_cell()),
    }
}

fn focus_and_select_all(ui: &Ui, response: &egui::Response, text: &str) {
    let mut state = egui::TextEdit::load_state(ui.ctx(), response.id).unwrap_or_default();
    state
        .cursor
        .set_char_range(Some(egui::text::CCursorRange::two(
            egui::text::CCursor::new(0),
            egui::text::CCursor::new(text.chars().count()),
        )));
    state.store(ui.ctx(), response.id);
    response.request_focus();
    ui.ctx().request_repaint();
}

fn icon_button(
    ui: &mut Ui,
    symbol: &str,
    label: &str,
    enabled: bool,
    selected: bool,
) -> egui::Response {
    let response = ui.add_enabled(
        enabled,
        egui::Button::new(symbol)
            .selected(selected)
            .frame_when_inactive(false)
            .stroke(egui::Stroke::NONE)
            .min_size(egui::vec2(26.0, 24.0)),
    );
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Button, enabled, selected, label)
    });
    response.on_hover_text(label)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Column, Page, QueryResult};

    fn result() -> ResultSet {
        ResultSet::new(
            "find".into(),
            String::new(),
            QueryResult {
                columns: vec![
                    Column {
                        name: "a".into(),
                        type_name: "TEXT".into(),
                    },
                    Column {
                        name: "b".into(),
                        type_name: "TEXT".into(),
                    },
                ],
                rows: vec![
                    vec![Value::Text("Łódź".into()), Value::Null],
                    vec![Value::Text("ŻÓŁĆ".into()), Value::Text("łódź_1".into())],
                    vec![Value::Text("Warsaw".into()), Value::Text("ŁÓDŹ".into())],
                ],
                ..Default::default()
            },
            Page::first(500),
        )
        .with_test_table()
    }

    fn search(rs: &ResultSet, query: &str) -> FindState {
        let mut state = FindState {
            visible: true,
            query: query.into(),
            ..Default::default()
        };
        assert!(state.refresh(rs));
        state
    }

    #[test]
    fn caches_searches_until_invalidated_and_skips_closed_searches() {
        let mut rs = result();
        let mut state = search(&rs, "łódź");
        assert_eq!(state.matches.len(), 3);
        let allocation = state.matches.as_ptr();
        for _ in 0..100 {
            assert!(!state.refresh(&rs));
        }
        assert_eq!(state.rebuilds, 1);
        assert_eq!(allocation, state.matches.as_ptr());
        rs.edits
            .changed
            .insert((0, 0), CellEdit::Text("other".into()));
        state.invalidate();
        assert!(state.refresh(&rs));
        assert_eq!(state.matches.len(), 2);
        state.visible = false;
        state.invalidate();
        assert!(!state.refresh(&rs));
        assert!(state.dirty);
    }

    #[test]
    fn indexes_pending_edits_nulls_deleted_rows_and_full_unicode_text() {
        let mut rs = result();
        rs.edits.deleted.insert(1);
        rs.edits
            .changed
            .insert((0, 0), CellEdit::Text("pending ŻÓŁĆ".into()));
        let state = search(&rs, "żółć");
        assert!(state.matches_cell(GridRow::Data(0), 0));
        assert!(state.matches_cell(GridRow::Data(1), 0));
        assert_eq!(state.matches.len(), 2);
        assert_eq!(search(&rs, "<null>").matches[0].cell, (GridRow::Data(0), 1));
        rs.edits
            .changed
            .insert((0, 0), CellEdit::Text(format!("{}needle", "x".repeat(500))));
        assert!(search(&rs, "needle").matches_cell(GridRow::Data(0), 0));
    }

    #[test]
    fn filtering_preserves_all_new_rows_and_empty_query_restores_all() {
        let mut rs = result();
        rs.edits
            .inserted
            .push(vec![CellEdit::Default, CellEdit::Text("new".into())]);
        let mut state = search(&rs, "Warsaw");
        state.filter_rows = true;
        assert_eq!(
            state.filtered_rows(),
            Some([GridRow::Data(2), GridRow::New(0)].as_slice())
        );
        assert_eq!(state.filtered_index(GridRow::New(0)), Some(1));
        assert_eq!(state.matches.len(), 1);
        assert!(!state.matches_cell(GridRow::New(0), 0));
        let defaults = search(&rs, "<default>");
        assert_eq!(defaults.matches[0].cell, (GridRow::New(0), 0));
        state.query.clear();
        state.invalidate();
        state.refresh(&rs);
        assert!(state.filtered_rows().is_none());
        assert!(state.matches.is_empty());
    }

    #[test]
    fn case_words_sort_and_transpose_follow_display_order() {
        let mut rs = result();
        let mut state = search(&rs, "łódź");
        state.whole_words = true;
        state.invalidate();
        state.refresh(&rs);
        assert_eq!(state.matches.len(), 2);
        state.match_case = true;
        state.invalidate();
        state.refresh(&rs);
        assert!(state.matches.is_empty());
        state.match_case = false;
        state.whole_words = false;
        rs.order.reverse();
        rs.selected = Some((GridRow::Data(0), 0));
        state.invalidate();
        state.refresh(&rs);
        assert_eq!(state.matches[0].cell, (GridRow::Data(2), 1));
        assert_eq!(state.active, Some(2));
        rs.transposed = true;
        assert!(state.refresh(&rs));
        assert_eq!(state.matches[0].cell, (GridRow::Data(0), 0));
        assert_eq!(state.matches[1].cell, (GridRow::Data(2), 1));
        assert_eq!(state.active, Some(0));
        state.navigate(rs.selected, true);
        assert_eq!(state.active, Some(2));
    }

    #[test]
    fn page_replacement_keeps_controls_but_drops_old_indexes() {
        let mut old = result();
        old.find = search(&old, "łódź");
        old.find.filter_rows = true;
        let mut next = result();
        std::sync::Arc::make_mut(&mut next.data).rows.clear();
        next.order.clear();
        next.take_find_from(&mut old);
        assert!(next.find.visible && next.find.filter_rows && next.find.dirty);
        assert_eq!(next.find.query, "łódź");
        assert!(next.find.matches.is_empty());
        assert!(next.find.filtered_positions.is_empty());
    }

    #[test]
    fn navigation_tracks_manual_selection_without_rebuilding_cells() {
        let mut rs = result();
        let mut state = search(&rs, "łódź");
        rs.selected = Some((GridRow::Data(1), 1));
        state.sync_selection(&rs);
        assert_eq!(state.active, Some(1));
        state.navigate(rs.selected, false);
        assert_eq!(
            state.matches[state.active.unwrap()].cell,
            (GridRow::Data(2), 1)
        );
        rs.selected = Some((GridRow::Data(1), 0));
        state.sync_selection(&rs);
        state.navigate(rs.selected, true);
        assert_eq!(
            state.matches[state.active.unwrap()].cell,
            (GridRow::Data(0), 0)
        );
        assert_eq!(state.rebuilds, 1);
    }

    #[test]
    fn search_commits_active_buffer_before_prefill_or_hiding_a_row() {
        let mut rs = result();
        rs.selected = Some((GridRow::Data(0), 0));
        rs.edits.editing = Some((GridRow::Data(0), 0, "new ŁÓDŹ".into()));
        rs.open_find();
        assert!(rs.edits.editing.is_none());
        assert_eq!(rs.find.query, "new ŁÓDŹ");
        assert_eq!(search_text(rs.cell(GridRow::Data(0), 0)), "new ŁÓDŹ");
        let mut state = search(&rs, "łódź");
        state.filter_rows = true;
        rs.edits.editing = Some((GridRow::Data(0), 0, "Paris".into()));
        assert!(
            !state.refresh(&rs),
            "ordinary repaint does not consume the editor buffer"
        );
        assert!(rs.edits.editing.is_some());
        assert!(rs.commit_find_buffer());
        state.invalidate();
        state.refresh(&rs);
        assert!(state.filtered_index(GridRow::Data(0)).is_none());
        assert!(rs.edits.editing.is_none());
        assert_eq!(search_text(rs.cell(GridRow::Data(0), 0)), "Paris");
    }

    #[test]
    fn escape_closes_only_the_search_that_owned_focus() {
        let context = egui::Context::default();
        let grid_id = Id::new("test_find_grid");
        let unrelated_id = Id::new("test_other_field");
        let mut rs = result();
        rs.find.visible = true;
        let mut frame = |focus: Option<Id>, escape: bool| {
            let mut input = egui::RawInput::default();
            if escape {
                input.events.push(egui::Event::Key {
                    key: Key::Escape,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                });
            }
            let mut output = context.run_ui(input, |ui| {
                rs.find_bar(ui, grid_id);
                for id in [grid_id, unrelated_id] {
                    let response = ui.interact(
                        egui::Rect::from_min_size(ui.cursor().min, egui::vec2(20.0, 20.0)),
                        id,
                        egui::Sense::click(),
                    );
                    if focus == Some(id) {
                        response.request_focus();
                    }
                }
            });
            output.textures_delta.clear();
            rs.find.visible
        };
        assert!(frame(Some(unrelated_id), false));
        assert!(
            frame(None, true),
            "Escape in an unrelated field must keep grid Find open"
        );
        assert!(frame(Some(grid_id), false));
        assert!(
            !frame(None, true),
            "Escape from the grid dismisses its search"
        );
    }

    #[test]
    #[ignore = "manual release benchmark of search index and cached lookups"]
    fn benchmark_page_find_cache() {
        use std::hint::black_box;
        use std::time::Instant;
        for count in [500, 5_000, 100_000] {
            let mut rs = result();
            std::sync::Arc::make_mut(&mut rs.data).rows = (0..count)
                .map(|row| {
                    vec![
                        Value::Int(row as i64),
                        Value::Text(if row % 10 == 0 {
                            format!("Łódź row {row}")
                        } else {
                            format!("Warsaw row {row}")
                        }),
                    ]
                })
                .collect();
            rs.order = (0..count).collect();
            let mut state = FindState {
                visible: true,
                query: "łódź".into(),
                filter_rows: true,
                ..Default::default()
            };
            let start = Instant::now();
            state.refresh(black_box(&rs));
            let rebuild = start.elapsed();
            let iterations = 100_000;
            let start = Instant::now();
            for iteration in 0..iterations {
                black_box(black_box(&mut state).refresh(black_box(&rs)));
                let rows = black_box(state.filtered_rows().unwrap());
                let row = rows[iteration % rows.len()];
                black_box(state.filtered_index(black_box(row)));
                black_box(state.matches_cell(black_box(row), black_box(1)));
            }
            let cached = start.elapsed();
            assert_eq!(state.rebuilds, 1);
            println!(
                "grid_find rows={count} rebuild_us={:.2} matches={} cached_refresh_and_lookups_ns={:.2}",
                rebuild.as_secs_f64() * 1e6,
                state.matches.len(),
                cached.as_secs_f64() * 1e9 / iterations as f64
            );
        }
    }
}
