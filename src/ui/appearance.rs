//! Appearance dialog: interface zoom, editor and grid font sizes, grid row
//! density and stripes. Changes preview live; Cancel restores the previous look.

use super::{App, theme, zoom};
use crate::config::{Appearance, RowDensity};
use egui::{Context, Id, RichText, Ui};

const LABEL_WIDTH: f32 = 110.0;

pub(super) struct AppearanceDialog {
    original: Appearance,
    original_zoom: f32,
}

impl App {
    pub(super) fn open_appearance(&mut self) {
        self.appearance_dialog = Some(AppearanceDialog {
            original: self.config.appearance.clone(),
            original_zoom: self.config.ui_zoom,
        });
    }

    pub(super) fn appearance_window(&mut self, ctx: &Context) {
        let Some(dialog) = &self.appearance_dialog else {
            return;
        };
        let mut draft = self.config.appearance.clone();
        let mut zoom = self.config.ui_zoom;
        let (mut ok, mut cancel) = (false, false);
        let response = theme::Dialog::new(Id::new("appearance_dialog"), "Appearance")
            .default_size([460.0, 380.0])
            .min_size([380.0, 340.0])
            .show(ctx, |ui| {
                theme::section_header(ui, "Interface");
                row(ui, "Zoom:", |ui| {
                    egui::ComboBox::from_id_salt("appearance_zoom")
                        .selected_text(format!("{:.0} %", zoom * 100.0))
                        .width(90.0)
                        .show_ui(ui, |ui| {
                            for step in 5..=25 {
                                let value = step as f32 / 10.0;
                                ui.selectable_value(&mut zoom, value, format!("{:.0} %", value * 100.0));
                            }
                        });
                    theme::help_icon(
                        ui,
                        &theme::keys("Scales the whole interface. Also Zoom In IDE ⌃⌥⇧= and Zoom Out IDE ⌃⌥⇧-."),
                    );
                });
                theme::section_header(ui, "Editor");
                row(ui, "Font size:", |ui| font_size(ui, &mut draft.editor_font_size));
                theme::section_header(ui, "Data Editor");
                row(ui, "Font size:", |ui| font_size(ui, &mut draft.grid_font_size));
                row(ui, "Row height:", |ui| {
                    egui::ComboBox::from_id_salt("appearance_density")
                        .selected_text(draft.row_density.label())
                        .width(120.0)
                        .show_ui(ui, |ui| {
                            for density in RowDensity::ALL {
                                ui.selectable_value(&mut draft.row_density, density, density.label());
                            }
                        });
                });
                row(ui, "", |ui| {
                    ui.checkbox(&mut draft.striped_rows, "Striped rows");
                });
                ui.add_space(12.0);
                theme::dialog_footer(
                    ui,
                    |ui| {
                        if ui.button("Reset to Defaults").clicked() {
                            draft = Appearance::default();
                            zoom = 1.0;
                        }
                    },
                    |ui| {
                        cancel = theme::dialog_button(ui, "Cancel").clicked();
                        ok = theme::primary_button(ui, "OK").clicked();
                    },
                );
            });
        if response.should_close() {
            cancel = true;
        }
        if cancel {
            draft = dialog.original.clone();
            zoom = dialog.original_zoom;
        }
        self.preview_appearance(ctx, draft, zoom);
        if ok || cancel {
            self.appearance_dialog = None;
            if ok {
                self.save_config();
            }
        }
    }

    fn preview_appearance(&mut self, ctx: &Context, appearance: Appearance, ui_zoom: f32) {
        let appearance = appearance.clamped();
        if appearance != self.config.appearance {
            theme::set_appearance(ctx, &appearance);
            self.config.appearance = appearance;
        }
        let ui_zoom = zoom::clamp(ui_zoom);
        if ui_zoom != self.config.ui_zoom {
            ctx.set_zoom_factor(ui_zoom);
            self.config.ui_zoom = ui_zoom;
        }
    }
}

fn row(ui: &mut Ui, label: &str, field: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(
            egui::vec2(LABEL_WIDTH, 24.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.set_min_width(LABEL_WIDTH);
                ui.label(label);
            },
        );
        field(ui);
    });
    ui.add_space(4.0);
}

fn font_size(ui: &mut Ui, size: &mut f32) {
    ui.add(
        egui::DragValue::new(size)
            .range(Appearance::FONT_SIZES)
            .speed(0.1)
            .fixed_decimals(1),
    );
    // Live preview applies each new size; half-point steps keep a drag from
    // rasterizing and re-laying out text for every tenth of a point.
    *size = (*size * 2.0).round() / 2.0;
    ui.label(RichText::new("pt").color(theme::TEXT_MUTED));
}
