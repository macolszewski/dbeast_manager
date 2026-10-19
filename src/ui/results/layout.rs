//! Cached grid sizing and allocation-free access to the displayed row order.

use super::{GridRow, ResultSet, Shown, text_width};
use crate::model::Value;
use crate::sqlgen::CellEdit;
use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;

const WIDTH_SAMPLE_ROWS: usize = 50;
const MAX_WIDTH_CHARS: usize = 40;

/// The columns of a wide table that are built this frame. Columns outside the
/// viewport are replaced by one spacer on each side with the same total width,
/// so the scroll extent and positions do not change.
#[derive(Debug, PartialEq)]
pub(super) struct ColumnWindow {
    pub range: std::ops::Range<usize>,
    pub before: f32,
    pub after: f32,
}

/// Extra columns built beyond each side of the viewport.
const WINDOW_MARGIN: usize = 2;

/// Cumulative widths allow viewport lookup without visiting hidden records.
#[derive(Default)]
pub(super) struct HorizontalLayout {
    key: Option<(usize, f32, f32, f32, u64)>,
    offsets: Vec<f32>,
}

impl HorizontalLayout {
    pub(super) fn prepare(&mut self, key: (usize, f32, f32, f32, u64), width_of: impl Fn(usize) -> f32) {
        if self.key == Some(key) { return; }
        self.offsets.clear();
        self.offsets.push(0.0);
        for index in 0..key.0 {
            self.offsets.push(self.offsets[index] + width_of(index) + key.2);
        }
        self.key = Some(key);
    }

    pub(super) fn offset(&self, index: usize) -> f32 { self.offsets[index] }

    pub(super) fn window(&self, visible: egui::Rangef) -> ColumnWindow {
        let count = self.offsets.len().saturating_sub(1);
        let spacing = self.key.map_or(0.0, |key| key.2);
        let first = self.offsets[1..].partition_point(|end| *end - spacing < visible.min);
        let last = self.offsets[..count].partition_point(|start| *start <= visible.max);
        let first = first.min(last).saturating_sub(WINDOW_MARGIN);
        let last = (last + WINDOW_MARGIN).min(count);
        ColumnWindow {
            before: if first > 0 { self.offsets[first] - spacing } else { 0.0 },
            after: if last < count { self.offsets[count] - self.offsets[last] - spacing } else { 0.0 },
            range: first..last,
        }
    }
}

#[cfg(test)]
fn column_window(widths: impl ExactSizeIterator<Item = f32>, spacing: f32, visible: egui::Rangef) -> ColumnWindow {
    let widths: Vec<_> = widths.collect();
    let mut layout = HorizontalLayout::default();
    layout.prepare((widths.len(), 1.0, spacing, 0.0, 0), |i| widths[i]);
    layout.window(visible)
}

#[derive(Default)]
pub(super) struct LayoutCache {
    normal: OnceCell<Vec<f32>>,
    pub(super) horizontal_normal: RefCell<HorizontalLayout>,
    pub(super) horizontal_transposed: RefCell<HorizontalLayout>,
    transposed: RefCell<HashMap<GridRow, f32>>,
    order_positions: OnceCell<Vec<usize>>,
}

impl LayoutCache {
    pub(super) fn clear_columns(&mut self) {
        self.normal.take();
        self.horizontal_normal.get_mut().key = None;
        self.clear_transposed();
    }

    /// Pending edits affect transposed widths, while fetched column samples are immutable.
    pub(super) fn clear_transposed(&mut self) {
        self.transposed.get_mut().clear();
        self.horizontal_transposed.get_mut().key = None;
    }

    /// Sorting changes record positions, but does not change their cached widths.
    pub(super) fn clear_order(&mut self) {
        self.order_positions.take();
        self.horizontal_transposed.get_mut().key = None;
    }
}

impl ResultSet {
    pub(super) fn row_count(&self) -> usize {
        self.filtered_rows().map_or_else(
            || self.order.len() + self.edits.inserted.len(),
            <[GridRow]>::len,
        )
    }

    pub(super) fn filtered_rows(&self) -> Option<&[GridRow]> {
        self.find
            .filtered_rows()
            .or_else(|| self.local_filter.filtered_rows())
    }

    /// Direct lookup keeps drawing a visible row independent of the fetched page size.
    pub(super) fn display_row(&self, index: usize) -> Option<GridRow> {
        if let Some(rows) = self.filtered_rows() {
            return rows.get(index).copied();
        }
        if let Some(&original) = self.order.get(index) {
            Some(GridRow::Data(original))
        } else {
            let inserted = index.checked_sub(self.order.len())?;
            (inserted < self.edits.inserted.len()).then_some(GridRow::New(inserted))
        }
    }

