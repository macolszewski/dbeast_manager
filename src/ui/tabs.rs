//! Tab navigation and closing share stable document identities and one save.

use super::{App, Tab, TabKind, theme};
use egui::{Context, RichText};
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CloseScope {
    Tab,
    OtherTabs,
    TabsToRight,
    AllTabs,
}

/// The requested documents are fixed when the menu action is invoked. A tab
/// opened while the confirmation is visible never joins this request.
pub(super) struct CloseRequest {
    ids: HashSet<u64>,
    /// SQL is not copied into the dialog. It cannot change under the modal.
    sql_consoles: HashSet<u64>,
    /// Close File removes the temporary source only after all its tabs close.
    cleanup_source: Option<String>,
    focus_cancel: bool,
}

#[derive(Default)]
struct CloseRisks {
    tabs: usize,
    staged: usize,
    transactions: usize,
    file_writes: usize,
    running: usize,
    saving: usize,
    context_changes: usize,
    sql_consoles: usize,
}

impl CloseRisks {
    fn requires_confirmation(&self) -> bool {
        self.staged != 0
            || self.transactions != 0
            || self.file_writes != 0
            || self.running != 0
            || self.saving != 0
            || self.context_changes != 0
            || self.sql_consoles != 0
    }
}

impl CloseRequest {
    fn risks(&self, tabs: &[Tab]) -> CloseRisks {
        let mut risks = CloseRisks::default();
        for tab in tabs.iter().filter(|tab| self.ids.contains(&tab.id)) {
            risks.tabs += 1;
            risks.staged += usize::from(tab.results.iter().any(|rs| !rs.edits.is_empty()));
            risks.transactions += usize::from(tab.in_tx);
            risks.file_writes += usize::from(tab.unsaved_writes);
            risks.running += usize::from(tab.is_running());
            risks.saving += usize::from(tab.saving_file);
            risks.context_changes += usize::from(tab.context_pending.is_some());
            risks.sql_consoles += usize::from(self.sql_consoles.contains(&tab.id));
        }
        risks
    }
}

fn adjacent_index(len: usize, active: usize, backwards: bool) -> Option<usize> {
    let last = len.checked_sub(1)?;
    let active = active.min(last);
    Some(if backwards {
        active.checked_sub(1).unwrap_or(last)
    } else if active == last {
        0
    } else {
        active + 1
    })
}

fn close_targets(ids: &[u64], anchor: u64, scope: CloseScope) -> Vec<u64> {
    let Some(anchor_index) = ids.iter().position(|id| *id == anchor) else {
        return Vec::new();
    };
    ids.iter()
        .enumerate()
        .filter(|(index, id)| match scope {
            CloseScope::Tab => **id == anchor,
            CloseScope::OtherTabs => **id != anchor,
            CloseScope::TabsToRight => *index > anchor_index,
            CloseScope::AllTabs => true,
        })
        .map(|(_, &id)| id)
        .collect()
}

/// Preserve the active document if it survives; otherwise use its first
/// surviving right neighbor, then the last left neighbor, or the empty index.
fn active_after_close(ids: &[u64], active: usize, closed: &HashSet<u64>) -> usize {
    let before = ids
        .iter()
        .take(active)
        .filter(|id| !closed.contains(id))
        .count();
    let remaining = ids.iter().filter(|id| !closed.contains(id)).count();
    before.min(remaining.saturating_sub(1))
}

