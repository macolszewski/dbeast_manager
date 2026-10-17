//! Runtime editing authority is separate from persisted result data.

use super::{GridRow, ResultSet};
use crate::model::ResultEditMetadata;
use std::sync::Arc;

pub(super) enum ResultEditability {
    Checking,
    ReadOnly(String),
    Editable(Arc<ResultEditMetadata>),
}

impl Default for ResultEditability {
    fn default() -> Self {
        Self::ReadOnly("Run the query again before editing this result.".into())
    }
}

impl ResultSet {
    pub(in crate::ui) fn begin_edit_metadata(&mut self, request_id: u64) {
        self.commit_find_buffer();
        self.edit_metadata_request = Some(request_id);
        self.editability = ResultEditability::Checking;
        self.edit_status = "Checking whether this result can be edited…".into();
        self.edit_preview = None;
    }

    pub(in crate::ui) fn accepts_edit_metadata(&self, request_id: u64) -> bool {
        self.edit_metadata_request == Some(request_id)
    }

    /// Returns false when a newer request already superseded this response.
    pub(in crate::ui) fn finish_edit_metadata(
        &mut self,
        request_id: u64,
        result: Result<ResultEditMetadata, String>,
    ) -> bool {
        if !self.accepts_edit_metadata(request_id) {
            return false;
        }
        self.edit_metadata_request = None;
        self.edit_preview = None;
        self.editability = match result {
            Ok(metadata) if self.metadata_fits(&metadata) => {
                ResultEditability::Editable(Arc::new(metadata))
            }
            Ok(_) => ResultEditability::ReadOnly(
                "The result metadata no longer matches this result. Run the query again.".into(),
            ),
            Err(reason) => ResultEditability::ReadOnly(reason),
        };
        self.refresh_edit_status();
        true
    }

    fn metadata_fits(&self, metadata: &ResultEditMetadata) -> bool {
        let columns = self.data.columns.len();
        metadata.physical_columns.len() == columns
            && metadata.writable.len() == columns
            && metadata.comparable.len() == columns
            && metadata.update_default.len() == columns
            && !metadata.primary_key.is_empty()
            && metadata.primary_key.iter().all(|&column| column < columns)
            && metadata.writable.iter().any(|&writable| writable)
            && self.data.source.as_ref().is_some_and(|source| {
                source.session_epoch == metadata.session_epoch
                    && source.columns.len() == columns
                    && source.exact_columns.len() == columns
            })
    }

    /// Keep local changes as a reference, including after an unknown commit outcome.
    pub(in crate::ui) fn set_edit_read_only(&mut self, reason: impl Into<String>) {
        self.commit_find_buffer();
        self.edit_metadata_request = None;
        self.editability = ResultEditability::ReadOnly(reason.into());
        self.refresh_edit_status();
        self.edit_preview = None;
    }

    pub(in crate::ui) fn edit_metadata(&self) -> Option<&ResultEditMetadata> {
        match &self.editability {
            ResultEditability::Editable(metadata) => Some(metadata),
            _ => None,
        }
    }

    fn refresh_edit_status(&mut self) {
        self.edit_status = match &self.editability {
            ResultEditability::Checking => "Checking whether this result can be edited…".into(),
            ResultEditability::ReadOnly(reason) => format!("Read-only: {reason}"),
            ResultEditability::Editable(_) => "Editable result".into(),
        };
    }

    pub(in crate::ui) fn edit_metadata_pending(&self) -> bool {
        matches!(self.editability, ResultEditability::Checking)
    }

    pub(in crate::ui) fn edit_read_only_reason(&self) -> Option<&str> {
        match &self.editability {
            ResultEditability::Checking => Some("Checking whether this result can be edited…"),
            ResultEditability::ReadOnly(reason) => Some(reason),
            ResultEditability::Editable(_) => None,
        }
    }

    pub(in crate::ui) fn can_submit_edits(&self) -> bool {
        matches!(self.editability, ResultEditability::Editable(_))
    }

    pub(super) fn can_edit_cell(&self, row: GridRow, column: usize) -> bool {
        if column >= self.data.columns.len() {
            return false;
        }
        match row {
            GridRow::Data(index) => {
                if self.edits.deleted.contains(&index)
                    || self
                        .data
                        .rows
                        .get(index)
                        .is_none_or(|row| column >= row.len())
                {
                    return false;
                }
                self.edit_metadata()
                    .is_some_and(|metadata| metadata.writable[column])
            }
            GridRow::New(index) => {
                self.edit_metadata()
                    .is_some_and(|metadata| metadata.writable[column])
                    && self
                        .edits
                        .inserted
                        .get(index)
                        .is_some_and(|row| column < row.len())
            }
        }
    }

