//! Undo history for real SQL, independent of the editor's folded placeholders.

use egui::util::undoer::Undoer;
use std::{cell::Cell, ops::Range, rc::Rc, sync::Arc};

#[derive(Clone)]
struct Snapshot {
    sql: Arc<str>,
    // A cursor movement updates the existing text revision, including any clone
    // already held by Undoer, without turning the movement into an undo step.
    cursor: Rc<Cell<(usize, usize)>>,
    revision: u64,
}

impl PartialEq for Snapshot {
    fn eq(&self, other: &Self) -> bool {
        self.revision == other.revision
    }
}

/// Raw SQL snapshots; folding and unfolding must not be reported as text edits.
#[derive(Default)]
pub(in crate::ui) struct EditorHistory {
    current: Option<Snapshot>,
    undoer: Undoer<Snapshot>,
    next_revision: u64,
}

impl EditorHistory {
    /// Feed the current SQL and real character selection to the undo grouper.
    ///
    /// The first call initializes history regardless of `changed`. Later calls
    /// with `changed == false` trust that SQL is unchanged: idle frames perform
    /// no text comparison or allocation. External SQL mutations must pass true.
    pub(super) fn sync(&mut self, sql: &str, cursor: Range<usize>, time: f64, changed: bool) {
        let text_changed = self
            .current
            .as_ref()
            .is_none_or(|current| changed && current.sql.as_ref() != sql);
        if text_changed {
            self.current = Some(Snapshot {
                sql: Arc::from(sql),
                cursor: Rc::new(Cell::new((cursor.start, cursor.end))),
                revision: self.next_revision,
            });
            self.next_revision += 1;
        } else if let Some(current) = &self.current {
            current.cursor.set((cursor.start, cursor.end));
        }
        if let Some(current) = &self.current {
            self.undoer.feed_state(time, current);
        }
    }

    /// End the current typing group. Call before and after an atomic operation
    /// such as Replace All, with `sync(..., true)` after changing the SQL.
    pub(super) fn checkpoint(&mut self) {
        if let Some(current) = &self.current {
            self.undoer.add_undo(current);
        }
    }

    /// Restore raw SQL and its character selection, or return `None` at the end
    /// of history. The caller rebuilds the editor without stale fold mappings.
    pub(super) fn undo(&mut self, redo: bool) -> Option<(String, Range<usize>)> {
        let current = self.current.as_ref()?;
        let restored = if redo {
            self.undoer.redo(current)
        } else {
            self.undoer.undo(current)
        }?
        .clone();
        let sql = restored.sql.to_string();
        let (start, end) = restored.cursor.get();
        self.current = Some(restored);
        Some((sql, start..end))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_is_grouped_until_a_stable_pause() {
        let mut history = EditorHistory::default();
        history.sync("", 0..0, 0.0, false);
        history.sync("S", 1..1, 0.1, true);
        history.sync("SE", 2..2, 0.2, true);
        history.sync("SELECT", 6..6, 0.3, true);
        history.sync("SELECT", 6..6, 1.4, false);
        history.sync("SELECT 1", 8..8, 1.5, true);
        assert_eq!(history.undo(false), Some(("SELECT".into(), 6..6)));
        assert_eq!(history.undo(false), Some(("".into(), 0..0)));
        assert_eq!(history.undo(false), None);
        assert_eq!(history.undo(true), Some(("SELECT".into(), 6..6)));
        assert_eq!(history.undo(true), Some(("SELECT 1".into(), 8..8)));
        assert_eq!(history.undo(true), None);
    }

    #[test]
    fn replacement_is_atomic_and_restores_its_prior_selection() {
        let mut history = EditorHistory::default();
        history.sync("SELECT a, a", 0..0, 0.0, false);
        history.sync("SELECT a, a", 7..8, 0.1, false);
        history.checkpoint();
        history.sync("SELECT item, item", 7..11, 0.2, true);
        history.checkpoint();
        history.sync("SELECT item, item;", 18..18, 0.3, true);
        assert_eq!(
            history.undo(false),
            Some(("SELECT item, item".into(), 7..11))
        );
        assert_eq!(history.undo(false), Some(("SELECT a, a".into(), 7..8)));
        assert_eq!(
            history.undo(true),
            Some(("SELECT item, item".into(), 7..11))
        );
    }

    #[test]
    fn editing_after_undo_discards_redo() {
        let mut history = EditorHistory::default();
        history.sync("SELECT 1", 8..8, 0.0, false);
        history.sync("SELECT 2", 8..8, 0.1, true);
        history.checkpoint();
        assert_eq!(history.undo(false), Some(("SELECT 1".into(), 8..8)));
        history.sync("SELECT 3", 8..8, 0.2, true);
        assert_eq!(history.undo(true), None);
        assert_eq!(history.undo(false), Some(("SELECT 1".into(), 8..8)));
        assert_eq!(history.undo(true), Some(("SELECT 3".into(), 8..8)));
    }

    #[test]
    fn folding_and_cursor_movement_do_not_create_history_or_copy_sql() {
        let sql = "SELECT (\n    'żółć 😀'\n);";
        let mut history = EditorHistory::default();
        history.sync(sql, 0..0, 0.0, false);
        let initial_sql = Arc::clone(&history.current.as_ref().unwrap().sql);
        let initial_revision = history.current.as_ref().unwrap().revision;
        for frame in 1..100 {
            history.sync(sql, 4..6, f64::from(frame), false);
        }
        history.sync(sql, 4..6, 100.0, true);
        assert!(Arc::ptr_eq(
            &initial_sql,
            &history.current.as_ref().unwrap().sql
        ));
        assert_eq!(initial_revision, history.current.as_ref().unwrap().revision);
        assert_eq!(history.undo(false), None);
        assert_eq!(history.undo(true), None);

        history.sync("SELECT 1", 8..8, 101.0, true);
        assert_eq!(history.undo(false), Some((sql.into(), 4..6)));
        // Synchronizing restored text must retain its revision and redo branch.
        history.sync(sql, 1..3, 102.0, true);
        assert_eq!(history.undo(true), Some(("SELECT 1".into(), 8..8)));
        assert_eq!(history.undo(false), Some((sql.into(), 1..3)));
    }

    #[test]
    fn checkpoints_without_text_changes_are_noops() {
        let mut history = EditorHistory::default();
        assert_eq!(history.undo(false), None);
        history.checkpoint();
        history.sync("", 0..0, 0.0, false);
        history.checkpoint();
        history.checkpoint();
        assert_eq!(history.undo(false), None);
        history.sync("é", 1..1, 0.1, true);
        history.checkpoint();
        history.checkpoint();
        assert_eq!(history.undo(false), Some(("".into(), 0..0)));
        assert_eq!(history.undo(false), None);
    }
}
