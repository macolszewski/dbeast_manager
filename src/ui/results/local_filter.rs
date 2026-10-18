//! Exact value filters for fetched rows. No database query is executed here.

use super::{GridRow, ResultSet, theme};
use crate::filter::{FilterValue, SelectedValues};
use crate::search::{SearchOptions, SearchPattern};
use egui::{Id, Key, Modifiers, Rect, RichText, Ui};
use std::collections::HashMap;

pub(super) struct LocalFilterState {
    pub(super) enabled: bool,
    conditions: Vec<Option<SelectedValues>>,
    active_columns: Vec<usize>,
    dirty: bool,
    rows: Vec<GridRow>,
    positions: HashMap<GridRow, usize>,
    popup: Option<ValuePopup>,
    #[cfg(test)]
    rebuilds: usize,
}

impl Default for LocalFilterState {
    fn default() -> Self {
        Self {
            enabled: false,
            conditions: Vec::new(),
            active_columns: Vec::new(),
            dirty: true,
            rows: Vec::new(),
            positions: HashMap::new(),
            popup: None,
            #[cfg(test)]
            rebuilds: 0,
        }
    }
}

impl LocalFilterState {
    pub(super) fn has_conditions(&self) -> bool {
        self.conditions.iter().any(Option::is_some)
    }

    pub(super) fn invalidate(&mut self) {
        self.dirty = true;
        if let Some(popup) = &mut self.popup {
            popup.choices_dirty = true;
        }
    }

    pub(super) fn filtered_rows(&self) -> Option<&[GridRow]> {
        (self.enabled && !self.active_columns.is_empty()).then_some(self.rows.as_slice())
    }

    pub(super) fn filtered_index(&self, row: GridRow) -> Option<usize> {
        self.positions.get(&row).copied()
    }

    pub(super) fn accepts(&self, row: GridRow) -> bool {
        self.filtered_rows().is_none() || self.positions.contains_key(&row)
    }

    pub(super) fn column_active(&self, column: usize) -> bool {
        self.conditions.get(column).is_some_and(Option::is_some)
    }

    fn ensure_columns(&mut self, count: usize) {
        self.conditions.resize_with(count, || None);
    }

    /// Rebuild only after values, conditions, order or the enabled state change.
    fn refresh(&mut self, rs: &ResultSet) -> bool {
        if !self.dirty {
            return false;
        }
        self.ensure_columns(rs.data.columns.len());
        self.active_columns.clear();
        self.active_columns.extend(
            self.conditions
                .iter()
                .enumerate()
                .filter_map(|(column, values)| values.as_ref().map(|_| column)),
        );
        self.rows.clear();
        self.positions.clear();
        if self.enabled && !self.active_columns.is_empty() {
            for &index in &rs.order {
                let row = GridRow::Data(index);
                if self.active_columns.iter().all(|&column| {
                    self.conditions[column]
                        .as_ref()
                        .unwrap()
                        .contains(&cell_key(rs, row, column))
                }) {
                    self.positions.insert(row, self.rows.len());
                    self.rows.push(row);
                }
            }
            // Unfinished inserted rows remain available for typing, regardless
            // of filter conditions. Deleted fetched rows still match normally.
            for index in 0..rs.edits.inserted.len() {
                let row = GridRow::New(index);
                self.positions.insert(row, self.rows.len());
                self.rows.push(row);
            }
        }
        self.dirty = false;
        #[cfg(test)]
        {
            self.rebuilds += 1;
        }
        true
    }

    /// The first change of a column turns "all values" into an explicit set of
    /// the listed values, so values that appear only on a later page stay
    /// excluded. Retained values are shared, page values are copied once.
    fn set_choice(
        &mut self,
        rs: &ResultSet,
        column: usize,
        value: &FilterValue<'_>,
        selected: bool,
        choices: &[Choice],
    ) {
        let selection = self.conditions[column].get_or_insert_with(|| {
            let mut selection = SelectedValues::default();
            for choice in choices {
                match &choice.value {
                    ChoiceValue::Cell(row) => selection.insert(cell_key(rs, *row, column)),
                    ChoiceValue::Retained(value) => selection.insert_shared(value.clone()),
                };
            }
            selection
        });
        if selected {
            if !selection.contains(value) {
                selection.insert(value.clone());
            }
        } else {
            selection.remove(value);
        }
        self.dirty = true;
    }
}