    pub(super) fn can_edit_text(&self, row: GridRow, column: usize) -> bool {
        self.can_edit_cell(row, column)
            && !matches!(row, GridRow::Data(index) if matches!(self.data.rows[index][column], crate::model::Value::Bytes(_)))
    }

    pub(super) fn text_read_only_reason(&self, row: GridRow, column: usize) -> Option<&str> {
        self.cell_read_only_reason(row, column).or_else(|| {
            (!self.can_edit_text(row, column))
                .then_some("Binary values require a binary editor; Set NULL remains available.")
        })
    }

    pub(super) fn cell_read_only_reason(&self, row: GridRow, column: usize) -> Option<&str> {
        if self.can_edit_cell(row, column) {
            None
        } else if matches!(row, GridRow::Data(index) if self.edits.deleted.contains(&index)) {
            Some("The selected row is marked for deletion.")
        } else {
            self.edit_read_only_reason().or(Some(
                "This column is computed, generated, or cannot be updated safely.",
            ))
        }
    }

    pub(super) fn can_set_default(&self, row: GridRow, column: usize) -> bool {
        self.can_edit_cell(row, column)
            && (matches!(row, GridRow::New(_))
                || self
                    .edit_metadata()
                    .is_some_and(|metadata| metadata.update_default[column]))
    }

    pub(super) fn can_add_row(&self) -> bool {
        self.edit_metadata().is_some()
    }

    pub(super) fn can_delete_row(&self, row: GridRow) -> bool {
        self.edit_metadata().is_some()
            && match row {
                GridRow::Data(index) => index < self.data.rows.len(),
                GridRow::New(index) => index < self.edits.inserted.len(),
            }
    }

    pub(super) fn can_clone_row(&self, row: GridRow) -> bool {
        self.can_delete_row(row)
            && !matches!(row, GridRow::Data(index) if self.edits.deleted.contains(&index))
    }
}

