//! Undo history for real SQL, independent of the editor's folded placeholders.
//!
//! Undo points are grouped like egui's `Undoer`: a new point is made once the
//! text has stayed the same for [`STABLE_SECONDS`], or every
//! [`AUTO_SAVE_SECONDS`] while it keeps changing. Unlike `Undoer`, the text is
//! copied only when a point is made, and the points are bounded by bytes as
//! well as by count.

use std::{cell::Cell, collections::VecDeque, ops::Range, rc::Rc, sync::Arc};

const STABLE_SECONDS: f64 = 1.0;
const AUTO_SAVE_SECONDS: f64 = 30.0;
/// Undo points kept at most.
const MAX_POINTS: usize = 100;
/// SQL text undo and redo points keep together; the two newest undo points
/// are kept even when they alone are larger, so one step can always be undone.
const HISTORY_BYTES: usize = 16 << 20;

#[derive(Clone)]
struct Snapshot {
    sql: Arc<str>,
    // A cursor movement updates the existing text revision, including the
    // copy already held as an undo point, without making it an undo step.
    cursor: Rc<Cell<(usize, usize)>>,
    revision: u64,
}

/// The text being edited. `text` is reused from frame to frame, so typing
/// copies into an existing buffer instead of allocating a new document.
struct Current {
    text: String,
    /// The same text as an undo point, once one was made from it.
    shared: Option<Arc<str>>,
    cursor: Rc<Cell<(usize, usize)>>,
    revision: u64,
}

/// A run of changes not yet recorded as an undo point.
struct Flux {
    start: f64,
    latest_change: f64,
    revision: u64,
}

/// Raw SQL snapshots; folding and unfolding must not be reported as text edits.
#[derive(Default)]
pub(in crate::ui) struct EditorHistory {
    current: Option<Current>,
    /// Oldest first; adjacent points never share a revision.
    undos: VecDeque<Snapshot>,
    /// Points undone since the last change, most recently undone last.
    redos: Vec<Snapshot>,
    flux: Option<Flux>,
    next_revision: u64,
}

impl EditorHistory {
    /// Feed the current SQL and real character selection to the undo grouper.
    ///
    /// The first call initializes history regardless of `changed`. Later calls
    /// with `changed == false` trust that SQL is unchanged: idle frames perform
    /// no text comparison or allocation. External SQL mutations must pass true.
    pub(super) fn sync(&mut self, sql: &str, cursor: Range<usize>, time: f64, changed: bool) {
        let cursor = (cursor.start, cursor.end);
        match &mut self.current {
            None => {
                self.current = Some(Current {
                    text: sql.to_owned(),
                    shared: None,
                    cursor: Rc::new(Cell::new(cursor)),
                    revision: self.next_revision,
                });
                self.next_revision += 1;
                self.add_undo();
                return;
            }
            Some(current) if changed && current.text != sql => {
                current.text.clear();
                current.text.push_str(sql);
                current.shared = None;
                current.cursor = Rc::new(Cell::new(cursor));
                current.revision = self.next_revision;
                self.next_revision += 1;
            }
            Some(current) => current.cursor.set(cursor),
        }
        self.feed(time);
    }

    /// End the current typing group. Call before and after an atomic operation
    /// such as Replace All, with `sync(..., true)` after changing the SQL.
    pub(super) fn checkpoint(&mut self) {
        if self.current.is_some() {
            self.add_undo();
        }
    }

    /// Restore raw SQL and its character selection, or return `None` at the end
    /// of history. The caller rebuilds the editor without stale fold mappings.
    pub(super) fn undo(&mut self, redo: bool) -> Option<(String, Range<usize>)> {
        let revision = self.current.as_ref()?.revision;
        let restored = if redo {
            if self.undos.back().is_some_and(|back| back.revision != revision) {
                // Changed since the last undo: there is nothing to redo.
                self.redos.clear();
                return None;
            }
            let point = self.redos.pop()?;
            self.undos.push_back(point);
            self.undos.back()?.clone()
        } else {
            let has_undo = match self.undos.len() {
                0 => false,
                1 => self.undos[0].revision != revision,
                _ => true,
            };
            if !has_undo {
                return None;
            }
            self.flux = None;
            if self.undos.back().is_some_and(|back| back.revision == revision) {
                let point = self.undos.pop_back()?;
                self.redos.push(point);
            } else {
                let point = self.snapshot();
                self.redos.push(point);
            }
            self.undos.back()?.clone()
        };
        let (start, end) = restored.cursor.get();
        let sql = restored.sql.to_string();
        let current = self.current.as_mut()?;
        current.text.clear();
        current.text.push_str(&restored.sql);
        current.shared = Some(restored.sql);
        current.cursor = restored.cursor;
        current.revision = restored.revision;
        self.trim();
        Some((sql, start..end))
    }