/// Where a listed value is read from: a cell of the page, or a selected value
/// that is not on this page. Neither copies the value.
enum ChoiceValue {
    Cell(GridRow),
    Retained(std::sync::Arc<FilterValue<'static>>),
}

struct Choice {
    value: ChoiceValue,
    label: String,
    count: usize,
    hover: std::cell::OnceCell<String>,
}

struct ValuePopup {
    column: usize,
    anchor: Rect,
    query: String,
    pattern: Option<SearchPattern>,
    pattern_dirty: bool,
    query_dirty: bool,
    choices: Vec<Choice>,
    choices_dirty: bool,
    shown: Vec<usize>,
    selected_shown: usize,
    selection_dirty: bool,
    focus_search: bool,
}

impl Choice {
    fn key<'a>(&'a self, rs: &'a ResultSet, column: usize) -> FilterValue<'a> {
        match &self.value {
            ChoiceValue::Cell(row) => cell_key(rs, *row, column),
            ChoiceValue::Retained(value) => value.as_borrowed(),
        }
    }
}

impl ValuePopup {
    fn new(column: usize, anchor: Rect) -> Self {
        Self {
            column,
            anchor,
            query: String::new(),
            pattern: None,
            pattern_dirty: true,
            query_dirty: true,
            choices: Vec::new(),
            choices_dirty: true,
            shown: Vec::new(),
            selected_shown: 0,
            selection_dirty: true,
            focus_search: true,
        }
    }

    /// Read the entire fetched page, including rows hidden by other columns.
    /// Only the opened column owns a distinct-value list and its display labels.
    fn refresh_choices(&mut self, rs: &ResultSet, selected: Option<&SelectedValues>) {
        if !self.choices_dirty {
            return;
        }
        let mut counts = HashMap::<FilterValue<'_>, (usize, GridRow)>::new();
        for row in (0..rs.data.rows.len())
            .map(GridRow::Data)
            .chain((0..rs.edits.inserted.len()).map(GridRow::New))
        {
            counts.entry(cell_key(rs, row, self.column)).or_insert((0, row)).0 += 1;
        }
        self.choices.clear();
        if let Some(selected) = selected {
            for value in selected.iter_shared() {
                if !counts.contains_key(value.as_ref()) {
                    self.choices.push(Choice {
                        label: value.display_label(),
                        value: ChoiceValue::Retained(value.clone()),
                        count: 0,
                        hover: std::cell::OnceCell::new(),
                    });
                }
            }
        }
        self.choices
            .extend(counts.into_iter().map(|(value, (count, row))| Choice {
                label: value.display_label(),
                value: ChoiceValue::Cell(row),
                count,
                hover: std::cell::OnceCell::new(),
            }));
        self.choices.sort_by(|a, b| a.label.cmp(&b.label));
        self.choices_dirty = false;
        self.query_dirty = true;
        self.selection_dirty = true;
    }

    fn refresh_query(&mut self, rs: &ResultSet, selected: Option<&SelectedValues>) {
        if self.pattern_dirty {
            self.pattern = (!self.query.is_empty())
                .then(|| SearchPattern::new(&self.query, SearchOptions::default()));
            self.pattern_dirty = false;
        }
        if self.query_dirty {
            self.shown.clear();
            self.shown.extend(
                self.choices
                    .iter()
                    .enumerate()
                    .filter_map(|(index, choice)| {
                        self.pattern
                            .as_ref()
                            .is_none_or(|pattern| {
                                pattern.is_match(&choice.label)
                                    || match &choice.key(rs, self.column) {
                                        FilterValue::Text(text) | FilterValue::Decimal(text) => {
                                            pattern.is_match(text)
                                        }
                                        _ => false,
                                    }
                            })
                            .then_some(index)
                    }),
            );
            self.query_dirty = false;
            self.selection_dirty = true;
        }
        if self.selection_dirty {
            self.selected_shown = self
                .shown
                .iter()
                .filter(|&&index| {
                    selected.is_none_or(|values| {
                        values.contains(&self.choices[index].key(rs, self.column))
                    })
                })
                .count();
            self.selection_dirty = false;
        }
    }
}

impl ResultSet {
    pub(super) fn toggle_local_filter(&mut self) {
        self.commit_find_buffer();
        self.invalidate_selection_order();
        self.pending_grid_focus = true;
        self.local_filter.enabled = !self.local_filter.enabled;
        if !self.local_filter.enabled {
            self.local_filter.popup = None;
        }
        self.local_filter.invalidate();
    }

