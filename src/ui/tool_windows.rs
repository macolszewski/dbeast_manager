//! All entry points share tool-window visibility and focus behavior.

use super::{App, ServicesView, TabKind, actions::ActionId, console};
use egui::{Context, Id};

pub(super) fn explorer_filter_id() -> Id {
    Id::new("explorer_filter")
}
pub(super) fn files_search_id() -> Id {
    Id::new("files_search")
}
pub(super) fn services_focus_id() -> Id {
    Id::new("services_focus")
}

impl App {
    pub(super) fn focus_active_editor(&mut self, ctx: &Context) {
        self.pending_action_focus = None;
        self.pending_editor_focus = true;
        ctx.request_repaint();
    }

    pub(super) fn apply_pending_editor_focus(&mut self, ctx: &Context) {
        if self.find_action.visible
            || self.related_rows.is_open()
            || self.structure_compare.is_open()
            || self.compare.is_open()
            || self.pending_close.is_some()
            || ctx.memory(|memory| memory.top_modal_layer().is_some())
            || !std::mem::take(&mut self.pending_editor_focus)
        {
            return;
        }
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        match &tab.kind {
            TabKind::Console { .. } => console::request_action_focus(tab, ctx),
            TabKind::Table { .. } => {
                if let Some(result) = tab.results.first_mut() {
                    result.request_action_focus();
                }
            }
        }
    }

    pub(super) fn apply_tool_window_action(&mut self, action: ActionId, ctx: &Context) {
        self.explorer_ui.cancel_reveal();
        let visible = match action {
            ActionId::DatabaseExplorer => {
                self.show_explorer = !self.show_explorer;
                if self.show_explorer {
                    self.pending_editor_focus = false;
                    self.show_files = false;
                    self.pending_action_focus = Some(explorer_filter_id());
                }
                self.show_explorer
            }
            ActionId::Files => {
                self.show_files = !self.show_files;
                if self.show_files {
                    self.pending_editor_focus = false;
                    self.show_explorer = false;
                    self.pending_action_focus = Some(files_search_id());
                }
                self.show_files
            }
            ActionId::Run => {
                self.show_run = !self.show_run;
                self.show_run
            }
            ActionId::Services => {
                // Run takes the bottom area; Services comes back in its place.
                self.show_services = self.show_run || !self.show_services;
                self.show_run = false;
                if self.show_services {
                    self.pending_editor_focus = false;
                    self.pending_action_focus = None;
                    if let Some(tab) = self.tabs.get_mut(self.active)
                        && let ServicesView::Result(index) = tab.view
                        && let Some(result) = tab.results.get_mut(index)
                    {
                        result.request_action_focus();
                    } else {
                        self.pending_action_focus = Some(services_focus_id());
                    }
                }
                self.show_services
            }
            _ => return,
        };
        if !visible {
            self.focus_active_editor(ctx);
        }
        ctx.request_repaint();
    }
}