impl App {
    /// Switching documents stages the current input without submitting SQL.
    pub(super) fn select_tab_by_id(&mut self, id: u64) -> bool {
        if self.pending_close.is_some() {
            return false;
        }
        let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
            return false;
        };
        self.explorer_ui.cancel_reveal();
        if index == self.active {
            return false;
        }
        self.stage_active_cell();
        self.related_rows.cancel();
        self.active = index;
        self.reveal_active_tab = true;
        self.pending_editor_focus = true;
        true
    }

    pub(super) fn select_adjacent_tab(&mut self, backwards: bool) -> bool {
        let Some(index) = adjacent_index(self.tabs.len(), self.active, backwards) else {
            return false;
        };
        self.select_tab_by_id(self.tabs[index].id)
    }

    /// Also used before creating a new document that hides the active grid.
    pub(super) fn stage_active_cell(&mut self) {
        if let Some(tab) = self.tabs.get_mut(self.active) {
            for result in &mut tab.results {
                result.commit_find_buffer();
            }
        }
    }

    pub(super) fn request_close_tabs(&mut self, anchor: u64, scope: CloseScope) {
        if self.pending_close.is_some() {
            return;
        }
        let order: Vec<_> = self.tabs.iter().map(|tab| tab.id).collect();
        let ids: HashSet<_> = close_targets(&order, anchor, scope).into_iter().collect();
        if ids.is_empty() {
            return;
        }
        self.request_close_ids(ids, None);
    }

    pub(super) fn request_close_file(&mut self, source: &str) {
        if self.pending_close.is_some() || !self.files.iter().any(|file| file.source.id == source) {
            return;
        }
        let ids = self
            .tabs
            .iter()
            .filter(|tab| tab.ds_id == source)
            .map(|tab| tab.id)
            .collect();
        self.request_close_ids(ids, Some(source.to_owned()));
    }

    fn request_close_ids(&mut self, ids: HashSet<u64>, cleanup_source: Option<String>) {
        self.stage_active_cell();
        let mut sql_consoles = HashSet::new();
        for tab in self.tabs.iter_mut().filter(|tab| ids.contains(&tab.id)) {
            // An inactive document may still have a buffer from older session
            // navigation. Include it in the same staging and warning path.
            for result in &mut tab.results {
                result.commit_find_buffer();
            }
            if matches!(&tab.kind, TabKind::Console { sql } if !sql.is_empty()) {
                // Configuration stores only open consoles, so even previously
                // saved SQL disappears when its console is closed.
                sql_consoles.insert(tab.id);
            }
        }
        let request = CloseRequest {
            ids,
            sql_consoles,
            cleanup_source,
            focus_cancel: true,
        };
        if request.risks(&self.tabs).requires_confirmation() {
            self.pending_close = Some(request);
        } else {
            self.finish_close_tabs(request);
        }
    }

    pub(super) fn close_tabs_dialog(&mut self, ctx: &Context) {
        // A late DDL event may have opened another tab while this modal was up.
        // Closing its source must include that tab and show its risks as well.
        if let Some(mut request) = self.pending_close.take() {
            self.include_new_file_tabs(&mut request);
            self.pending_close = Some(request);
        }
        let Some(request) = &mut self.pending_close else {
            return;
        };
        let risks = request.risks(&self.tabs);
        if risks.tabs == 0 && request.cleanup_source.is_none() {
            self.pending_close = None;
            return;
        }
        let mut cancel = false;
        let mut close = false;
        let mut stop = false;
        let title = if request.cleanup_source.is_some() { "Close File" } else if risks.tabs == 1 { "Close Tab" } else { "Close Tabs" };
        let response = theme::Dialog::new(egui::Id::new("close_tabs_confirmation"), title)
            .resizable(false)
            .default_size([480.0, 200.0])
            .show(ctx, |ui| {
            ui.set_width(480.0_f32.min((ctx.content_rect().width() - 40.0).max(240.0)));
            ui.label(RichText::new("These changes will be discarded:").color(theme::TEXT_MUTED));
            ui.add_space(6.0);
            egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                for tab in self.tabs.iter().filter(|tab| request.ids.contains(&tab.id)) {
                    ui.add(egui::Label::new(RichText::new(&tab.title).font(theme::semibold(13.0))).truncate())
                        .on_hover_text(&tab.title);
                    // One short line per risk, indented under the tab name.
                    let mut risk = |text: &str, waiting: bool| {
                        ui.horizontal(|ui| {
                            ui.add_space(12.0);
                            ui.label(RichText::new(text).color(if waiting { theme::SEARCH_MARKER } else { theme::TEXT_MUTED }));
                        });
                    };
                    if tab.results.iter().any(|rs| !rs.edits.is_empty()) {
                        risk("Pending cell and row changes", false);
                    }
                    if tab.in_tx {
                        risk("Open transaction, rolled back when its connection closes", false);
                    }
                    if tab.unsaved_writes {
                        risk("Changes not written back to the source file", false);
                    }
                    if request.sql_consoles.contains(&tab.id) {
                        risk("Console SQL, removed from the saved workspace", false);
                    }
                    if tab.is_running() {
                        risk("A request is running: stop it or wait before closing", true);
                    }
                    if tab.context_pending.is_some() {
                        risk("A schema change is in progress: wait before closing", true);
                    }
                    if tab.saving_file {
                        risk("The source file is being saved: wait before closing", true);
                    }
                    ui.add_space(8.0);
                }
            });
            if risks.running != 0 {
                ui.label(RichText::new("Stopping a request does not undo statements that already committed.").small().color(theme::TEXT_MUTED));
                stop = ui.button("Stop Running Queries").clicked();
            }
            theme::dialog_footer(
                ui,
                |_| {},
                |ui| {
                    // Right to left: Cancel is the default, the destructive action stays secondary.
                    let button = theme::dialog_button(ui, "Cancel");
                    if std::mem::take(&mut request.focus_cancel) {
                        button.request_focus();
                    }
                    cancel = button.clicked();
                    close = ui
                        .add_enabled(
                            risks.running == 0 && risks.saving == 0 && risks.context_changes == 0,
                            egui::Button::new("Close and Discard").min_size(egui::vec2(72.0, 26.0)),
                        )
                        .clicked();
                },
            );
        });
        cancel |= response.should_close();
        if cancel {
            self.pending_close = None;
            self.focus_active_editor(ctx);
        } else if close {
            let request = self.pending_close.take().unwrap();
            self.finish_close_tabs(request);
            self.focus_active_editor(ctx);
        } else if stop {
            for tab in self
                .tabs
                .iter()
                .filter(|tab| request.ids.contains(&tab.id) && tab.is_running())
            {
                self.backend.cancel(tab.id);
            }
        }
    }

    fn include_new_file_tabs(&mut self, request: &mut CloseRequest) -> bool {
        let Some(source) = &request.cleanup_source else {
            return false;
        };
        let mut changed = false;
        for tab in self.tabs.iter_mut().filter(|tab| tab.ds_id == *source) {
            if request.ids.insert(tab.id) {
                changed = true;
                for result in &mut tab.results {
                    result.commit_find_buffer();
                }
                if matches!(&tab.kind, TabKind::Console { sql } if !sql.is_empty()) {
                    request.sql_consoles.insert(tab.id);
                }
            }
        }
        changed
    }

    fn finish_close_tabs(&mut self, mut request: CloseRequest) {
        // Removing a session's map entry cannot abort a task holding its Arc.
        // Keep the document until RunFinished instead of losing its outcome.
        let added = self.include_new_file_tabs(&mut request);
        let risks = request.risks(&self.tabs);
        if added || risks.running != 0 || risks.saving != 0 || risks.context_changes != 0 {
            self.pending_close = Some(request);
            return;
        }
        let order: Vec<_> = self.tabs.iter().map(|tab| tab.id).collect();
        let active = active_after_close(&order, self.active, &request.ids);
        let previous_len = self.tabs.len();
        self.tabs.retain(|tab| {
            if request.ids.contains(&tab.id) {
                self.backend.close_session(&tab.ds_id, tab.id);
                false
            } else {
                true
            }
        });
        let removed_source = request.cleanup_source.is_some();
        if let Some(source) = request.cleanup_source {
            self.cleanup_opened_file(&source);
        }
        if self.tabs.len() != previous_len || removed_source {
            self.related_rows.cancel();
            self.explorer_ui.cancel_reveal();
            self.active = active;
            self.reveal_active_tab = true;
            self.pending_editor_focus = true;
            self.save_config();
            self.save_session(true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CloseRisks, CloseScope, active_after_close, adjacent_index, close_targets};
    use crate::files::{CsvTable, FileKind, OpenedFile};
    use crate::model::{DataSource, DriverKind, Page, QueryResult};
    use crate::ui::actions::tests::isolated_app;
    use crate::ui::{App, RunKind, TabKind};
    use std::collections::HashSet;

    fn file_app() -> App {
        let mut app = isolated_app(&egui::Context::default());
        let mut source = DataSource::new(DriverKind::Sqlite);
        source.id = "file:unit-test".into();
        app.files.push(OpenedFile {
            origin: "unit-test.csv".into(),
            kind: FileKind::Csv,
            db_path: "unit-test.sqlite".into(),
            tables: vec![CsvTable {
                table: "records".into(),
                path: "unit-test.csv".into(),
                delimiter: b',',
            }],
            source,
        });
        for title in ["first", "second"] {
            app.add_tab(
                title.into(),
                "file:unit-test".into(),
                TabKind::Console { sql: String::new() },
            );
        }
        app
    }

    #[test]
    fn only_imported_open_files_become_dirty_on_writes() {
        let mut app = file_app();
        let file = app.files.remove(0);
        let id = app.tabs[0].id;
        let changed = || QueryResult {
            affected: Some(1),
            ..Default::default()
        };
        app.statement_done(
            id,
            "update records set x = 1".into(),
            Page::first(500),
            Ok(changed()),
        );
        assert!(
            !app.tabs[0].unsaved_writes,
            "permanent databases have no file writeback"
        );
        app.files.push(OpenedFile {
            kind: FileKind::Sqlite,
            ..file
        });
        app.statement_done(
            id,
            "update records set x = 1".into(),
            Page::first(500),
            Ok(changed()),
        );
        assert!(
            !app.tabs[0].unsaved_writes,
            "native SQLite already writes its own file"
        );
        for kind in [FileKind::Csv, FileKind::DataPackage] {
            app.files[0].kind = kind;
            app.statement_done(
                id,
                "update records set x = 1".into(),
                Page::first(500),
                Ok(changed()),
            );
            assert!(app.tabs[0].unsaved_writes);
            app.tabs[0].unsaved_writes = false;
        }
        app.statement_done(id, "COMMIT".into(), Page::first(500), Ok(changed()));
        assert!(
            !app.tabs[0].unsaved_writes,
            "transaction control does not create new writes"
        );
    }

    #[test]
    fn saving_retains_dirty_until_success_and_blocks_new_queries_for_the_source() {
        let mut app = file_app();
        app.tabs[0].unsaved_writes = true;
        let source = app.queue_file_save(0).unwrap();
        let (id, _, _) = app.begin_queued_file_save(&source).unwrap();
        assert!(app.tabs[0].unsaved_writes);
        assert!(app.tabs[0].saving_file);
        assert!(!app.tabs[0].file_save_queued);
        assert!(app.source_is_saving(&source));
        app.start_run(
            1,
            vec!["update records set x = 2".into()],
            Page::first(500),
            RunKind::Query,
        );
        assert!(
            app.tabs[1].running.is_none(),
            "blocked before any backend request"
        );
        assert!(
            app.tabs[1]
                .log
                .last()
                .unwrap()
                .text
                .contains("finish saving")
        );
        assert_eq!(app.complete_file_save(id, Ok(1)), Some(source.clone()));
        assert!(!app.tabs[0].unsaved_writes);
        assert!(!app.source_is_saving(&source));
        assert_eq!(
            app.complete_file_save(id, Err("late duplicate".into())),
            None
        );
        assert!(!app.tabs[0].unsaved_writes);
    }

    #[test]
    fn queued_writes_survive_another_save_and_failures_do_not_retry_forever() {
        let mut app = file_app();
        app.tabs[0].unsaved_writes = true;
        let source = app.queue_file_save(0).unwrap();
        let (first, _, _) = app.begin_queued_file_save(&source).unwrap();
        // This request began before the other tab's save. Its result may arrive
        // while that save is running, and must queue another write afterwards.
        app.tabs[1].running = Some(RunKind::Query);
        let second = app.tabs[1].id;
        app.statement_done(
            second,
            "update records set x = 2".into(),
            Page::first(500),
            Ok(QueryResult {
                affected: Some(1),
                ..Default::default()
            }),
        );
        assert!(app.tabs[1].unsaved_writes);
        app.queue_file_save(1);
        assert!(app.begin_queued_file_save(&source).is_none());
        app.tabs[1].running = None;
        app.complete_file_save(first, Err("permission denied".into()));
        assert!(app.tabs[0].unsaved_writes);
        assert!(!app.tabs[0].file_save_queued);
        let (next, _, _) = app.begin_queued_file_save(&source).unwrap();
        assert_eq!(next, second);
        app.complete_file_save(second, Err("disk full".into()));
        assert!(app.tabs.iter().all(|tab| tab.unsaved_writes));
        assert!(app.begin_queued_file_save(&source).is_none());
        // An explicit later run is allowed to request a retry.
        app.queue_file_save(0);
        assert_eq!(app.begin_queued_file_save(&source).unwrap().0, first);
    }

    #[test]
    fn saves_wait_for_transactions_and_running_requests_without_dropping_queue() {
        let mut app = file_app();
        let source = app.files[0].source.id.clone();
        app.tabs[0].unsaved_writes = true;
        app.tabs[0].in_tx = true;
        assert_eq!(app.queue_file_save(0), None);
        app.tabs[0].in_tx = false;
        app.tabs[0].running = Some(RunKind::Page(0));
        assert_eq!(app.queue_file_save(0), Some(source.clone()));
        assert!(app.begin_queued_file_save(&source).is_none());
        assert!(app.tabs[0].file_save_queued);
        app.tabs[0].running = None;
        assert!(app.begin_queued_file_save(&source).is_some());
        assert_eq!(
            app.queue_file_save(0),
            None,
            "an in-flight owner does not queue itself again"
        );
    }

    #[test]
    fn close_file_uses_shared_confirmation_and_cannot_remove_a_saving_owner() {
        let mut app = file_app();
        let source = app.files[0].source.id.clone();
        app.tabs[0].unsaved_writes = true;
        app.tabs[0].saving_file = true;
        app.request_close_file(&source);
        let request = app.pending_close.as_ref().unwrap();
        assert_eq!(request.cleanup_source.as_deref(), Some(source.as_str()));
        assert_eq!(request.ids.len(), 2);
        let risks = request.risks(&app.tabs);
        assert_eq!(risks.saving, 1);
        assert_eq!(risks.running, 0);
        assert_eq!(risks.file_writes, 1);
        let request = app.pending_close.take().unwrap();
        app.finish_close_tabs(request);
        assert!(
            app.pending_close.is_some(),
            "save still prevents finalization"
        );
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.files.len(), 1);
        app.pending_close = None; // Cancel performs no backend or file cleanup.
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.files.len(), 1);
    }

    #[test]
    fn close_file_includes_late_source_tabs_without_touching_other_sources() {
        let mut app = file_app();
        let source = app.files[0].source.id.clone();
        app.tabs[0].unsaved_writes = true;
        app.request_close_file(&source);
        app.add_tab(
            "late".into(),
            source.clone(),
            TabKind::Console {
                sql: "select 1".into(),
            },
        );
        let late = app.tabs.last().unwrap().id;
        app.add_tab(
            "unrelated".into(),
            "other".into(),
            TabKind::Console {
                sql: "select 2".into(),
            },
        );
        let other = app.tabs.last().unwrap().id;
        let mut request = app.pending_close.take().unwrap();
        assert!(app.include_new_file_tabs(&mut request));
        assert!(request.ids.contains(&late));
        assert!(request.sql_consoles.contains(&late));
        assert!(!request.ids.contains(&other));
        assert!(!app.include_new_file_tabs(&mut request));
    }

    #[test]
    fn navigation_wraps_without_overflow_and_handles_empty_tabs() {
        assert_eq!(adjacent_index(0, 0, false), None);
        assert_eq!(adjacent_index(1, 0, false), Some(0));
        assert_eq!(adjacent_index(1, 0, true), Some(0));
        assert_eq!(adjacent_index(4, 3, false), Some(0));
        assert_eq!(adjacent_index(4, 0, true), Some(3));
        assert_eq!(adjacent_index(4, 2, false), Some(3));
        assert_eq!(adjacent_index(4, usize::MAX, true), Some(2));
    }

    #[test]
    fn close_scopes_use_the_context_target_instead_of_the_active_index() {
        let ids = [11, 42, 7, 90];
        assert_eq!(close_targets(&ids, 42, CloseScope::Tab), [42]);
        assert_eq!(close_targets(&ids, 42, CloseScope::OtherTabs), [11, 7, 90]);
        assert_eq!(close_targets(&ids, 42, CloseScope::TabsToRight), [7, 90]);
        assert_eq!(close_targets(&ids, 42, CloseScope::AllTabs), ids);
        assert!(close_targets(&ids, 90, CloseScope::TabsToRight).is_empty());
        for scope in [
            CloseScope::Tab,
            CloseScope::OtherTabs,
            CloseScope::TabsToRight,
            CloseScope::AllTabs,
        ] {
            assert!(close_targets(&ids, 999, scope).is_empty());
            assert!(close_targets(&[], 42, scope).is_empty());
        }
    }

    #[test]
    fn a_confirmation_keeps_target_ids_when_documents_are_reordered_or_opened() {
        let closed: HashSet<_> = close_targets(&[1, 2, 3], 2, CloseScope::OtherTabs)
            .into_iter()
            .collect();
        let reordered = [3, 4, 2, 1];
        let remaining: Vec<_> = reordered
            .into_iter()
            .filter(|id| !closed.contains(id))
            .collect();
        assert_eq!(remaining, [4, 2]);
        assert_eq!(active_after_close(&reordered, 2, &closed), 1);
    }

    #[test]
    fn active_document_survives_or_uses_the_nearest_surviving_neighbor() {
        let ids = [1, 2, 3, 4, 5];
        for (active, closed, expected) in [
            (2, vec![1, 4], Some(3)),
            (1, vec![2, 3], Some(4)),
            (4, vec![4, 5], Some(3)),
            (0, vec![1, 2, 3, 4, 5], None),
        ] {
            let closed: HashSet<_> = closed.into_iter().collect();
            let next = active_after_close(&ids, active, &closed);
            let remaining: Vec<_> = ids.into_iter().filter(|id| !closed.contains(id)).collect();
            assert_eq!(remaining.get(next).copied(), expected);
            if remaining.is_empty() {
                assert_eq!(next, 0);
            }
        }
    }

    #[test]
    fn every_nonempty_subset_preserves_active_identity_and_nearest_neighbor() {
        let ids = [10, 20, 30, 40, 50, 60];
        for mask in 0..1_u32 << ids.len() {
            let closed: HashSet<_> = ids
                .iter()
                .enumerate()
                .filter(|(i, _)| mask & (1 << i) != 0)
                .map(|(_, &id)| id)
                .collect();
            let remaining: Vec<_> = ids
                .iter()
                .filter(|id| !closed.contains(id))
                .copied()
                .collect();
            for active in 0..ids.len() {
                let expected = ids[active..]
                    .iter()
                    .find(|id| !closed.contains(id))
                    .or_else(|| ids[..active].iter().rev().find(|id| !closed.contains(id)))
                    .copied();
                let index = active_after_close(&ids, active, &closed);
                assert_eq!(
                    remaining.get(index).copied(),
                    expected,
                    "mask={mask}, active={active}"
                );
            }
        }
    }

    #[test]
    fn each_loss_reason_requires_confirmation() {
        assert!(!CloseRisks::default().requires_confirmation());
        for risks in [
            CloseRisks {
                staged: 1,
                ..Default::default()
            },
            CloseRisks {
                transactions: 1,
                ..Default::default()
            },
            CloseRisks {
                file_writes: 1,
                ..Default::default()
            },
            CloseRisks {
                running: 1,
                ..Default::default()
            },
            CloseRisks {
                saving: 1,
                ..Default::default()
            },
            CloseRisks {
                context_changes: 1,
                ..Default::default()
            },
            CloseRisks {
                sql_consoles: 1,
                ..Default::default()
            },
        ] {
            assert!(risks.requires_confirmation());
        }
    }
    #[test]
    fn closing_waits_for_the_schema_acknowledgement() {
        let mut app = file_app();
        let id = app.tabs[0].id;
        app.tabs[0].context_pending = Some(crate::model::ExecutionContext::default());
        app.request_close_tabs(id, CloseScope::Tab);
        let request = app
            .pending_close
            .take()
            .expect("pending change needs a close dialog");
        assert_eq!(request.risks(&app.tabs).context_changes, 1);
        app.finish_close_tabs(request);
        assert!(app.tabs.iter().any(|tab| tab.id == id));
        assert!(
            app.pending_close.is_some(),
            "the schema operation still owns this tab"
        );
    }
}
