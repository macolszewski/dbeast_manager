//! Shared typed controls for grid, Record View and Value Editor.
use super::*;
use crate::cell_value::{ColumnSpec, EditorKind};

impl ResultSet {
    pub(super) fn column_spec(&self, column: usize) -> Option<&ColumnSpec> {
        self.edit_metadata()?.column_specs.get(column)
    }

    pub(super) fn boolean_column(&self, column: usize) -> bool {
        self.column_spec(column)
            .is_some_and(|spec| spec.kind == EditorKind::Boolean)
            || self.data.columns[column]
                .type_name
                .eq_ignore_ascii_case("bool")
            || self.data.columns[column]
                .type_name
                .eq_ignore_ascii_case("boolean")
    }

    pub(super) fn enum_column(&self, column: usize) -> Option<&[String]> {
        match &self.column_spec(column)?.kind {
            EditorKind::Enum(values) => Some(values),
            _ => None,
        }
    }

    pub(super) fn can_set_null(&self, row: GridRow, column: usize) -> bool {
        self.can_edit_cell(row, column)
            && self.column_spec(column).is_some_and(|spec| spec.nullable)
    }

    pub(super) fn validate_text(
        &self,
        row: GridRow,
        column: usize,
        text: &str,
    ) -> Result<(), String> {
        if matches!(row, GridRow::New(_)) && self.is_auto_increment(column) && text.is_empty() {
            return Ok(());
        }
        self.column_spec(column)
            .ok_or_else(|| "Column metadata is unavailable.".to_string())?
            .validate_text(text)
            .map_err(|error| format!("{}: {error}", self.data.columns[column].name))
    }

    pub(super) fn validate_cell(
        &self,
        row: GridRow,
        column: usize,
        edit: &CellEdit,
    ) -> Result<(), String> {
        if matches!(row, GridRow::New(_))
            && self.is_auto_increment(column)
            && matches!(edit, CellEdit::Text(text) if text.is_empty())
        {
            return Ok(());
        }
        self.column_spec(column)
            .ok_or_else(|| "Column metadata is unavailable.".to_string())?
            .validate(edit)
            .map_err(|error| format!("{}: {error}", self.data.columns[column].name))
    }
}

pub(super) fn boolean(
    ui: &mut Ui,
    shown: Shown<'_>,
    enabled: bool,
    nullable: bool,
) -> Option<CellEdit> {
    let (value, empty) = match shown {
        Shown::Value(Value::Bool(value)) => (Some(*value), ""),
        Shown::Value(Value::Null) | Shown::Edit(CellEdit::Null) => (None, "NULL"),
        Shown::Edit(CellEdit::Default) => (None, "DEFAULT"),
        Shown::Edit(CellEdit::Text(text)) | Shown::Value(Value::Text(text)) => {
            (crate::cell_value::boolean(text), "Invalid")
        }
        Shown::Value(Value::Int(value)) if *value == 0 || *value == 1 => (Some(*value == 1), ""),
        _ => (None, "Invalid"),
    };
    let mut checked = value.unwrap_or(false);
    let label = value.map_or(empty, |v| if v { "True" } else { "False" });
    let mut edit = None;
    let response = ui.scope(|ui| {
        if value.is_some() {
            let widgets = &mut ui.style_mut().visuals.widgets;
            for visual in [&mut widgets.inactive, &mut widgets.hovered, &mut widgets.active, &mut widgets.noninteractive] {
                visual.fg_stroke = egui::Stroke::new(2.0, theme::GREEN);
            }
        }
        let checkbox = if value.is_some() {
            egui::Checkbox::without_text(&mut checked)
        } else {
            egui::Checkbox::new(&mut checked, empty).indeterminate(true)
        };
        ui.add_enabled(enabled, checkbox)
    }).inner;
    // Keep a spoken state for accessibility without duplicating the visual checkmark.
    ui.ctx().accesskit_node_builder(response.id, |node| node.set_label(label));
    if response.changed() {
        edit = Some(CellEdit::Text(checked.to_string()));
    }
    if nullable
        && ui
            .add_enabled(
                enabled && empty != "NULL",
                egui::Button::new("NULL").small(),
            )
            .on_hover_text("Set NULL")
            .clicked()
    {
        edit = Some(CellEdit::Null);
    }
    edit
}

pub(super) fn validation_feedback(
    ui: &Ui,
    response: &egui::Response,
    refresh: bool,
    validate: impl FnOnce() -> Option<String>,
) -> Option<String> {
    let id = response.id.with("typed_validation");
    let cached = ui.ctx().data(|data| data.get_temp::<Option<String>>(id));
    let error = if refresh || response.changed() || cached.is_none() {
        let error = validate();
        ui.ctx()
            .data_mut(|data| data.insert_temp(id, error.clone()));
        error
    } else {
        cached.flatten()
    };
    if let Some(error) = &error {
        ui.painter().rect_stroke(
            response.rect,
            2.0,
            egui::Stroke::new(1.0, Color32::from_rgb(230, 90, 90)),
            egui::StrokeKind::Inside,
        );
        response.clone().on_hover_text(error);
    }
    error
}

pub(super) fn toggle_boolean(shown: Shown<'_>) -> CellEdit {
    let value = match shown {
        Shown::Value(Value::Bool(v)) => *v,
        Shown::Value(Value::Int(v)) => *v != 0,
        Shown::Value(Value::Text(text)) | Shown::Edit(CellEdit::Text(text)) => {
            crate::cell_value::boolean(text).unwrap_or(false)
        }
        _ => false,
    };
    CellEdit::Text((!value).to_string())
}

/// Catalog labels are selectable without accepting arbitrary text.
pub(super) fn enumeration(
    ui: &mut Ui,
    shown: Shown<'_>,
    enabled: bool,
    nullable: bool,
    values: &[String],
) -> Option<CellEdit> {
    let label = match shown {
        Shown::Value(Value::Text(s)) | Shown::Edit(CellEdit::Text(s)) => s.as_str(),
        Shown::Edit(CellEdit::Default) => "DEFAULT",
        Shown::Value(Value::Null) | Shown::Edit(CellEdit::Null) => "NULL",
        _ => "Invalid",
    };
    let mut edit = None;
    ui.add_enabled_ui(enabled, |ui| {
        egui::ComboBox::from_id_salt("enum_value")
            .selected_text(label)
            .width(ui.available_width().min(240.0))
            .show_ui(ui, |ui| {
                for value in values {
                    if ui.selectable_label(label == value, value).clicked() {
                        edit = Some(CellEdit::Text(value.clone()));
                    }
                }
                if nullable {
                    ui.separator();
                    if ui.button("Set NULL").clicked() {
                        edit = Some(CellEdit::Null);
                    }
                }
            });
    });
    edit
}
