//! Small command adapters keep palette dispatch independent of grid internals.

use super::ResultSet;
use crate::ui::actions::{ActionId, GridActionState};
use egui::{Context, Id};

#[derive(Default)]
pub(super) struct ActionFocus {
    pub(super) grid: Option<Id>,
    pub(super) editor: Option<Id>,
}

impl ResultSet {
    pub(in crate::ui) fn related_focus_id(&self) -> Option<Id> {
        self.action_focus.grid
    }

    pub(in crate::ui) fn related_action_has_focus(&self, ctx: &Context) -> bool {
        self.action_focus
            .grid
            .is_some_and(|id| ctx.memory(|memory| memory.has_focus(id)))
    }

    /// Constant-time eligibility; provenance/FK mapping is validated on demand.
    pub(in crate::ui) fn related_row_anchor(&self) -> Result<(usize, usize), &'static str> {
        if self.geo.is_some() {
            return Err("Switch from Geo Viewer to the result grid");
        }
        if !self.edits.is_empty() || self.edits.editing.is_some() {
            return Err("Submit or revert pending changes before opening Related Rows");
        }
        let Some((super::GridRow::Data(row), col)) = self.valid_selection() else {
            return Err("Select an existing row first");
        };
        if self
            .selection_bounds()
            .is_some_and(|range| range.row_end - range.row_start > 1)
        {
            return Err("Related Rows currently supports one selected row");
        }
        if self.data.source.is_none() {
            return Err("Execute the query again to establish column origins");
        }
        Ok((row, col))
    }

    pub(in crate::ui) fn action_identity(&self) -> u64 {
        self.action_id
    }

    pub(in crate::ui) fn action_has_focus(&self, ctx: &Context) -> bool {
        let Some(focused) = ctx.memory(|memory| memory.focused()) else {
            return false;
        };
        self.action_focus.grid == Some(focused)
            || self.action_focus.editor == Some(focused)
            || self
                .action_focus
                .grid
                .is_some_and(|id| self.find.action_has_focus(id, focused))
    }

    pub(in crate::ui) fn action_state(&self) -> GridActionState {
        let selection = self.valid_selection();
        GridActionState {
            grid_visible: self.geo.is_none(),
            can_add_row: self.can_add_row(),
            can_delete_row: selection.is_some_and(|(row, _)| self.can_delete_row(row)),
            can_clone_row: selection.is_some_and(|(row, _)| self.can_clone_row(row)),
            can_submit: self.can_submit_edits(),
            has_selection: selection.is_some(),
            has_changes: !self.edits.is_empty() || self.edits.editing.is_some(),
            has_local_filters: self.local_filter.has_conditions(),
        }
    }

    pub(in crate::ui) fn stage_action_buffer(&mut self) {
        self.commit_find_buffer();
    }

    pub(in crate::ui) fn request_action_focus(&mut self) {
        self.pending_grid_focus = true;
    }

    pub(in crate::ui) fn apply_local_action(&mut self, action: ActionId, ctx: &Context) -> bool {
        self.commit_find_buffer();
        match action {
            ActionId::FindOnCurrentPage => self.open_find(),
            ActionId::RecordView => {
                self.record_view = !self.record_view;
                if self.record_view {
                    self.aggregate_view = false;
                }
                self.pending_reveal = true;
            }
            ActionId::AggregateView => {
                self.aggregate_view = !self.aggregate_view;
                if self.aggregate_view {
                    self.record_view = false;
                }
            }
            ActionId::ValueEditor => self.show_value_editor = true,
            ActionId::Transpose => {
                self.transposed = !self.transposed;
                self.find.invalidate();
                self.aggregate.invalidate();
                self.pending_reveal = true;
            }
            ActionId::LocalFilter => self.toggle_local_filter(),
            ActionId::ClearLocalFilters => self.clear_local_filters(),
            ActionId::AddRow if self.can_add_row() => self.add_row(),
            ActionId::DeleteRow => {
                let Some((row, _)) = self.valid_selection() else {
                    return false;
                };
                if !self.can_delete_row(row) {
                    return false;
                }
                self.delete_row(row);
            }
            ActionId::CloneRow => {
                let Some((row, _)) = self.valid_selection() else {
                    return false;
                };
                if !self.can_clone_row(row) {
                    return false;
                }
                self.clone_row(row);
            }
            ActionId::Revert => self.revert(),
            _ => return false,
        }
        if action != ActionId::FindOnCurrentPage {
            self.pending_grid_focus = true;
        }
        ctx.request_repaint();
        true
    }
}