#[cfg(test)]
impl ResultSet {
    /// Grants edit authority over a synthetic single table, key in column 0.
    pub(in crate::ui) fn with_test_table(mut self) -> Self {
        use crate::model::{ColumnOrigin, OriginColumn, OriginTable, PhysicalTable, ResultSource};
        let names: Vec<String> = self.data.columns.iter().map(|c| c.name.clone()).collect();
        let columns = names.len();
        std::sync::Arc::make_mut(&mut self.data).source = Some(ResultSource {
            session_epoch: 1,
            columns: names
                .iter()
                .map(|name| {
                    Some(ColumnOrigin {
                        table: OriginTable::Named {
                            schema: "main".into(),
                            table: "test".into(),
                        },
                        column: OriginColumn::Named(name.clone()),
                    })
                })
                .collect(),
            exact_columns: vec![true; columns],
        });
        self.editability = ResultEditability::Editable(Arc::new(ResultEditMetadata {
            table: PhysicalTable {
                schema: "main".into(),
                name: "test".into(),
                oid: None,
            },
            physical_columns: names.into_iter().map(Some).collect(),
            writable: vec![true; columns],
            comparable: vec![true; columns],
            update_default: vec![false; columns],
            primary_key: vec![0],
            session_epoch: 1,
        }));
        self.refresh_edit_status();
        self
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::{
        model::{
            Column, ColumnOrigin, DriverKind, OriginColumn, OriginTable, Page, PhysicalTable,
            QueryResult, ResultSource, Value,
        },
        sqlgen::CellEdit,
    };

    pub(in crate::ui::results) fn console_result() -> ResultSet {
        ResultSet::new(
            "Result".into(),
            "select id as selected_id, name as display_name, length(name) as size from records"
                .into(),
            QueryResult {
                columns: ["selected_id", "display_name", "size"]
                    .map(|name| Column {
                        name: name.into(),
                        type_name: "mixed".into(),
                    })
                    .into(),
                rows: vec![vec![
                    Value::Int(7),
                    Value::Text("Ada".into()),
                    Value::Int(3),
                ]],
                source: Some(ResultSource {
                    session_epoch: 17,
                    columns: vec![
                        Some(ColumnOrigin {
                            table: OriginTable::Named {
                                schema: "main".into(),
                                table: "records".into(),
                            },
                            column: OriginColumn::Named("id".into()),
                        }),
                        Some(ColumnOrigin {
                            table: OriginTable::Named {
                                schema: "main".into(),
                                table: "records".into(),
                            },
                            column: OriginColumn::Named("name".into()),
                        }),
                        None,
                    ],
                    exact_columns: vec![true; 3],
                }),
                ..Default::default()
            },
            Page::first(500),
        )
    }

    pub(in crate::ui::results) fn metadata() -> ResultEditMetadata {
        ResultEditMetadata {
            table: PhysicalTable {
                schema: "main".into(),
                name: "records".into(),
                oid: None,
            },
            physical_columns: vec![Some("id".into()), Some("name".into()), None],
            writable: vec![true, true, false],
            comparable: vec![true, true, false],
            update_default: vec![false; 3],
            primary_key: vec![0],
            session_epoch: 17,
        }
    }

    pub(in crate::ui::results) fn editable_result() -> ResultSet {
        let mut result = console_result();
        result.begin_edit_metadata(1);
        assert!(result.finish_edit_metadata(1, Ok(metadata())));
        result
    }

    #[test]
    fn stale_metadata_and_restored_results_never_gain_authority() {
        let mut result = console_result();
        assert!(!result.can_submit_edits());
        result.begin_edit_metadata(1);
        result.begin_edit_metadata(2);
        assert!(result.edit_metadata_pending());
        assert!(!result.finish_edit_metadata(1, Ok(metadata())));
        assert!(result.edit_metadata_pending());
        assert!(result.finish_edit_metadata(2, Ok(metadata())));
        assert!(result.can_submit_edits());
        assert!(!result.finish_edit_metadata(2, Err("late failure".into())));
        assert!(result.can_submit_edits());

        let encoded = serde_json::to_string(&result.data).unwrap();
        let data = serde_json::from_str(&encoded).unwrap();
        let mut restored = ResultSet::new("restored".into(), result.sql.clone(), data, result.page);
        assert!(restored.data.source.is_none());
        restored.begin_edit_metadata(3);
        assert!(restored.finish_edit_metadata(3, Ok(metadata())));
        assert!(!restored.can_submit_edits());
        assert!(
            restored
                .edit_read_only_reason()
                .unwrap()
                .contains("no longer matches")
        );
    }

    #[test]
    fn verified_capabilities_are_enforced_at_every_mutation_entry() {
        let mut result = editable_result();
        let row = GridRow::Data(0);
        assert!(result.can_edit_cell(row, 0));
        assert!(result.can_edit_cell(row, 1));
        assert!(!result.can_edit_cell(row, 2));
        assert!(!result.can_edit_cell(GridRow::Data(99), 1));
        assert!(!result.can_edit_cell(row, 99));
        assert!(!result.can_set_default(row, 1));
        result.set_cell(row, 2, CellEdit::Text("unsafe expression".into()));
        result.set_cell(row, 1, CellEdit::Default);
        assert!(result.edits.is_empty());
        result.set_cell(row, 1, CellEdit::Text("Olek".into()));
        assert_eq!(
            result.edits.changed.get(&(0, 1)),
            Some(&CellEdit::Text("Olek".into()))
        );
        // Verified metadata also allows adding and deleting rows, but a new row
        // still cannot set a computed column, and the key is left to its default.
        result.add_row();
        assert!(result.can_edit_cell(GridRow::New(0), 1));
        assert!(!result.can_edit_cell(GridRow::New(0), 2));
        result.clone_row(row);
        assert_eq!(result.edits.inserted[1][0], CellEdit::Default);
        result.delete_row(row);
        assert!(result.edits.deleted.contains(&0));
        assert!(!result.can_edit_cell(row, 1));
        // Layout and transpose do not grant a computed column editing authority.
        result.transposed = true;
        result.record_view = true;
        assert!(!result.can_edit_cell(row, 2));
    }

    #[test]
    fn default_mask_and_legacy_insert_default_remain_independent() {
        let mut result = console_result();
        let mut meta = metadata();
        meta.update_default[1] = true;
        result.begin_edit_metadata(1);
        result.finish_edit_metadata(1, Ok(meta));
        result.set_cell(GridRow::Data(0), 1, CellEdit::Default);
        assert_eq!(result.edits.changed.get(&(0, 1)), Some(&CellEdit::Default));

        let mut table = console_result().with_test_table();
        table.add_row();
        assert!(table.can_set_default(GridRow::New(0), 1));
        assert!(!table.can_set_default(GridRow::Data(0), 1));
        assert!(table.can_delete_row(GridRow::New(0)));
        table.delete_row(GridRow::Data(0));
        assert!(!table.can_edit_cell(GridRow::Data(0), 1));
    }

    #[test]
    fn unknown_outcome_preserves_buffer_and_edits_without_submit_authority() {
        let mut result = editable_result();
        result.edits.editing = Some((GridRow::Data(0), 1, "reference only".into()));
        result.set_edit_read_only("The commit outcome is unknown. Run the query again.");
        assert!(!result.can_submit_edits());
        assert!(result.edits.editing.is_none());
        assert_eq!(
            result.edits.changed.get(&(0, 1)),
            Some(&CellEdit::Text("reference only".into()))
        );
        result.set_cell(
            GridRow::Data(0),
            1,
            CellEdit::Text("must not overwrite".into()),
        );
        assert_eq!(
            result.edits.changed.get(&(0, 1)),
            Some(&CellEdit::Text("reference only".into()))
        );
        result.revert();
        assert!(result.edits.is_empty());
        assert!(!result.can_submit_edits());
    }

    #[test]
    fn bound_preview_uses_physical_names_and_reuses_cached_text() {
        let mut result = editable_result();
        result.set_cell(GridRow::Data(0), 1, CellEdit::Text("private ' text".into()));
        let preview = result.pending_preview(DriverKind::Sqlite);
        let (statement, parameters) = preview.split_once('\n').unwrap();
        assert!(statement.contains("\"name\" = ?"));
        assert!(!preview.contains("display_name"));
        // Values appear only in the display comment, never inside the SQL.
        assert!(!statement.contains("private"));
        assert!(parameters.starts_with("-- parameters: 1 = 'private '' text'"));
        let allocation = preview.as_ptr();
        for _ in 0..100 {
            assert_eq!(
                result.pending_preview(DriverKind::Sqlite).as_ptr(),
                allocation
            );
        }
        result.set_cell(GridRow::Data(0), 1, CellEdit::Text("new".into()));
        assert!(result.edit_preview.is_none());
    }
    #[test]
    fn binary_display_placeholders_cannot_be_saved_as_text() {
        let mut result = editable_result();
        std::sync::Arc::make_mut(&mut result.data).rows[0][1] = Value::Bytes(vec![0, 255, 13]);
        assert!(result.can_edit_cell(GridRow::Data(0), 1));
        assert!(!result.can_edit_text(GridRow::Data(0), 1));
        result.set_cell(GridRow::Data(0), 1, CellEdit::Text("<3 bytes>".into()));
        assert!(result.edits.is_empty());
        result.set_cell(GridRow::Data(0), 1, CellEdit::Null);
        assert_eq!(result.edits.changed.get(&(0, 1)), Some(&CellEdit::Null));
        assert!(!result.can_edit_text(GridRow::Data(0), 1));
    }

    #[test]
    fn toolbar_prioritizes_submit_and_moves_secondary_controls_without_overlap() {
        use egui::accesskit::Role;
        use egui_kittest::kittest::{NodeT, Queryable};
        struct State {
            result: ResultSet,
            bounds: egui::Rect,
        }
        let mut result = editable_result();
        result.set_cell(GridRow::Data(0), 1, CellEdit::Text("pending".into()));
        let mut h = egui_kittest::Harness::builder()
            .with_size([1440.0, 200.0])
            .build_ui_state(
                |ui, state: &mut State| {
                    ui.horizontal(|ui| {
                        ui.allocate_space(egui::vec2(350.0, 56.0));
                        ui.vertical(|ui| {
                            state.bounds = ui.available_rect_before_wrap();
                            super::super::grid_toolbar(
                                ui,
                                &mut state.result,
                                DriverKind::Sqlite,
                                500,
                                true,
                            );
                        });
                    });
                },
                State {
                    result,
                    bounds: egui::Rect::NOTHING,
                },
            );
        crate::ui::theme::apply(&h.ctx);
        for width in [1440.0, 1100.0] {
            h.set_size(egui::vec2(width, 200.0));
            h.run_steps(4);
            for label in [
                "◀",
                "▶",
                "Revert",
                "Submit",
                "DML",
                "Options  ⏷",
                "Export  ⏷",
            ] {
                h.get_by_label(label);
            }
            let controls: Vec<_> = h
                .query_all_by_role(Role::Button)
                .chain(h.query_all_by_role(Role::ComboBox))
                .map(|node| {
                    (
                        node.accesskit_node().label().unwrap_or_default(),
                        node.rect(),
                    )
                })
                .collect();
            for (index, (label, rect)) in controls.iter().enumerate() {
                assert!(
                    h.state().bounds.expand(1.0).contains_rect(*rect),
                    "{label} clipped at width {width}: {rect:?}, {:?}",
                    h.state().bounds
                );
                for (other_label, other) in &controls[index + 1..] {
                    let overlap = rect.intersect(*other);
                    assert!(
                        overlap.width() <= 1.0 || overlap.height() <= 1.0,
                        "{label} overlaps {other_label} at width {width}"
                    );
                }
            }
        }
        assert!(h.query_by_label("Aggregate View").is_none());
        h.get_by_label("Options  ⏷").click();
        h.run_steps(3);
        h.get_by_label("Aggregate View").click();
        h.run_steps(3);
        assert!(h.state().result.aggregate_view);
        assert!(!h.ctx.any_popup_open());
    }
}
