//! A single record, using the same value cells and pending edits as the grid.

use super::*;

pub(super) fn panel(ui: &mut Ui, rs: &ResultSet, ev: &mut GridEvents) {
    egui::Panel::right(ui.id().with("record_view"))
        .resizable(true)
        .default_size(340.0)
        .size_range(220.0..=600.0)
        .frame(theme::side_panel_frame())
        .show(ui, |ui| {
            let selected = rs.valid_selection();
            let index = selected.and_then(|(row, _)| rs.display_index(row));
            theme::panel_header(ui, |ui| {
                ui.label(RichText::new("Record View").strong());
                if theme::tool_button_enabled(
                    ui,
                    index.is_some_and(|i| i > 0),
                    "◀",
                    "Previous Record",
                )
                .clicked()
                {
                    select_adjacent(rs, ev, -1);
                }
                if theme::tool_button_enabled(
                    ui,
                    index.is_some_and(|i| i + 1 < rs.row_count()),
                    "▶",
                    "Next Record",
                )
                .clicked()
                {
                    select_adjacent(rs, ev, 1);
                }
                if let Some(index) = index {
                    ui.label(
                        RichText::new(format!("{} of {}", index + 1, rs.row_count()))
                            .small()
                            .color(theme::TEXT_MUTED),
                    );
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if theme::tool_button(ui, "×", "Close Record View").clicked() {
                        ev.close_record = true;
                    }
                });
            });
            let Some((grid_row, _)) = selected else {
                ui.label(
                    RichText::new(if rs.row_count() == 0 {
                        "No records"
                    } else {
                        "Select a cell to view its record"
                    })
                    .color(theme::TEXT_MUTED),
                );
                return;
            };
            let look = GridLook::of(ui);
            let mut table = TableBuilder::new(ui)
                .id_salt("record_fields")
                .striped(look.striped)
                .resizable(true)
                .sense(Sense::click())
                .min_scrolled_height(0.0)
                .auto_shrink([false, false])
                .cell_layout(Layout::left_to_right(Align::Center))
                .column(Column::initial(120.0).at_least(60.0).clip(true))
                .column(Column::remainder().at_least(80.0).clip(true));
            if let Some((_, col)) = ev.reveal {
                table = table.scroll_to_row(col, None);
            }
            table
                .header(26.0, |mut header| {
                    for label in ["Column", "Value"] {
                        header.col(|ui| {
                            header_cell_bg(ui);
                            ui.add_space(6.0);
                            ui.label(RichText::new(label).strong());
                        });
                    }
                })
                .body(|body| {
                    body.rows(look.row_height, rs.data.columns.len(), |mut row| {
                        let col = row.index();
                        row.col(|ui| {
                            ui.painter()
                                .rect_filled(ui.max_rect(), 0.0, theme::BG_PANEL);
                            grid_lines(ui);
                            ui.add_space(6.0);
                            ui.add(egui::Label::new(&rs.data.columns[col].name).truncate())
                                .on_hover_text(&rs.data.columns[col].type_name);
                        });
                        data_cell(&mut row, rs, grid_row, col, CellSurface::Record, ev);
                    });
                });
        });
}

fn select_adjacent(rs: &ResultSet, ev: &mut GridEvents, delta: isize) {
    if let Some((row, col)) = rs.moved_selection(delta, 0) {
        ev.clicked_cell = Some((row, col, CellSurface::Record));
        ev.reveal_after = true;
    }
}