    pub(super) fn clear_local_filters(&mut self) {
        self.commit_find_buffer();
        self.pending_grid_focus = true;
        for condition in &mut self.local_filter.conditions {
            *condition = None;
        }
        self.local_filter.invalidate();
        if let Some(popup) = &mut self.local_filter.popup {
            popup.selection_dirty = true;
        }
    }

    pub(super) fn open_local_filter(&mut self, column: usize, anchor: Rect) {
        if column >= self.data.columns.len() {
            return;
        }
        self.commit_find_buffer();
        self.local_filter.enabled = true;
        self.local_filter.ensure_columns(self.data.columns.len());
        self.local_filter.popup = Some(ValuePopup::new(column, anchor));
        self.local_filter.dirty = true;
    }

    /// Preserve conditions by column name and database type, not stale indexes.
    pub fn take_local_filters_from(&mut self, old: &mut Self) {
        let mut next = LocalFilterState {
            enabled: old.local_filter.enabled,
            ..Default::default()
        };
        next.ensure_columns(self.data.columns.len());
        for (column, meta) in self.data.columns.iter().enumerate() {
            let same_signature = |old: &crate::model::Column| {
                old.name == meta.name && old.type_name == meta.type_name
            };
            if old.data.columns.get(column).is_some_and(same_signature) {
                if let Some(condition) = old.local_filter.conditions.get_mut(column) {
                    next.conditions[column] = condition.take();
                }
                continue;
            }
            if self
                .data
                .columns
                .iter()
                .filter(|meta| same_signature(meta))
                .count()
                != 1
            {
                continue;
            }
            let mut matching = old
                .data
                .columns
                .iter()
                .enumerate()
                .filter(|(_, old)| old.name == meta.name && old.type_name == meta.type_name);
            if let Some((previous, _)) = matching.next()
                && matching.next().is_none()
                && let Some(condition) = old.local_filter.conditions.get_mut(previous)
            {
                next.conditions[column] = condition.take();
            }
        }
        self.local_filter = next;
    }

