//! Full cell values share the grid's permissions and pending edit buffer.

use super::*;

pub(super) fn viewer(ui: &mut Ui, rs: &mut ResultSet) {
    let Some((row, column)) = rs.valid_selection() else {
        return;
    };
    let editing = rs.edits.editing_surface == CellSurface::Value
        && rs
            .edits
            .editing
            .as_ref()
            .is_some_and(|(r, c, _)| (*r, *c) == (row, column));
    let mut start = false;
    let mut apply = false;
    let mut cancel = false;
    theme::panel_header(ui, |ui| {
        let mut title = egui::text::LayoutJob::default();
        let font = egui::FontId::proportional(13.0);
        title.append(
            &rs.data.columns[column].name,
            0.0,
            egui::TextFormat::simple(font.clone(), theme::TEXT),
        );
        title.append(
            &format!(" · {}", rs.data.columns[column].type_name),
            0.0,
            egui::TextFormat::simple(font, theme::TEXT_MUTED),
        );
        ui.label(title);
        ui.add_space(8.0);
        if editing {
            apply = ui.button("Apply Value").clicked();
            cancel = ui.button("Cancel Value Edit").clicked();
        } else {
            start = ui
                .add_enabled(
                    rs.can_edit_text(row, column),
                    egui::Button::new("Edit Value"),
                )
                .on_disabled_hover_text(rs.text_read_only_reason(row, column).unwrap_or(""))
                .clicked();
            if ui.small_button("Copy").clicked() {
                ui.ctx().copy_text(rs.copy_cell(row, column));
            }
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let response = theme::tool_button(ui, "×", "Close Value Editor");
            response.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Close Value Editor")
            });
            if response.clicked() {
                rs.commit_find_buffer();
                rs.show_value_editor = false;
                rs.pending_grid_focus = true;
            }
        });
    });
    if start {
        rs.commit_find_buffer();
        let text = match rs.cell(row, column) {
            Shown::Value(Value::Null) | Shown::Edit(CellEdit::Null | CellEdit::Default) => {
                String::new()
            }
            Shown::Value(value) => value.to_string(),
            Shown::Edit(CellEdit::Text(text)) => text.clone(),
        };
        rs.edits.editing = Some((row, column, text));
        rs.edits.editing_surface = CellSurface::Value;
        rs.edits.editing_needs_focus = true;
        rs.pending_grid_focus = false;
    }
    if editing || start {
        let id = egui::Id::new(("value_editor_text", rs.action_id, row, column));
        let owned_focus = ui
            .ctx()
            .memory(|memory| memory.has_focus(id) || memory.had_focus_last_frame(id));
        let submit = ui.is_enabled()
            && owned_focus
            && ui.input(|input| {
                input.key_pressed(Key::Enter)
                    && input.modifiers.matches_logically(Modifiers::COMMAND)
            });
        cancel |= ui.is_enabled()
            && owned_focus
            && ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape));
        if let Some((_, _, text)) = &mut rs.edits.editing {
            let minimum_size = ui.available_size();
            let response = egui::ScrollArea::vertical()
                .id_salt("value_edit_scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(text)
                            .id(id)
                            .font(egui::TextStyle::Monospace)
                            .desired_width(f32::INFINITY)
                            .min_size(minimum_size)
                            .hint_text("Value editor text"),
                    )
                })
                .inner;
            // Preserve TextEdit's multiline role and selection nodes. Replacing
            // WidgetInfo would reset its role and copy the entire value again.
            ui.ctx().accesskit_node_builder(response.id, |node| {
                node.set_label("Value editor text");
            });
            if rs.edits.editing_needs_focus {
                response.request_focus();
                rs.edits.editing_needs_focus = false;
            }
            rs.action_focus.editor = Some(id);
            apply |= submit || response.lost_focus();
        }
        if cancel {
            rs.edits.editing = None;
            rs.edits.editing_needs_focus = false;
            rs.pending_grid_focus = true;
        } else if apply {
            rs.commit_find_buffer();
            rs.pending_grid_focus = true;
        }
        return;
    }

    let Some(value) = rs.selected_value() else {
        return;
    };
    let json = match &value {
        Value::Text(text) => super::super::json_view::parse(text),
        _ => None,
    };
    let mode_id = egui::Id::new("value_viewer_json_text");
    let mut as_text = ui
        .ctx()
        .data(|data| data.get_temp::<bool>(mode_id))
        .unwrap_or(false);
    if json.is_some() {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut as_text, false, "Tree")
                .on_hover_text("Collapsible JSON tree");
            ui.selectable_value(&mut as_text, true, "Text")
                .on_hover_text("Formatted JSON");
        });
        ui.ctx().data_mut(|data| data.insert_temp(mode_id, as_text));
    }
    egui::ScrollArea::both()
        .id_salt("value_viewer")
        .auto_shrink([false, false])
        .show(ui, |ui| match &json {
            Some(json) if as_text => {
                ui.add(
                    egui::Label::new(super::super::json_view::highlighted(json)).selectable(true),
                );
            }
            Some(json) => {
                super::super::json_view::tree(ui, json, egui::Id::new(("json", rs.selected)))
            }
            None => {
                let text = match &value {
                    Value::Null => "<null>".to_owned(),
                    value => value.to_string(),
                };
                ui.add(egui::Label::new(RichText::new(text).monospace()).selectable(true));
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::accesskit::Role;
    use egui_kittest::kittest::{NodeT, Queryable};

    fn harness() -> egui_kittest::Harness<'static, ResultSet> {
        let mut result = editability::tests::editable_result();
        result.selected = Some((GridRow::Data(0), 1));
        egui_kittest::Harness::builder()
            .with_size([700.0, 260.0])
            .build_ui_state(viewer, result)
    }

    #[test]
    fn multiline_value_apply_and_cancel_share_pending_edits() {
        let mut h = harness();
        h.get_by_label("Edit Value").click();
        h.run_steps(3);
        h.key_press_modifiers(Modifiers::COMMAND, Key::A);
        h.get_by_role_and_label(Role::MultilineTextInput, "Value editor text")
            .type_text("Łódź\nsecond line");
        h.run_steps(2);
        assert!(h.state().edits.changed.is_empty());
        h.get_by_label("Apply Value").click();
        h.run_steps(3);
        assert_eq!(
            h.state().edits.changed.get(&(0, 1)),
            Some(&CellEdit::Text("Łódź\nsecond line".into()))
        );
        assert_eq!(h.state().data.rows[0][1], Value::Text("Ada".into()));
        assert!(h.state().edits.editing.is_none());

        h.get_by_label("Edit Value").click();
        h.run_steps(3);
        h.key_press_modifiers(Modifiers::COMMAND, Key::A);
        h.get_by_role_and_label(Role::MultilineTextInput, "Value editor text")
            .type_text("discard");
        h.run_steps(2);
        h.key_press(Key::Escape);
        h.run_steps(3);
        assert_eq!(
            h.state().edits.changed.get(&(0, 1)),
            Some(&CellEdit::Text("Łódź\nsecond line".into()))
        );
        assert!(h.state().edits.editing.is_none());
    }

    #[test]
    fn generated_and_restored_values_have_no_edit_entry() {
        let mut h = harness();
        h.state_mut().selected = Some((GridRow::Data(0), 2));
        h.run_steps(2);
        assert!(h.get_by_label("Edit Value").accesskit_node().is_disabled());
        assert!(h.query_by_label("Value editor text").is_none());
        h.state_mut().selected = Some((GridRow::Data(0), 1));
        h.state_mut().set_edit_read_only("Run the query again.");
        h.run_steps(2);
        assert!(h.get_by_label("Edit Value").accesskit_node().is_disabled());
        h.get_by_label("Edit Value").click();
        h.run_steps(2);
        assert!(h.state().edits.editing.is_none());
    }
}