    /// Decides whether the current text becomes an undo point now.
    fn feed(&mut self, time: f64) {
        let Some(current) = &self.current else {
            return;
        };
        let revision = current.revision;
        if self.undos.back().is_none_or(|back| back.revision == revision) {
            if self.undos.is_empty() {
                self.add_undo();
            }
            self.flux = None;
            return;
        }
        self.redos.clear();
        match &mut self.flux {
            None => {
                self.flux = Some(Flux {
                    start: time,
                    latest_change: time,
                    revision,
                });
            }
            Some(flux) if flux.revision == revision => {
                if time - flux.latest_change >= STABLE_SECONDS {
                    self.add_undo();
                }
            }
            Some(flux) => {
                if time - flux.start >= AUTO_SAVE_SECONDS {
                    self.add_undo();
                } else {
                    flux.latest_change = time;
                    flux.revision = revision;
                }
            }
        }
    }

    /// The current text as an undo point, copying it at most once.
    fn snapshot(&mut self) -> Snapshot {
        let current = self.current.as_mut().expect("history is initialized");
        let sql = current
            .shared
            .get_or_insert_with(|| Arc::from(current.text.as_str()))
            .clone();
        Snapshot {
            sql,
            cursor: current.cursor.clone(),
            revision: current.revision,
        }
    }

    fn add_undo(&mut self) {
        let revision = self.current.as_ref().map(|current| current.revision);
        if self.undos.back().map(|back| back.revision) != revision {
            let point = self.snapshot();
            self.undos.push_back(point);
        }
        self.trim();
        self.flux = None;
    }

    /// Drops the oldest undo points beyond the count or byte budget.
    fn trim(&mut self) {
        while self.undos.len() > 2
            && (self.undos.len() > MAX_POINTS || self.owned_bytes() > HISTORY_BYTES)
        {
            self.undos.pop_front();
        }
    }

    /// Text bytes held by undo and redo points.
    fn owned_bytes(&self) -> usize {
        self.undos
            .iter()
            .chain(&self.redos)
            .map(|point| point.sql.len())
            .sum()
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
        let initial_sql = Arc::clone(&history.undos.back().unwrap().sql);
        let initial_revision = history.current.as_ref().unwrap().revision;
        for frame in 1..100 {
            history.sync(sql, 4..6, f64::from(frame), false);
        }
        history.sync(sql, 4..6, 100.0, true);
        assert!(Arc::ptr_eq(&initial_sql, &history.undos.back().unwrap().sql));
        assert_eq!(history.undos.len(), 1);
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

    #[test]
    fn large_documents_keep_history_within_the_byte_budget() {
        let mut history = EditorHistory::default();
        let doc = |i: usize| format!("-- revision {i}\n{}", "ż".repeat(2 << 20));
        history.sync(&doc(0), 0..0, 0.0, false);
        // Fifty distinct, stable revisions of a 4 MiB document.
        for i in 1..=50 {
            let t = f64::from(i as u32) * 2.0;
            history.sync(&doc(i), 0..0, t, true);
            history.sync(&doc(i), 0..0, t + 1.5, false);
        }
        assert!(history.owned_bytes() <= HISTORY_BYTES, "{}", history.owned_bytes());
        assert!(history.undos.len() >= 2);
        // The newest points are the ones kept, in order.
        assert_eq!(history.undo(false).map(|(sql, _)| sql), Some(doc(49)));
        assert_eq!(history.undo(false).map(|(sql, _)| sql), Some(doc(48)));
        assert_eq!(history.undo(true).map(|(sql, _)| sql), Some(doc(49)));
        assert!(history.owned_bytes() <= HISTORY_BYTES);
    }

    #[test]
    fn a_document_larger_than_the_budget_still_has_one_undo_step() {
        let mut history = EditorHistory::default();
        let big = "x".repeat(HISTORY_BYTES + 1);
        history.sync(&big, 0..0, 0.0, false);
        history.sync(&format!("{big}y"), 0..0, 0.1, true);
        history.checkpoint();
        history.sync(&format!("{big}yz"), 0..0, 0.2, true);
        history.checkpoint();
        assert_eq!(history.undos.len(), 2);
        assert_eq!(history.undo(false).map(|(sql, _)| sql.len()), Some(big.len() + 1));
    }

    #[test]
    fn typing_copies_text_only_when_an_undo_point_is_made() {
        let mut history = EditorHistory::default();
        history.sync("", 0..0, 0.0, false);
        let mut sql = String::new();
        for i in 0..200 {
            sql.push('a');
            history.sync(&sql, i + 1..i + 1, 0.001 * i as f64, true);
            assert!(history.current.as_ref().unwrap().shared.is_none());
        }
        assert_eq!(history.undos.len(), 1);
        history.sync(&sql, 200..200, 5.0, false);
        assert_eq!(history.undos.len(), 2);
        assert!(history.current.as_ref().unwrap().shared.is_some());
    }

    #[test]
    fn changes_that_never_settle_are_saved_every_auto_save_interval() {
        let mut history = EditorHistory::default();
        history.sync("", 0..0, 0.0, false);
        let mut sql = String::new();
        for step in 0..=140 {
            sql.push('a');
            history.sync(&sql, 0..0, f64::from(step) * 0.5, true);
        }
        // Two auto saves in 70 seconds of continuous typing.
        assert_eq!(history.undos.len(), 3);
    }
}
