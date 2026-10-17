//! One rectangular selection in the currently displayed grid order.

use super::{GridEvents, GridRow, ResultSet, Shown};
use crate::{filter::FilterValue, model::Value, sqlgen::CellEdit};
use std::fmt::Write;

type Cell = (GridRow, usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CellRange {
    pub(super) row_start: usize,
    pub(super) row_end: usize,
    pub(super) col_start: usize,
    pub(super) col_end: usize,
}

impl CellRange {
    pub(super) fn contains(self, row: usize, col: usize) -> bool {
        (self.row_start..self.row_end).contains(&row)
            && (self.col_start..self.col_end).contains(&col)
    }
    pub(super) fn dimensions(self, transposed: bool) -> (usize, usize) {
        let dimensions = (self.row_end - self.row_start, self.col_end - self.col_start);
        if transposed {
            (dimensions.1, dimensions.0)
        } else {
            dimensions
        }
    }
}

#[derive(Default)]
pub(super) struct SelectionState {
    anchor: Option<Cell>,
    active: Option<Cell>,
    order_dirty: bool,
    pub(super) dragging: bool,
    last_drag_scroll: Option<f64>,
}

/// Advance only the active endpoint, then let the existing virtual table reveal
/// that cell. No row scan or hidden-cell widget is needed for drag scrolling.
pub(super) fn drag_autoscroll(
    ui: &egui::Ui,
    rs: &mut ResultSet,
    ev: &mut GridEvents,
    viewport: egui::Rect,
) {
    let (pointer, down, now) = ui.input(|input| {
        (
            input.pointer.latest_pos(),
            input.pointer.primary_down(),
            input.time,
        )
    });
    if !down || !(rs.selection.dragging || ev.drag_anchor.is_some()) {
        rs.selection.last_drag_scroll = None;
        return;
    }
    let Some(pointer) = pointer else { return };
    let (horizontal, vertical) = edge_delta(pointer, viewport);
    if horizontal == 0 && vertical == 0 {
        rs.selection.last_drag_scroll = None;
        return;
    }
    let Some(mut cell) = ev.drag_to.or(ev.drag_anchor).or(rs.selected) else {
        return;
    };
    if rs.selection.last_drag_scroll.is_some()
        && ev.drag_anchor.is_none()
        && let Some(active) = rs.selected
    {
        // A hovered row can still be behind the revealed endpoint until the
        // next table layout. Keep progress on the scrolling axes between ticks.
        cell = scroll_origin(active, cell, horizontal, vertical, rs.transposed);
        ev.drag_to = Some(cell);
    }
    const INTERVAL: f64 = 0.05;
    let elapsed = rs
        .selection
        .last_drag_scroll
        .map_or(INTERVAL, |last| now - last);
    if elapsed < INTERVAL {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_secs_f64(INTERVAL - elapsed));
        return;
    }
    let Some(target) = rs.offset_visual_cell(cell, horizontal, vertical) else {
        return;
    };
    if target != cell {
        ev.drag_to = Some(target);
        ev.reveal_after = true;
        rs.selection.last_drag_scroll = Some(now);
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_secs_f64(INTERVAL));
    }
}

fn scroll_origin(
    active: Cell,
    hovered: Cell,
    horizontal: isize,
    vertical: isize,
    transposed: bool,
) -> Cell {
    let (rows, columns) = if transposed {
        (horizontal != 0, vertical != 0)
    } else {
        (vertical != 0, horizontal != 0)
    };
    (
        if rows { active.0 } else { hovered.0 },
        if columns { active.1 } else { hovered.1 },
    )
}

fn edge_delta(pointer: egui::Pos2, viewport: egui::Rect) -> (isize, isize) {
    const EDGE: f32 = 14.0;
    let horizontal = if pointer.x <= viewport.left() + EDGE {
        -1
    } else if pointer.x >= viewport.right() - EDGE {
        1
    } else {
        0
    };
    // The fixed column header is not part of the scrollable record area.
    let vertical = if pointer.y <= viewport.top() + 26.0 + EDGE {
        -1
    } else if pointer.y >= viewport.bottom() - EDGE {
        1
    } else {
        0
    };
    (horizontal, vertical)
}

impl ResultSet {
    fn offset_visual_cell(&self, cell: Cell, horizontal: isize, vertical: isize) -> Option<Cell> {
        let (row_delta, col_delta) = if self.transposed {
            (horizontal, vertical)
        } else {
            (vertical, horizontal)
        };
        let last_row = self.row_count().checked_sub(1)?;
        let last_col = self.data.columns.len().checked_sub(1)?;
        let row = self
            .display_index(cell.0)?
            .saturating_add_signed(row_delta)
            .min(last_row);
        Some((
            self.display_row(row)?,
            cell.1.saturating_add_signed(col_delta).min(last_col),
        ))
    }
    /// Called while Find/filter indexes may still be temporarily borrowed.
    /// Reconciliation waits until both filters have installed their new state.
    pub(super) fn invalidate_selection_order(&mut self) {
        self.selection.order_dirty = true;
        self.aggregate.invalidate();
    }