    pub(super) fn local_filter_ui(&mut self, ui: &mut Ui, focus_id: Id) {
        let filtered_before = self.local_filter.filtered_rows().is_some();
        let mut state = std::mem::take(&mut self.local_filter);
        let mut changed = state.refresh(self);
        if let Some(mut popup) = state
            .popup
            .take()
            .filter(|popup| popup.column < self.data.columns.len())
        {
            popup.refresh_choices(self, state.conditions[popup.column].as_ref());
            popup.refresh_query(self, state.conditions[popup.column].as_ref());
            let escaped = ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape));
            let mut open = !escaped;
            let mut filter_action = false;
            let popup_id = focus_id.with(("local_value_filter", popup.column));
            egui::Popup::new(popup_id, ui.ctx().clone(), popup.anchor, ui.layer_id())
                .open_bool(&mut open)
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                .width(420.0)
                .show(|ui| {
                    ui.set_width(404.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            RichText::new(format!(
                                "Local Filter For '{}'",
                                self.data.columns[popup.column].name
                            ))
                            .strong(),
                        );
                    });
                    ui.separator();
                    let search = ui.add(
                        egui::TextEdit::singleline(&mut popup.query)
                            .id(popup_id.with("search"))
                            .desired_width(f32::INFINITY)
                            .hint_text("Search Filter Values"),
                    );
                    search.widget_info(|| {
                        let mut info =
                            egui::WidgetInfo::text_edit(true, &popup.query, &popup.query, "");
                        info.label = Some("Search Filter Values".into());
                        info
                    });
                    if popup.focus_search {
                        search.request_focus();
                        popup.focus_search = false;
                    }
                    if search.changed() {
                        popup.query_dirty = true;
                        popup.pattern_dirty = true;
                    }
                    popup.refresh_query(self, state.conditions[popup.column].as_ref());
                    ui.horizontal(|ui| {
                        let mut all =
                            !popup.shown.is_empty() && popup.selected_shown == popup.shown.len();
                        let partial = popup.selected_shown > 0 && !all;
                        let master = value_checkbox(ui, &mut all, "Value", partial);
                        if master.changed() {
                            for &index in &popup.shown {
                                state.set_choice(
                                    self,
                                    popup.column,
                                    &popup.choices[index].key(self, popup.column),
                                    all,
                                    &popup.choices,
                                );
                            }
                            popup.selection_dirty = true;
                            filter_action = true;
                        }
                        ui.label(RichText::new("Count").color(theme::TEXT_MUTED));
                    });
                    ui.separator();
                    if popup.shown.is_empty() {
                        ui.label(RichText::new("No values found").color(theme::TEXT_MUTED));
                    }
                    egui::ScrollArea::vertical()
                        .id_salt(popup_id.with("values"))
                        .max_height(240.0)
                        .auto_shrink([false, true])
                        .show_rows(ui, 24.0, popup.shown.len(), |ui, rows| {
                            for row in rows {
                                let choice = &popup.choices[popup.shown[row]];
                                ui.push_id(row, |ui| {
                                    ui.horizontal(|ui| {
                                        let mut selected = state.conditions[popup.column]
                                            .as_ref()
                                            .is_none_or(|values| {
                                                values.contains(&choice.key(self, popup.column))
                                            });
                                        let checkbox =
                                            value_checkbox(ui, &mut selected, &choice.label, false);
                                        checkbox.widget_info(|| {
                                            egui::WidgetInfo::selected(
                                                egui::WidgetType::Checkbox,
                                                true,
                                                selected,
                                                format!("Filter value: {}", choice.label),
                                            )
                                        });
                                        let checkbox =
                                            checkbox.on_hover_ui(|ui| {
                                                full_value_hover(ui, choice, self, popup.column)
                                            });
                                        if checkbox.changed() {
                                            state.set_choice(
                                                self,
                                                popup.column,
                                                &choice.key(self, popup.column),
                                                selected,
                                                &popup.choices,
                                            );
                                            popup.selection_dirty = true;
                                            filter_action = true;
                                        }
                                        ui.label(
                                            RichText::new(choice.count.to_string())
                                                .color(theme::TEXT_MUTED),
                                        );
                                    });
                                });
                            }
                        });
                    ui.separator();
                    ui.horizontal(|ui| {
                        if ui.button("All Values").clicked() {
                            state.conditions[popup.column] = None;
                            state.dirty = true;
                            popup.selection_dirty = true;
                            filter_action = true;
                        }
                        ui.label(
                            RichText::new("Current page only")
                                .small()
                                .color(theme::TEXT_MUTED),
                        );
                    });
                });
            if filter_action && self.commit_find_buffer() {
                state.dirty = true;
                popup.choices_dirty = true;
            }
            if filter_action {
                ui.ctx().request_repaint();
            }
            if open {
                state.popup = Some(popup);
            }
            if escaped {
                ui.memory_mut(|memory| memory.request_focus(focus_id));
            }
            changed |= state.refresh(self);
        }
        if changed {
            self.layout.clear_order();
            if filtered_before || state.filtered_rows().is_some() {
                self.invalidate_selection_order();
            }
            self.find.invalidate();
            if let Some(rows) = state.filtered_rows()
                && !self.selected.is_some_and(|(row, column)| {
                    column < self.data.columns.len() && state.filtered_index(row).is_some()
                })
            {
                self.selected = rows
                    .first()
                    .copied()
                    .filter(|_| !self.data.columns.is_empty())
                    .map(|row| (row, 0));
            }
            self.pending_reveal |= self.selected.is_some();
            ui.ctx().request_repaint();
        }
        self.local_filter = state;
    }
}

