//! Cached statistics for the active rectangular cell selection.

use super::{GridEvents, ResultSet, selection::CellRange, theme};
use crate::aggregate::{self, AggregateNumber, AggregateResult, AggregateSummary};
use egui::{Align, Layout, RichText, Ui};

struct Metric {
    label: &'static str,
    text: String,
    detail: Option<String>,
}

pub(super) struct AggregateState {
    dirty: bool,
    selection: Option<CellRange>,
    transposed: bool,
    metrics: Vec<Metric>,
    #[cfg(test)]
    rebuilds: usize,
}

impl Default for AggregateState {
    fn default() -> Self {
        Self {
            dirty: true,
            selection: None,
            transposed: false,
            metrics: Vec::new(),
            #[cfg(test)]
            rebuilds: 0,
        }
    }
}

impl AggregateState {
    pub(super) fn invalidate(&mut self) {
        self.dirty = true;
    }

    fn refresh(&mut self, rs: &ResultSet) {
        let selection = rs.selection_bounds();
        if !self.dirty && self.selection == selection && self.transposed == rs.transposed {
            return;
        }
        let summary = if let Some(range) = selection {
            let (rows, cols) = range.dimensions(rs.transposed);
            let cells = (range.row_start..range.row_end).flat_map(|row| {
                let row = rs.display_row(row).unwrap();
                (range.col_start..range.col_end).map(move |col| rs.aggregate_cell(row, col))
            });
            aggregate::aggregate(cells, rows, cols)
        } else {
            AggregateSummary::default()
        };
        self.metrics.clear();
        for (label, value) in [
            ("ROWS", summary.rows),
            ("COLS", summary.cols),
            ("COUNT", summary.count),
            ("COUNT_NUMS", summary.count_nums),
        ] {
            self.metrics.push(Metric {
                label,
                text: value.to_string(),
                detail: None,
            });
        }
        for (label, value) in [
            ("SUM", summary.sum),
            ("AVG", summary.avg),
            ("MIN", summary.min),
            ("MAX", summary.max),
            ("MEDIAN", summary.median),
            ("COEFFICIENT_OF_VARIATION", summary.coefficient_of_variation),
        ] {
            self.metrics.push(metric(label, value));
        }
        self.metrics.sort_by_key(|metric| metric.label);
        self.selection = selection;
        self.transposed = rs.transposed;
        self.dirty = false;
        #[cfg(test)]
        {
            self.rebuilds += 1;
        }
    }
}

fn metric(label: &'static str, value: AggregateResult) -> Metric {
    let (text, detail) = match value {
        Ok(Some(AggregateNumber::Decimal(value))) => (value.normalize().to_string(), None),
        Ok(Some(AggregateNumber::Approximate(value))) => (
            format!("≈ {value}"),
            Some("Approximate floating-point result".into()),
        ),
        Ok(None) => ("N/A".into(), Some("No numeric cells selected".into())),
        Err(error) => ("Unavailable".into(), Some(error.to_string())),
    };
    Metric {
        label,
        text,
        detail,
    }
}

pub(super) fn panel(ui: &mut Ui, rs: &mut ResultSet, ev: &mut GridEvents) {
    let mut state = std::mem::take(&mut rs.aggregate);
    state.refresh(rs);
    egui::Panel::right(ui.id().with("aggregate_view"))
        .resizable(true)
        .default_size(280.0)
        .size_range(210.0..=560.0)
        .frame(theme::side_panel_frame())
        .show(ui, |ui| {
            theme::panel_header(ui, |ui| {
                ui.label(RichText::new("Aggregates").strong());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let close = theme::tool_button(ui, "×", "Close Aggregate View");
                    close.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::Button,
                            true,
                            "Close Aggregate View",
                        )
                    });
                    if close.clicked() {
                        ev.close_aggregate = true;
                    }
                });
            });
            egui::ScrollArea::vertical()
                .id_salt("aggregate_metrics")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for metric in &state.metrics {
                        ui.horizontal(|ui| {
                            let label_width = ui.available_width() * 0.58;
                            ui.allocate_ui_with_layout(
                                egui::vec2(label_width, 22.0),
                                Layout::left_to_right(Align::Center),
                                |ui| {
                                    ui.set_min_width(label_width);
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(metric.label).color(theme::TEXT_MUTED),
                                        )
                                        .truncate(),
                                    )
                                    .on_hover_text(metric.label);
                                },
                            );
                            let response = ui.add(
                                egui::Label::new(RichText::new(&metric.text).monospace())
                                    .truncate(),
                            );
                            response.widget_info(|| {
                                egui::WidgetInfo::labeled(
                                    egui::WidgetType::Label,
                                    true,
                                    format!("{}: {}", metric.label, metric.text),
                                )
                            });
                            response.on_hover_ui(|ui| {
                                ui.label(&metric.text);
                                if let Some(detail) = &metric.detail {
                                    ui.label(detail);
                                }
                            });
                        });
                    }
                    ui.separator();
                    ui.label(
                        RichText::new(if state.selection.is_some() {
                            "Selected cells · current page"
                        } else {
                            "Select cells to calculate aggregates"
                        })
                        .small()
                        .color(theme::TEXT_MUTED),
                    );
                });
        });
    rs.aggregate = state;
}