    pub(super) fn sync_selection(&mut self) {
        if self.selection.order_dirty || self.selection.active != self.selected {
            self.selection.anchor = self.valid_selection();
            self.selection.active = self.selected;
            self.selection.dragging = false;
            self.selection.order_dirty = false;
            self.aggregate.invalidate();
        }
    }

    pub(super) fn selection_bounds(&self) -> Option<CellRange> {
        let (active_row, active_col) = self.valid_selection()?;
        let active_index = self.display_index(active_row)?;
        let anchor = if self.selection.active == self.selected && !self.selection.order_dirty {
            self.selection.anchor
        } else {
            None
        };
        let (anchor_row, anchor_col) = anchor
            .filter(|&(row, col)| {
                col < self.data.columns.len() && self.display_index(row).is_some()
            })
            .unwrap_or((active_row, active_col));
        let anchor_index = self.display_index(anchor_row)?;
        Some(CellRange {
            row_start: anchor_index.min(active_index),
            row_end: anchor_index.max(active_index) + 1,
            col_start: anchor_col.min(active_col),
            col_end: anchor_col.max(active_col) + 1,
        })
    }

    pub(super) fn select_cell(&mut self, cell: Cell, extend: bool) {
        let previous = (
            self.selection.anchor,
            self.selection.active,
            self.selection.order_dirty,
        );
        if !extend || self.selection.anchor.is_none() || self.selection.order_dirty {
            self.selection.anchor = if extend {
                self.valid_selection().or(Some(cell))
            } else {
                Some(cell)
            };
        }
        self.selected = Some(cell);
        self.selection.active = self.selected;
        self.selection.order_dirty = false;
        if previous
            != (
                self.selection.anchor,
                self.selection.active,
                self.selection.order_dirty,
            )
        {
            self.aggregate.invalidate();
        }
    }

    pub(super) fn select_all_cells(&mut self) {
        let Some(last_col) = self.data.columns.len().checked_sub(1) else {
            return;
        };
        let (Some(first), Some(last)) = (
            self.display_row(0),
            self.row_count()
                .checked_sub(1)
                .and_then(|i| self.display_row(i)),
        ) else {
            return;
        };
        self.select_cell((first, 0), false);
        self.select_cell((last, last_col), true);
    }

    pub(super) fn select_column(&mut self, column: usize) {
        let (Some(first), Some(last)) = (
            self.display_row(0),
            self.row_count()
                .checked_sub(1)
                .and_then(|i| self.display_row(i)),
        ) else {
            return;
        };
        self.select_cell((first, column), false);
        self.select_cell((last, column), true);
    }

    pub(super) fn select_row(&mut self, row: GridRow) {
        let Some(last_col) = self.data.columns.len().checked_sub(1) else {
            return;
        };
        self.select_cell((row, 0), false);
        self.select_cell((row, last_col), true);
    }

    /// Borrow fetched values and preserve known numeric types in pending edits.
    /// A new row's unvalidated text remains text until the server returns a type.
    pub(super) fn aggregate_cell(&self, row: GridRow, col: usize) -> FilterValue<'_> {
        match row {
            GridRow::Data(index) => match self.edits.changed.get(&(index, col)) {
                Some(edit) => FilterValue::from_edited_value(&self.data.rows[index][col], edit),
                None => FilterValue::from_value(&self.data.rows[index][col]),
            },
            GridRow::New(index) => FilterValue::from_edit(&self.edits.inserted[index][col]),
        }
    }

    pub(super) fn copy_selection(&self) -> Option<String> {
        let range = self.selection_bounds()?;
        let (rows, cols) = range.dimensions(self.transposed);
        if rows == 1 && cols == 1 {
            return Some(self.copy_cell(self.display_row(range.row_start)?, range.col_start));
        }
        let mut output = String::new();
        for visual_row in 0..rows {
            if visual_row != 0 {
                output.push('\n');
            }
            for visual_col in 0..cols {
                if visual_col != 0 {
                    output.push('\t');
                }
                let (row, col) = if self.transposed {
                    (range.row_start + visual_col, range.col_start + visual_row)
                } else {
                    (range.row_start + visual_row, range.col_start + visual_col)
                };
                append_cell(&mut output, self.cell(self.display_row(row)?, col), true);
            }
        }
        Some(output)
    }

    pub(super) fn copy_cell(&self, row: GridRow, col: usize) -> String {
        let mut output = String::new();
        append_cell(&mut output, self.cell(row, col), false);
        output
    }

    pub(super) fn copy_row(&self, row: GridRow) -> String {
        let mut output = String::new();
        for col in 0..self.data.columns.len() {
            if col != 0 {
                output.push('\t');
            }
            append_cell(&mut output, self.cell(row, col), true);
        }
        output
    }
}