fn value_checkbox(ui: &mut Ui, selected: &mut bool, label: &str, partial: bool) -> egui::Response {
    let width = (ui.available_width() - 64.0).max(40.0);
    ui.allocate_ui_with_layout(
        egui::vec2(width, 24.0),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_min_width(width);
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
            ui.add(egui::Checkbox::new(selected, label).indeterminate(partial))
        },
    )
    .inner
}

fn cell_key(rs: &ResultSet, row: GridRow, column: usize) -> FilterValue<'_> {
    match row {
        GridRow::Data(index) => match rs.edits.changed.get(&(index, column)) {
            Some(edit) => FilterValue::from_edited_value(&rs.data.rows[index][column], edit),
            None => FilterValue::from_value(&rs.data.rows[index][column]),
        },
        GridRow::New(index) => FilterValue::from_edit(&rs.edits.inserted[index][column]),
    }
}

/// Draw a funnel shape from vector paths, without image resources.
pub(super) fn header_button(ui: &mut Ui, rs: &ResultSet, column: usize) -> Option<Rect> {
    if !rs.local_filter.enabled {
        return None;
    }
    let (rect, response) = ui.allocate_exact_size(egui::vec2(20.0, 24.0), egui::Sense::click());
    let label = format!("Local Filter: {}", rs.data.columns[column].name);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label));
    let active = rs.local_filter.column_active(column);
    if response.hovered() || response.has_focus() {
        ui.painter().rect_filled(rect, 3.0, theme::BG_HOVER);
    }
    // The funnel shows when it filters or under the pointer, not on every header.
    if !active && !ui.ui_contains_pointer() && !response.has_focus() {
        return response.clicked().then_some(rect);
    }
    let origin = rect.center();
    let points = [
        (-7.0, -6.0),
        (7.0, -6.0),
        (2.0, 0.0),
        (2.0, 6.0),
        (-2.0, 8.0),
        (-2.0, 0.0),
    ]
    .into_iter()
    .map(|(x, y)| origin + egui::vec2(x, y))
    .collect();
    ui.painter().add(egui::Shape::closed_line(
        points,
        egui::Stroke::new(1.3, theme::TEXT_MUTED),
    ));
    if active {
        ui.painter()
            .circle_filled(rect.right_top() + egui::vec2(-3.0, 4.0), 3.0, theme::GREEN);
    }
    let clicked = response.clicked();
    response.on_hover_text(label);
    clicked.then_some(rect)
}

fn full_value_hover(ui: &mut Ui, choice: &Choice, rs: &ResultSet, column: usize) {
    ui.set_max_width(600.0);
    let preview = choice
        .hover
        .get_or_init(|| hover_preview(&choice.key(rs, column)));
    egui::ScrollArea::vertical()
        .max_height(240.0)
        .show(ui, |ui| {
            ui.label(preview);
        });
}