    /// The inverse of `display_row`; rebuild the inverse order only after sorting.
    pub(super) fn display_index(&self, row: GridRow) -> Option<usize> {
        if self.find.filtered_rows().is_some() {
            return self.find.filtered_index(row);
        }
        if self.local_filter.filtered_rows().is_some() {
            return self.local_filter.filtered_index(row);
        }
        match row {
            GridRow::Data(original) => {
                if original >= self.data.rows.len() {
                    return None;
                }
                if self.sort.is_none() {
                    return Some(original);
                }
                self.layout
                    .order_positions
                    .get_or_init(|| {
                        let mut positions = vec![0; self.order.len()];
                        for (display, &original) in self.order.iter().enumerate() {
                            positions[original] = display;
                        }
                        positions
                    })
                    .get(original)
                    .copied()
            }
            GridRow::New(inserted) => {
                (inserted < self.edits.inserted.len()).then_some(self.order.len() + inserted)
            }
        }
    }

    /// Calculate the initial widths once from a fixed sample of the fetched rows.
    pub(super) fn normal_widths(&self) -> &[f32] {
        self.layout.normal.get_or_init(|| {
            self.data
                .columns
                .iter()
                .enumerate()
                .map(|(column, meta)| {
                    let header = bounded_chars(&meta.name)
                        .max(meta.type_name.chars().take(MAX_WIDTH_CHARS * 2).count() / 2 + 2);
                    let sample = self
                        .data
                        .rows
                        .iter()
                        .take(WIDTH_SAMPLE_ROWS)
                        .map(|row| value_chars(&row[column]))
                        .max()
                        .unwrap_or(0);
                    let editor = super::date_edit::column_chars(&meta.type_name)
                        .max(if self.is_auto_increment(column) { 11 } else { 0 });
                    text_width(header.max(sample).max(editor))
                })
                .collect()
        })
    }

    /// Cache each requested record without revisiting its values on subsequent frames.
    pub(super) fn transposed_width(&self, row: GridRow) -> f32 {
        if let Some(&width) = self.layout.transposed.borrow().get(&row) {
            return width;
        }
        let widest = (0..self.data.columns.len())
            .map(|column| (match self.cell(row, column) {
                Shown::Value(value) => value_chars(value),
                Shown::Edit(CellEdit::Text(text)) => bounded_chars(text),
                Shown::Edit(_) => if self.is_auto_increment(column) { 11 } else { 9 },
            }).max(super::date_edit::column_chars(&self.data.columns[column].type_name)))
            .max()
            .unwrap_or(0);
        let width = text_width(widest);
        self.layout.transposed.borrow_mut().insert(row, width);
        width
    }
}

fn bounded_chars(text: &str) -> usize {
    text.chars().take(MAX_WIDTH_CHARS).count()
}