fn append_cell(output: &mut String, shown: Shown<'_>, quoted: bool) {
    match shown {
        Shown::Value(Value::Text(text) | Value::Decimal(text))
        | Shown::Edit(CellEdit::Text(text)) => append_text(output, text, quoted),
        Shown::Value(Value::Null) | Shown::Edit(CellEdit::Null) => output.push_str("NULL"),
        Shown::Edit(CellEdit::Default) => output.push_str("DEFAULT"),
        Shown::Value(Value::Bytes(bytes)) => {
            output.push_str("0x");
            for byte in bytes {
                write!(output, "{byte:02X}").unwrap();
            }
        }
        Shown::Value(value) => write!(output, "{value}").unwrap(),
    }
}

fn append_text(output: &mut String, text: &str, quoted: bool) {
    if quoted && (text.is_empty() || text.contains(['\t', '\n', '\r', '"'])) {
        output.push('"');
        for ch in text.chars() {
            if ch == '"' {
                output.push('"');
            }
            output.push(ch);
        }
        output.push('"');
    } else {
        output.push_str(text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Column, Page, QueryResult};

    fn result() -> ResultSet {
        ResultSet::new(
            "selection".into(),
            String::new(),
            QueryResult {
                columns: ["id", "name", "amount"]
                    .map(|name| Column {
                        name: name.into(),
                        type_name: "mixed".into(),
                    })
                    .into(),
                rows: vec![
                    vec![Value::Int(1), Value::Text("Ada".into()), Value::Int(10)],
                    vec![Value::Int(2), Value::Text("Olek".into()), Value::Int(20)],
                    vec![Value::Int(3), Value::Text("Lena".into()), Value::Int(30)],
                ],
                ..Default::default()
            },
            Page::first(500),
        )
        .with_test_table()
    }

    #[test]
    fn rectangle_extends_reverses_shrinks_and_collapses() {
        let mut rs = result();
        rs.select_cell((GridRow::Data(1), 1), false);
        rs.select_cell((GridRow::Data(0), 0), true);
        assert_eq!(rs.copy_selection().as_deref(), Some("1\tAda\n2\tOlek"));
        rs.select_cell((GridRow::Data(2), 2), true);
        assert_eq!(rs.copy_selection().as_deref(), Some("Olek\t20\nLena\t30"));
        rs.select_cell((GridRow::Data(1), 1), true);
        assert_eq!(rs.copy_selection().as_deref(), Some("Olek"));
        rs.select_cell((GridRow::Data(0), 0), false);
        assert_eq!(rs.selection_bounds().unwrap().dimensions(false), (1, 1));
    }

    #[test]
    fn copy_uses_display_order_transpose_and_pending_values() {
        let mut rs = result();
        rs.set_sort(Some((1, true)));
        rs.sync_selection();
        rs.set_cell(GridRow::Data(2), 2, CellEdit::Text("99".into()));
        rs.select_all_cells();
        assert_eq!(
            rs.copy_selection().as_deref(),
            Some("1\tAda\t10\n3\tLena\t99\n2\tOlek\t20")
        );
        rs.transposed = true;
        assert_eq!(
            rs.copy_selection().as_deref(),
            Some("1\t3\t2\nAda\tLena\tOlek\n10\t99\t20")
        );
        rs.select_row(GridRow::Data(2));
        assert_eq!(rs.copy_selection().as_deref(), Some("3\nLena\n99"));
        rs.select_column(1);
        assert_eq!(rs.copy_selection().as_deref(), Some("Ada\tLena\tOlek"));
    }

    #[test]
    fn quoted_tsv_preserves_delimiters_newlines_quotes_and_unicode() {
        let mut rs = result();
        let text = "żółć\t😀\r\n\"quoted\"";
        rs.set_cell(GridRow::Data(0), 1, CellEdit::Text(text.into()));
        let copied = rs.copy_row(GridRow::Data(0));
        let parsed = csv::ReaderBuilder::new()
            .delimiter(b'\t')
            .has_headers(false)
            .from_reader(copied.as_bytes())
            .records()
            .next()
            .unwrap()
            .unwrap();
        assert_eq!(parsed.iter().collect::<Vec<_>>(), ["1", text, "10"]);
        rs.select_cell((GridRow::Data(0), 1), false);
        assert_eq!(rs.copy_selection().as_deref(), Some(text));
    }

    #[test]
    fn one_column_tsv_preserves_empty_first_middle_and_last_records() {
        let mut rs = result();
        for row in [0, 2] {
            rs.set_cell(GridRow::Data(row), 1, CellEdit::Text(String::new()));
        }
        rs.select_column(1);
        let copied = rs.copy_selection().unwrap();
        assert_eq!(copied, "\"\"\nOlek\n\"\"");
        let parsed: Vec<String> = csv::ReaderBuilder::new()
            .delimiter(b'\t')
            .has_headers(false)
            .from_reader(copied.as_bytes())
            .records()
            .map(|row| row.unwrap()[0].to_owned())
            .collect();
        assert_eq!(parsed, ["", "Olek", ""]);
        rs.set_cell(GridRow::Data(1), 1, CellEdit::Text(String::new()));
        assert_eq!(rs.copy_selection().as_deref(), Some("\"\"\n\"\"\n\"\""));
        rs.select_cell((GridRow::Data(0), 1), false);
        assert_eq!(rs.copy_selection().as_deref(), Some(""));
    }

    #[test]
    fn new_rows_defaults_nulls_and_full_binary_payload_are_copied() {
        let mut rs = result();
        std::sync::Arc::make_mut(&mut rs.data).rows[0][1] = Value::Bytes((0..=255).collect());
        let bytes = rs.copy_cell(GridRow::Data(0), 1);
        assert_eq!(bytes.len(), 514);
        assert!(bytes.starts_with("0x00010203"));
        assert!(bytes.ends_with("FEFF"));
        rs.add_row();
        rs.set_cell(GridRow::New(0), 1, CellEdit::Text("new\trow".into()));
        rs.set_cell(GridRow::New(0), 2, CellEdit::Null);
        assert_eq!(rs.copy_row(GridRow::New(0)), "DEFAULT\t\"new\trow\"\tNULL");
        rs.select_row(GridRow::New(0));
        assert_eq!(rs.copy_selection(), Some(rs.copy_row(GridRow::New(0))));
        rs.delete_row(GridRow::New(0));
        rs.sync_selection();
        assert_eq!(rs.copy_selection(), None);
    }

    #[test]
    fn external_selection_and_order_changes_drop_stale_anchors() {
        let mut rs = result();
        rs.select_all_cells();
        rs.selected = Some((GridRow::Data(1), 1));
        rs.sync_selection();
        assert_eq!(rs.copy_selection().as_deref(), Some("Olek"));
        rs.select_all_cells();
        rs.set_sort(Some((0, false)));
        rs.sync_selection();
        assert_eq!(rs.copy_selection().as_deref(), Some("30"));
        rs.selected = Some((GridRow::Data(99), 0));
        rs.sync_selection();
        assert_eq!(rs.selection_bounds(), None);
    }

    #[test]
    fn drag_edges_map_to_clamped_visual_cells_in_both_orientations() {
        let viewport = egui::Rect::from_min_max(egui::pos2(100.0, 100.0), egui::pos2(500.0, 500.0));
        assert_eq!(edge_delta(egui::pos2(300.0, 300.0), viewport), (0, 0));
        assert_eq!(edge_delta(egui::pos2(510.0, 510.0), viewport), (1, 1));
        assert_eq!(edge_delta(egui::pos2(90.0, 110.0), viewport), (-1, -1));
        let mut rs = result();
        assert_eq!(
            rs.offset_visual_cell((GridRow::Data(0), 1), 0, 1),
            Some((GridRow::Data(1), 1))
        );
        assert_eq!(
            rs.offset_visual_cell((GridRow::Data(2), 2), 1, 1),
            Some((GridRow::Data(2), 2))
        );
        rs.transposed = true;
        assert_eq!(
            rs.offset_visual_cell((GridRow::Data(0), 1), 0, 1),
            Some((GridRow::Data(0), 2))
        );
        assert_eq!(
            rs.offset_visual_cell((GridRow::Data(0), 1), 1, 0),
            Some((GridRow::Data(1), 1))
        );
    }

    #[test]
    fn drag_scroll_keeps_progress_between_ticks_and_tracks_the_other_axis() {
        let active = (GridRow::Data(20), 5);
        let hovered = (GridRow::Data(18), 2);
        assert_eq!(
            scroll_origin(active, hovered, 0, 1, false),
            (GridRow::Data(20), 2)
        );
        assert_eq!(
            scroll_origin(active, hovered, 1, 0, true),
            (GridRow::Data(20), 2)
        );
        assert_eq!(scroll_origin(active, hovered, 1, 1, false), active);
        assert_eq!(scroll_origin(active, hovered, 0, 0, false), hovered);
        assert_eq!(
            scroll_origin(active, hovered, 0, -1, true),
            (GridRow::Data(18), 5)
        );
    }
}
