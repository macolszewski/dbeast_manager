//! Cached grid sizing and allocation-free access to the displayed row order.

use super::{GridRow, ResultSet, Shown, text_width};
use crate::model::Value;
use crate::sqlgen::CellEdit;
use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;

const WIDTH_SAMPLE_ROWS: usize = 50;
const MAX_WIDTH_CHARS: usize = 40;

#[derive(Default)]
pub(super) struct LayoutCache {
    normal: OnceCell<Vec<f32>>,
    transposed: RefCell<HashMap<GridRow, f32>>,
    order_positions: OnceCell<Vec<usize>>,
}

impl LayoutCache {
    /// Pending edits affect transposed widths, while fetched column samples are immutable.
    pub(super) fn clear_transposed(&mut self) {
        self.transposed.get_mut().clear();
    }

    /// Sorting changes record positions, but does not change their cached widths.
    pub(super) fn clear_order(&mut self) {
        self.order_positions.take();
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
                    text_width(header.max(sample))
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
            .map(|column| match self.cell(row, column) {
                Shown::Value(value) => value_chars(value),
                Shown::Edit(CellEdit::Text(text)) => bounded_chars(text),
                Shown::Edit(_) => 9,
            })
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