/// Match `Value::to_cell` without copying text, decimals, or binary payloads.
fn value_chars(value: &Value) -> usize {
    match value {
        Value::Null => 6,
        Value::Bool(value) => {
            if *value {
                4
            } else {
                5
            }
        }
        Value::Text(text) | Value::Decimal(text) => bounded_chars(text),
        Value::Bytes(bytes) => {
            (2 + bytes.len().min(32) * 2 + usize::from(bytes.len() > 32)).min(MAX_WIDTH_CHARS)
        }
        Value::Int(_) | Value::Float(_) => bounded_chars(&value.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn horizontal_layout_does_not_visit_hidden_records_on_repaint() {
        let mut layout = HorizontalLayout::default();
        let key = (100_000, 1.0, 4.0, 40.0, 0);
        layout.prepare(key, |_| 100.0);
        layout.prepare(key, |_| panic!("unchanged widths must stay cached"));
        assert_eq!(layout.window(egui::Rangef::new(52_000.0, 52_500.0)).range, 498..507);
        layout.prepare((100_000, 1.0, 4.0, 40.0, 1), |_| 200.0);
        assert_eq!(layout.offset(2), 408.0);
    }

    #[test]
    fn column_window_keeps_positions_and_total_width() {
        let widths = vec![100.0; 1000];
        let total = |w: &ColumnWindow| {
            w.before
                + w.after
                + w.range.len() as f32 * 100.0
                + (w.range.len() + usize::from(w.before > 0.0) + usize::from(w.after > 0.0) - 1)
                    as f32
                    * 4.0
        };
        let at_start = column_window(widths.iter().copied(), 4.0, egui::Rangef::new(0.0, 500.0));
        assert_eq!(at_start.range, 0..7);
        assert_eq!(at_start.before, 0.0);
        let middle = column_window(widths.iter().copied(), 4.0, egui::Rangef::new(52_000.0, 52_500.0));
        assert_eq!(middle.range, 498..507);
        // The first built column starts where it would without the window.
        assert_eq!(middle.before + 4.0, 498.0 * 104.0);
        let end = column_window(widths.iter().copied(), 4.0, egui::Rangef::new(103_600.0, 104_000.0));
        assert_eq!(end.range.end, 1000);
        assert_eq!(end.after, 0.0);
        for window in [&at_start, &middle, &end] {
            assert_eq!(total(window), 1000.0 * 104.0 - 4.0);
        }
        let few = column_window([50.0, 60.0].into_iter(), 4.0, egui::Rangef::new(0.0, 900.0));
        assert_eq!(few, ColumnWindow { range: 0..2, before: 0.0, after: 0.0 });
    }
    use crate::model::{Column, Page, QueryResult};

    fn result(rows: usize) -> ResultSet {
        ResultSet::new(
            "layout".into(),
            "select id, name".into(),
            QueryResult {
                columns: vec![
                    Column {
                        name: "id".into(),
                        type_name: "INTEGER".into(),
                    },
                    Column {
                        name: "name".into(),
                        type_name: "TEXT".into(),
                    },
                ],
                rows: (0..rows)
                    .map(|row| vec![Value::Int(row as i64), Value::Text("value".into())])
                    .collect(),
                ..Default::default()
            },
            Page::first(rows),
        )
        .with_test_table()
    }

    #[test]
    fn sizing_matches_display_and_bounds_large_values() {
        let values = [
            Value::Null,
            Value::Bool(true),
            Value::Bool(false),
            Value::Int(i64::MIN),
            Value::Float(f64::MIN_POSITIVE),
            Value::Text("a\nb\nżółć".into()),
            Value::Decimal("123456789.125".into()),
            Value::Bytes(Vec::new()),
            Value::Bytes(vec![0; 8]),
            Value::Bytes(vec![255; 1024]),
        ];
        for value in values {
            assert_eq!(value_chars(&value), value.to_cell().chars().count().min(40));
        }
        let large = Value::Text("ż".repeat(1_000_000));
        assert_eq!(value_chars(&large), MAX_WIDTH_CHARS);
    }

    #[test]
    fn normal_widths_are_sampled_and_cached() {
        let mut rs = result(51);
        std::sync::Arc::make_mut(&mut rs.data).rows[50][1] = Value::Text("x".repeat(1_000_000));
        assert_eq!(rs.normal_widths(), &[text_width(5), text_width(5)]);
        let widths = rs.normal_widths().as_ptr();
        assert_eq!(rs.normal_widths().as_ptr(), widths);
        rs.set_sort(Some((0, false)));
        assert_eq!(rs.normal_widths().as_ptr(), widths);
    }

    #[test]
    fn transposed_cache_is_lazy_and_invalidated_by_edits() {
        let mut rs = result(100_000);
        assert!(rs.layout.transposed.borrow().is_empty());
        let original = rs.transposed_width(GridRow::Data(42));
        assert_eq!(rs.layout.transposed.borrow().len(), 1);
        assert_eq!(rs.transposed_width(GridRow::Data(42)), original);
        assert_eq!(rs.layout.transposed.borrow().len(), 1);
        rs.set_cell(
            GridRow::Data(42),
            1,
            CellEdit::Text("long pending edit".into()),
        );
        assert_eq!(rs.transposed_width(GridRow::Data(42)), text_width(17));
        rs.revert();
        assert_eq!(rs.transposed_width(GridRow::Data(42)), original);

        rs.add_row();
        rs.add_row();
        rs.set_cell(
            GridRow::New(0),
            1,
            CellEdit::Text("first inserted row is longer".into()),
        );
        rs.set_cell(GridRow::New(1), 1, CellEdit::Text("second row".into()));
        assert_eq!(rs.transposed_width(GridRow::New(0)), text_width(28));
        rs.delete_row(GridRow::New(0));
        assert_eq!(rs.transposed_width(GridRow::New(0)), text_width(10));
    }

    #[test]
    fn row_lookup_handles_large_pages_sorting_and_insertions() {
        for count in [500, 5_000, 100_000] {
            let mut rs = result(count);
            assert_eq!(rs.row_count(), count);
            assert_eq!(rs.display_row(0), Some(GridRow::Data(0)));
            assert_eq!(rs.display_row(count - 1), Some(GridRow::Data(count - 1)));
            assert_eq!(rs.display_row(count), None);
            assert_eq!(rs.display_row(usize::MAX), None);
            assert_eq!(rs.display_index(GridRow::Data(count)), None);
            rs.set_sort(Some((0, false)));
            for display in [0, count / 2, count - 1] {
                let row = GridRow::Data(count - display - 1);
                assert_eq!(rs.display_row(display), Some(row));
                assert_eq!(rs.display_index(row), Some(display));
            }
            let positions = rs.layout.order_positions.get().unwrap().as_ptr();
            assert_eq!(rs.display_index(GridRow::Data(0)), Some(count - 1));
            assert_eq!(rs.layout.order_positions.get().unwrap().as_ptr(), positions);
            rs.set_sort(Some((0, true)));
            assert_eq!(rs.display_index(GridRow::Data(0)), Some(0));
            rs.set_sort(None);
            assert_eq!(rs.display_index(GridRow::Data(count - 1)), Some(count - 1));
            rs.add_row();
            assert_eq!(rs.row_count(), count + 1);
            assert_eq!(rs.display_row(count), Some(GridRow::New(0)));
            assert_eq!(rs.display_index(GridRow::New(0)), Some(count));
            rs.revert();
            assert_eq!(rs.display_index(GridRow::New(0)), None);
        }
    }

    #[test]
    #[ignore = "Run with --release --ignored --nocapture to measure grid layout preparation"]
    fn layout_preparation_benchmark() {
        use std::hint::black_box;
        use std::time::Instant;

        const FRAMES: usize = 1_000;
        const VISIBLE_ROWS: usize = 40;
        for count in [500, 5_000, 100_000] {
            let rs = result(count);
            black_box(rs.normal_widths());
            for row in 0..count {
                black_box(rs.transposed_width(GridRow::Data(row)));
            }
            let start = Instant::now();
            for _ in 0..FRAMES {
                let rows: Vec<GridRow> = rs.order.iter().copied().map(GridRow::Data).collect();
                black_box(rows);
                let widths: Vec<f32> = rs
                    .data
                    .columns
                    .iter()
                    .enumerate()
                    .map(|(column, meta)| {
                        let header = meta.name.len().max(meta.type_name.len() / 2 + 2);
                        let sample = rs
                            .data
                            .rows
                            .iter()
                            .take(50)
                            .map(|row| row[column].to_cell().chars().count())
                            .max()
                            .unwrap_or(0);
                        text_width(header.max(sample))
                    })
                    .collect();
                black_box(widths);
            }
            let before = start.elapsed();
            let start = Instant::now();
            for _ in 0..FRAMES {
                let rows: Vec<GridRow> = rs.order.iter().copied().map(GridRow::Data).collect();
                let widths: Vec<f32> = rows
                    .iter()
                    .map(|&row| {
                        text_width(
                            (0..rs.data.columns.len())
                                .map(|column| match rs.cell(row, column) {
                                    Shown::Value(value) => value.to_cell().chars().count(),
                                    Shown::Edit(CellEdit::Text(text)) => text.chars().count(),
                                    Shown::Edit(_) => 9,
                                })
                                .max()
                                .unwrap_or(0),
                        )
                    })
                    .collect();
                black_box(widths);
            }
            let before_transposed = start.elapsed();
            let start = Instant::now();
            for _ in 0..FRAMES {
                black_box(rs.normal_widths());
                black_box(rs.row_count());
                for display in 0..VISIBLE_ROWS {
                    let row = black_box(rs.display_row(black_box(display)).unwrap());
                    black_box(rs.display_index(row));
                }
            }
            let cached = start.elapsed();
            let start = Instant::now();
            for _ in 0..FRAMES {
                for display in 0..rs.row_count() {
                    let row = rs.display_row(black_box(display)).unwrap();
                    black_box(rs.transposed_width(row));
                }
            }
            let transposed = start.elapsed();
            println!(
                "{count} rows, {FRAMES} frames: old normal layout {before:?}, cached normal with {VISIBLE_ROWS} visible lookups {cached:?}, old transposed {before_transposed:?}, cached transposed over all records {transposed:?}"
            );
        }
    }
}