fn hover_preview(value: &FilterValue<'_>) -> String {
    const NOTICE: &str = "\n… Preview truncated. Use Show Value Editor for the full value.";
    match value {
        FilterValue::Text(text) | FilterValue::Decimal(text) => {
            let mut chars = text.chars();
            let mut preview: String = chars.by_ref().take(4096).collect();
            if chars.next().is_some() {
                preview.push_str(NOTICE);
            }
            preview
        }
        FilterValue::Bytes(bytes) => {
            use std::fmt::Write;
            let mut preview = String::from("0x");
            for byte in bytes.iter().take(256) {
                write!(preview, "{byte:02X}").unwrap();
            }
            if bytes.len() > 256 {
                write!(preview, " ({} bytes)", bytes.len()).unwrap();
                preview.push_str(NOTICE);
            }
            preview
        }
        _ => value.display_label(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Column, Page, QueryResult, Value};
    use crate::sqlgen::CellEdit;

    fn result() -> ResultSet {
        ResultSet::new(
            "filter".into(),
            String::new(),
            QueryResult {
                columns: vec![
                    Column {
                        name: "city".into(),
                        type_name: "TEXT".into(),
                    },
                    Column {
                        name: "status".into(),
                        type_name: "TEXT".into(),
                    },
                ],
                rows: vec![
                    vec![Value::Text("Łódź".into()), Value::Text("Active".into())],
                    vec![Value::Text("Łódź".into()), Value::Text("Pending".into())],
                    vec![Value::Text("Kraków".into()), Value::Text("Active".into())],
                ],
                ..Default::default()
            },
            Page::first(500),
        )
        .with_test_table()
    }

    fn selected(value: &Value) -> Option<SelectedValues> {
        let mut selection = SelectedValues::default();
        selection.insert(FilterValue::from_value(value));
        Some(selection)
    }

    fn filter(rs: &ResultSet) -> LocalFilterState {
        let mut state = LocalFilterState {
            enabled: true,
            ..Default::default()
        };
        state.ensure_columns(rs.data.columns.len());
        state.conditions[0] = selected(&Value::Text("Łódź".into()));
        state.refresh(rs);
        state
    }

    #[test]
    fn columns_intersect_and_cache_ignores_idle_frames() {
        let rs = result();
        let mut state = filter(&rs);
        assert_eq!(
            state.filtered_rows().unwrap(),
            [GridRow::Data(0), GridRow::Data(1)]
        );
        for _ in 0..100 {
            assert!(!state.refresh(&rs));
        }
        assert_eq!(state.rebuilds, 1);
        state.conditions[1] = selected(&Value::Text("Active".into()));
        state.invalidate();
        state.refresh(&rs);
        assert_eq!(state.filtered_rows().unwrap(), [GridRow::Data(0)]);
        assert_eq!(state.filtered_index(GridRow::Data(0)), Some(0));
        state.enabled = false;
        state.invalidate();
        state.refresh(&rs);
        assert!(state.filtered_rows().is_none());
        state.enabled = true;
        state.invalidate();
        state.refresh(&rs);
        assert_eq!(state.filtered_rows().unwrap(), [GridRow::Data(0)]);
        state.conditions[0] = Some(SelectedValues::default());
        state.invalidate();
        state.refresh(&rs);
        assert!(state.filtered_rows().unwrap().is_empty());
    }

    #[test]
    fn typed_values_pending_changes_deletions_and_insertions() {
        let mut rs = result();
        std::sync::Arc::make_mut(&mut rs.data).rows[0][0] = Value::Null;
        std::sync::Arc::make_mut(&mut rs.data).rows[1][0] = Value::Text("<null>".into());
        let mut state = filter(&rs);
        state.conditions[0] = selected(&Value::Null);
        state.invalidate();
        state.refresh(&rs);
        assert_eq!(state.filtered_rows().unwrap(), [GridRow::Data(0)]);
        rs.edits.deleted.insert(0);
        rs.edits
            .inserted
            .push(vec![CellEdit::Default, CellEdit::Default]);
        state.invalidate();
        state.refresh(&rs);
        assert_eq!(
            state.filtered_rows().unwrap(),
            [GridRow::Data(0), GridRow::New(0)]
        );
        rs.edits
            .changed
            .insert((0, 0), CellEdit::Text("Paris".into()));
        state.invalidate();
        state.refresh(&rs);
        assert_eq!(state.filtered_rows().unwrap(), [GridRow::New(0)]);
    }

    fn big_text_page(prefix: char, rows: usize) -> ResultSet {
        let mut rs = result();
        let data = std::sync::Arc::make_mut(&mut rs.data);
        data.rows = (0..rows)
            .map(|i| {
                vec![
                    Value::Text(format!("{prefix}{i}{}", "x".repeat(1 << 16))),
                    Value::Text("Active".into()),
                ]
            })
            .collect();
        rs.order = (0..rows).collect();
        rs
    }

    #[test]
    fn value_list_reads_page_cells_and_shares_retained_values() {
        let first = big_text_page('a', 50);
        let mut popup = ValuePopup::new(0, Rect::NOTHING);
        popup.refresh_choices(&first, None);
        assert_eq!(popup.choices.len(), 50);
        assert!(
            popup
                .choices
                .iter()
                .all(|choice| matches!(choice.value, ChoiceValue::Cell(_))),
            "the list does not copy page values"
        );
        // Unchecking one value selects the other 49 explicitly.
        let mut state = filter(&first);
        state.conditions[0] = None;
        let excluded = popup.choices[0].key(&first, 0).into_owned();
        state.set_choice(&first, 0, &excluded, false, &popup.choices);
        state.dirty = true;
        state.refresh(&first);
        let selection = state.conditions[0].as_ref().unwrap();
        assert_eq!(selection.len(), 49);
        assert!(!selection.contains(&excluded));
        assert_eq!(state.filtered_rows().unwrap().len(), 49);

        // On another page, values seen only on the first page are listed from
        // the selection itself, and new values stay excluded.
        let mut old = first;
        old.local_filter = state;
        let mut next = big_text_page('b', 10);
        next.take_local_filters_from(&mut old);
        drop(old);
        next.local_filter.dirty = true;
        let mut state = std::mem::take(&mut next.local_filter);
        state.refresh(&next);
        assert_eq!(state.filtered_rows().unwrap().len(), 0);
        let mut popup = ValuePopup::new(0, Rect::NOTHING);
        popup.refresh_choices(&next, state.conditions[0].as_ref());
        assert_eq!(popup.choices.len(), 59);
        let retained: Vec<_> = popup
            .choices
            .iter()
            .filter_map(|choice| match &choice.value {
                ChoiceValue::Retained(value) => Some(std::sync::Arc::downgrade(value)),
                ChoiceValue::Cell(_) => None,
            })
            .collect();
        assert_eq!(retained.len(), 49);
        assert!(retained.iter().all(|value| value.strong_count() == 2));
        // Clearing the filter and the list releases the retained values.
        state.conditions[0] = None;
        drop(popup);
        assert!(retained.iter().all(|value| value.strong_count() == 0));
    }

    #[test]
    fn choices_include_all_page_values_and_retained_zero_counts() {
        let rs = result();
        let mut values = SelectedValues::default();
        values.insert(FilterValue::from_value(&Value::Text("Poznań".into())));
        let mut popup = ValuePopup::new(0, Rect::NOTHING);
        popup.refresh_choices(&rs, Some(&values));
        assert_eq!(popup.choices.len(), 3);
        assert_eq!(
            popup
                .choices
                .iter()
                .find(|c| c.label == "\"Łódź\"")
                .unwrap()
                .count,
            2
        );
        assert_eq!(
            popup
                .choices
                .iter()
                .find(|c| c.label == "\"Poznań\"")
                .unwrap()
                .count,
            0
        );
        popup.query = "łód".into();
        popup.refresh_query(&rs, Some(&values));
        assert_eq!(popup.shown.len(), 1);
        assert_eq!(popup.selected_shown, 0);
        let choices = popup.choices.as_ptr();
        popup.refresh_choices(&rs, Some(&values));
        popup.refresh_query(&rs, Some(&values));
        assert_eq!(choices, popup.choices.as_ptr());
    }

    #[test]
    fn page_refresh_remaps_columns_and_drops_missing_or_changed_types() {
        let mut old = result();
        old.local_filter = filter(&old);
        old.local_filter.conditions[1] = selected(&Value::Text("Active".into()));
        let mut next = result();
        std::sync::Arc::make_mut(&mut next.data).columns.swap(0, 1);
        for row in &mut std::sync::Arc::make_mut(&mut next.data).rows {
            row.swap(0, 1);
        }
        next.take_local_filters_from(&mut old);
        assert!(next.local_filter.column_active(0) && next.local_filter.column_active(1));
        let mut state = std::mem::take(&mut next.local_filter);
        state.refresh(&next);
        assert_eq!(state.filtered_rows().unwrap(), [GridRow::Data(0)]);
        next.local_filter = state;
        let mut changed = result();
        std::sync::Arc::make_mut(&mut changed.data).columns[0].type_name = "INTEGER".into();
        std::sync::Arc::make_mut(&mut changed.data).columns[1].name = "missing".into();
        changed.take_local_filters_from(&mut next);
        assert!(!changed.local_filter.column_active(0) && !changed.local_filter.column_active(1));
    }

    #[test]
    fn page_refresh_keeps_duplicate_column_positions_without_ambiguous_remap() {
        let mut old = result();
        std::sync::Arc::make_mut(&mut old.data).columns[1].name = "city".into();
        old.local_filter = filter(&old);
        old.local_filter.conditions[1] = selected(&Value::Text("Active".into()));
        let mut next = result();
        std::sync::Arc::make_mut(&mut next.data).columns[1].name = "city".into();
        next.take_local_filters_from(&mut old);
        assert!(
            next.local_filter.conditions[0]
                .as_ref()
                .unwrap()
                .contains(&FilterValue::from_value(&Value::Text("Łódź".into())))
        );
        assert!(
            next.local_filter.conditions[1]
                .as_ref()
                .unwrap()
                .contains(&FilterValue::from_value(&Value::Text("Active".into())))
        );

        let mut changed = result();
        std::sync::Arc::make_mut(&mut changed.data)
            .columns
            .push(Column {
                name: "city".into(),
                type_name: "TEXT".into(),
            });
        std::sync::Arc::make_mut(&mut changed.data).columns[0].name = "new".into();
        changed.take_local_filters_from(&mut next);
        assert!(
            !changed.local_filter.column_active(2),
            "moved ambiguous duplicate must not borrow another column's filter"
        );
    }

    #[test]
    fn hover_previews_bound_large_values_without_affecting_identity() {
        let text = Value::Text("ż".repeat(100_000));
        let preview = hover_preview(&FilterValue::from_value(&text));
        assert!(preview.chars().count() < 4200);
        assert!(preview.contains("Show Value Editor"));
        let bytes = Value::Bytes(vec![255; 100_000]);
        let preview = hover_preview(&FilterValue::from_value(&bytes));
        assert!(preview.len() < 650);
        assert!(preview.contains("100000 bytes"));
    }

    #[test]
    fn geo_selection_uses_fetched_bounds_instead_of_grid_filters() {
        let mut rs = result();
        let mut state = filter(&rs);
        state.conditions[1] = selected(&Value::Text("Active".into()));
        state.invalidate();
        state.refresh(&rs);
        rs.local_filter = state;
        rs.selected = Some((GridRow::Data(1), 0));
        assert!(rs.selected_value().is_none());
        rs.geo = Some(super::super::GeoState::new(0));
        assert_eq!(rs.selected_value(), Some(Value::Text("Łódź".into())));
        rs.selected = Some((GridRow::Data(99), 0));
        assert!(rs.selected_value().is_none());
    }

    #[test]
    #[ignore = "manual release benchmark of exact local filters and cached lookups"]
    fn benchmark_local_filter_cache() {
        use std::{hint::black_box, time::Instant};
        for count in [500, 5_000, 100_000] {
            let mut rs = result();
            std::sync::Arc::make_mut(&mut rs.data).rows = (0..count)
                .map(|i| {
                    vec![
                        Value::Text(if i % 10 == 0 {
                            "Łódź".into()
                        } else {
                            "Warsaw".into()
                        }),
                        Value::Text("Active".into()),
                    ]
                })
                .collect();
            rs.order = (0..count).collect();
            let mut state = LocalFilterState {
                enabled: true,
                ..Default::default()
            };
            state.ensure_columns(2);
            state.conditions[0] = selected(&Value::Text("Łódź".into()));
            state.conditions[1] = selected(&Value::Text("Active".into()));
            let start = Instant::now();
            state.refresh(black_box(&rs));
            let rebuild = start.elapsed();
            let start = Instant::now();
            for i in 0..100_000 {
                black_box(black_box(&mut state).refresh(black_box(&rs)));
                let rows = black_box(state.filtered_rows().unwrap());
                let row = rows[i % rows.len()];
                black_box(state.filtered_index(black_box(row)));
                black_box(state.accepts(black_box(row)));
            }
            println!(
                "local_filter rows={count} rebuild_us={:.2} kept={} cached_refresh_and_lookups_ns={:.2}",
                rebuild.as_secs_f64() * 1e6,
                state.rows.len(),
                start.elapsed().as_secs_f64() * 1e9 / 100_000.0
            );
            assert_eq!(state.rebuilds, 1);
        }
    }
}