#[cfg(test)]
mod tests {
    use super::super::GridRow;
    use super::*;
    use crate::{
        model::{Column, Page, QueryResult, Value},
        sqlgen::CellEdit,
    };

    fn result() -> ResultSet {
        ResultSet::new(
            "aggregate".into(),
            String::new(),
            QueryResult {
                columns: ["number", "text"]
                    .map(|name| Column {
                        name: name.into(),
                        type_name: "mixed".into(),
                    })
                    .into(),
                rows: vec![
                    vec![Value::Int(10), Value::Text("100".into())],
                    vec![Value::Decimal("20.5".into()), Value::Null],
                ],
                ..Default::default()
            },
            Page::first(500),
        )
        .with_test_table()
    }

    fn refresh(rs: &mut ResultSet) {
        let mut state = std::mem::take(&mut rs.aggregate);
        state.refresh(rs);
        rs.aggregate = state;
    }

    fn value<'a>(rs: &'a ResultSet, label: &str) -> &'a str {
        &rs.aggregate
            .metrics
            .iter()
            .find(|metric| metric.label == label)
            .unwrap()
            .text
    }

    #[test]
    fn aggregates_count_types_and_keep_idle_and_stationary_drag_cached() {
        let mut rs = result();
        rs.select_all_cells();
        refresh(&mut rs);
        assert_eq!(value(&rs, "COUNT"), "4");
        assert_eq!(value(&rs, "COUNT_NUMS"), "2");
        assert_eq!(value(&rs, "SUM"), "30.5");
        assert_eq!(value(&rs, "AVG"), "15.25");
        let allocation = value(&rs, "SUM").as_ptr();
        for _ in 0..100 {
            rs.select_cell((GridRow::Data(1), 1), true);
            refresh(&mut rs);
        }
        assert_eq!(rs.aggregate.rebuilds, 1);
        assert_eq!(allocation, value(&rs, "SUM").as_ptr());
        rs.set_cell(GridRow::Data(0), 0, CellEdit::Text("50".into()));
        refresh(&mut rs);
        assert_eq!(rs.aggregate.rebuilds, 2);
        assert_eq!(value(&rs, "SUM"), "70.5");
        rs.transposed = true;
        refresh(&mut rs);
        assert_eq!(rs.aggregate.rebuilds, 3);
    }

    #[test]
    fn new_defaults_pending_text_and_deleted_rows_keep_explicit_types() {
        let mut rs = result();
        rs.delete_row(GridRow::Data(0));
        rs.add_row();
        rs.set_cell(GridRow::New(0), 0, CellEdit::Text("200".into()));
        rs.select_all_cells();
        refresh(&mut rs);
        assert_eq!(value(&rs, "COUNT"), "6");
        assert_eq!(value(&rs, "COUNT_NUMS"), "2");
        assert_eq!(value(&rs, "SUM"), "30.5");
        assert_eq!(value(&rs, "ROWS"), "3");
        assert_eq!(value(&rs, "COLS"), "2");
        rs.transposed = true;
        refresh(&mut rs);
        assert_eq!(value(&rs, "ROWS"), "2");
        assert_eq!(value(&rs, "COLS"), "3");
    }

    #[test]
    fn unavailable_and_missing_numeric_results_have_distinct_details() {
        let mut rs = result();
        rs.select_column(1);
        refresh(&mut rs);
        assert_eq!(value(&rs, "COUNT"), "2");
        assert_eq!(value(&rs, "SUM"), "N/A");
        std::sync::Arc::make_mut(&mut rs.data).rows[0][0] = Value::Float(f64::NAN);
        rs.select_column(0);
        refresh(&mut rs);
        assert_eq!(value(&rs, "COUNT_NUMS"), "2");
        assert_eq!(value(&rs, "SUM"), "Unavailable");
        assert!(
            rs.aggregate
                .metrics
                .iter()
                .find(|metric| metric.label == "SUM")
                .unwrap()
                .detail
                .as_ref()
                .unwrap()
                .contains("NaN")
        );
        rs.selected = None;
        rs.sync_selection();
        refresh(&mut rs);
        assert_eq!(value(&rs, "COUNT"), "0");
    }
}
