//! Search actions operate on source SQL, never on folding placeholders.

use super::{SearchState, Tab, TabKind};
use crate::search;
use egui::text::{CCursor, CCursorRange};
use egui::{Id, Key, KeyboardShortcut, Modifiers, Ui};
use std::ops::Range;

const FIND: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::F);
const REPLACE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::R);
const GO_TO: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::L);
const NEXT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::G);
const PREVIOUS: KeyboardShortcut =
    KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::G);

pub(super) fn open(ctx: &egui::Context, tab: &mut Tab, replace: bool) {
    if let (TabKind::Console { sql }, Some(selection)) = (&tab.kind, &tab.cursor)
        && !selection.is_empty()
    {
        tab.search.query = sql
            .chars()
            .skip(selection.start)
            .take(selection.len())
            .collect();
    }
    tab.search.visible = true;
    tab.search.replace_mode = replace && tab.bound_query.is_none();
    tab.search.focus_query = true;
    tab.search.dirty = true;
    tab.completion = None;
    ctx.request_repaint();
}

pub(super) fn open_goto(tab: &mut Tab) {
    if let TabKind::Console { sql } = &tab.kind {
        let index = tab.cursor.as_ref().map_or(0, |r| r.start);
        let (mut line, mut column) = (1, 1);
        for c in sql.chars().take(index) {
            if c == '\n' {
                line += 1;
                column = 1;
            } else {
                column += 1;
            }
        }
        tab.search.goto_text = format!("{line}:{column}");
    }
    tab.search.goto_open = true;
    tab.search.goto_error = None;
    tab.completion = None;
}

/// Returns whether SQL changed and a visible selection to scroll into view.
pub(super) fn prepare(
    ui: &mut Ui,
    tab: &mut Tab,
    source_changed: bool,
) -> (bool, Option<Range<usize>>) {
    let read_only = tab.bound_query.is_some();
    if read_only {
        tab.search.replace_mode = false;
    }
    let id = super::field_id(tab.id, super::Field::Console);
    let focused = ui.memory(|m| m.focused());
    let owns_focus = focused.is_none() || focused == Some(id) || tab.search.owns_focus(ui, tab.id);
    let owns_escape = focused == Some(id)
        || tab.search.owns_focus(ui, tab.id)
        || (focused.is_none() && ui.memory(|m| m.had_focus_last_frame(id)));
    let mut next = false;
    let mut previous = false;
    if owns_focus && !tab.search.goto_open {
        // More specific shortcuts are checked first; ⌥⌘L remains Format.
        if ui.input_mut(|i| i.consume_shortcut(&FIND)) {
            open(ui.ctx(), tab, false);
        } else if ui.input_mut(|i| i.consume_shortcut(&REPLACE)) {
            open(ui.ctx(), tab, true);
        }
        if ui.input_mut(|i| i.consume_shortcut(&GO_TO)) {
            open_goto(tab);
        }
        previous = ui.input_mut(|i| i.consume_shortcut(&PREVIOUS));
        next = !previous && ui.input_mut(|i| i.consume_shortcut(&NEXT));
        if tab.search.visible
            && owns_escape
            && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape))
        {
            tab.search.visible = false;
            ui.memory_mut(|m| m.request_focus(id));
        }
    }
    let TabKind::Console { sql } = &mut tab.kind else {
        return (false, None);
    };
    let state = &mut tab.search;
    state.dirty |= source_changed;
    let caret = tab.cursor.as_ref().map_or(0, |r| r.start);
    let opened = state.focus_query;
    if state.visible || next || previous {
        refresh(state, sql, caret);
    }
    let action = state.show(ui, tab.id, read_only);
    if state.visible {
        refresh(state, sql, caret);
    }
    let mut selection = None;
    if (opened || action.query_changed) && !state.matches.is_empty() {
        selection = state.active.map(|i| state.matches[i].chars.clone());
    }
    if (next || previous || action.next || action.previous) && !state.matches.is_empty() {
        let backwards = previous || action.previous;
        let index = match state
            .active
            .filter(|&i| tab.cursor.as_ref() == Some(&state.matches[i].chars))
        {
            Some(i) if backwards => (i + state.matches.len() - 1) % state.matches.len(),
            Some(i) => (i + 1) % state.matches.len(),
            None if backwards => state
                .matches
                .partition_point(|m| m.chars.start < caret)
                .checked_sub(1)
                .unwrap_or(state.matches.len() - 1),
            None => state
                .matches
                .iter()
                .position(|m| m.chars.start >= caret)
                .unwrap_or(0),
        };
        state.active = Some(index);
        selection = Some(state.matches[index].chars.clone());
    }
    let mut changed = false;
    if !read_only && (action.replace || action.replace_all) && !state.matches.is_empty() {
        tab.editor_history.checkpoint();
        let start = if action.replace_all {
            0
        } else {
            state.active.unwrap_or(0)
        };
        let replaced_end = state.matches[start].chars.start + state.replacement.chars().count();
        let matches = if action.replace_all {
            &state.matches[..]
        } else {
            &state.matches[start..start + 1]
        };
        let replaced = search::replace_matches(sql, matches, &state.replacement);
        changed = *sql != replaced;
        *sql = replaced;
        if !tab.folds.is_empty() {
            tab.folds.clear();
            tab.editor_cache.invalidate_folds();
        }
        state.dirty = true;
        refresh(state, sql, replaced_end);
        selection = state
            .active
            .map(|i| state.matches[i].chars.clone())
            .or(Some(replaced_end..replaced_end));
        tab.editor_history.sync(
            sql,
            selection.clone().unwrap(),
            ui.input(|i| i.time),
            changed,
        );
        tab.editor_history.checkpoint();
    }
    if let Some(value) = action.goto_submit {
        let mut parts = value.trim().split(':');
        let line = parts.next().and_then(|p| p.trim().parse().ok());
        let column = parts.next().map_or(Some(1), |p| p.trim().parse().ok());
        let at = match (line, column, parts.next()) {
            (Some(line), Some(column), None) => search::caret_at_line_column(sql, line, column),
            _ => None,
        };
        if let Some(at) = at {
            selection = Some(at..at);
            state.goto_open = false;
            state.goto_error = None;
            ui.memory_mut(|m| m.request_focus(id));
        } else {
            state.goto_error =
                Some("Enter an existing line and a positive column, for example 12:3".into());
        }
    }
    if action.return_editor_focus {
        ui.memory_mut(|m| m.request_focus(id));
    }
    let scroll = selection.map(|range| {
        let bytes = char_byte(sql, range.start)..char_byte(sql, range.end);
        let folds_before = tab.folds.len();
        tab.folds.retain(|f| {
            if range.is_empty() {
                !(f.start <= bytes.start && bytes.start < f.end)
            } else {
                f.end <= bytes.start || bytes.end <= f.start
            }
        });
        if tab.folds.len() != folds_before {
            tab.editor_cache.invalidate_folds();
        }
        *tab.cursor.get_or_insert(0..0) = range.clone();
        let visible =
            to_visible(sql, &tab.folds, range.start)..to_visible(sql, &tab.folds, range.end);
        set_selection(ui.ctx(), id, visible.clone());
        ui.ctx().request_repaint();
        visible
    });
    if state.visible {
        project(state, sql, &tab.folds);
    }
    (changed, scroll)
}

fn refresh(state: &mut SearchState, sql: &str, caret: usize) {
    if !state.dirty || (!state.visible && state.query.is_empty()) {
        return;
    }
    state.matches = search::find_all(
        sql,
        &state.query,
        search::SearchOptions {
            match_case: state.match_case,
            whole_words: state.whole_words,
        },
    );
    state.active = (!state.matches.is_empty())
        .then(|| state.matches.partition_point(|m| m.chars.start < caret) % state.matches.len());
    state.dirty = false;
    state.projection_dirty = true;
}

pub(super) fn set_selection(ctx: &egui::Context, id: Id, range: Range<usize>) {
    let mut state = egui::TextEdit::load_state(ctx, id).unwrap_or_default();
    state.cursor.set_char_range(Some(CCursorRange::two(
        CCursor::new(range.start),
        CCursor::new(range.end),
    )));
    state.store(ctx, id);
}

fn char_byte(text: &str, index: usize) -> usize {
    text.char_indices()
        .nth(index)
        .map_or(text.len(), |(b, _)| b)
}

fn to_visible(sql: &str, folds: &[Range<usize>], index: usize) -> usize {
    let byte = char_byte(sql, index);
    index
        - folds
            .iter()
            .take_while(|f| f.end <= byte)
            .map(|f| sql[f.clone()].chars().count() - 1)
            .sum::<usize>()
}

/// Project matches only on source/search/fold changes, not on repaint.
fn project(state: &mut SearchState, sql: &str, folds: &[Range<usize>]) {
    if !state.projection_dirty && state.projected_folds == folds {
        return;
    }
    state.highlights.clear();
    let mut removed = 0;
    let mut ranges = folds.iter().peekable();
    for found in &state.matches {
        while let Some(fold) = ranges.peek().filter(|f| f.end <= found.bytes.start) {
            removed += sql[(*fold).clone()].chars().count() - 1;
            ranges.next();
        }
        if ranges.peek().is_none_or(|f| found.bytes.end <= f.start) {
            state
                .highlights
                .push(found.chars.start - removed..found.chars.end - removed);
        }
    }
    state.projected_folds.clear();
    state.projected_folds.extend_from_slice(folds);
    state.projection_dirty = false;
}

pub(super) fn paint(
    ui: &Ui,
    out: &egui::text_edit::TextEditOutput,
    state: &SearchState,
    shape: egui::layers::ShapeIdx,
    selection: Option<CCursorRange>,
    row_offsets: &[usize],
) {
    if !state.visible || state.highlights.is_empty() {
        return;
    }
    let mut shapes = Vec::new();
    let selection = selection.map(|r| {
        r.primary.index.0.min(r.secondary.index.0)..r.primary.index.0.max(r.secondary.index.0)
    });
    let clip = ui.clip_rect();
    let first_row = out
        .galley
        .rows
        .partition_point(|row| row.rect().bottom() + out.galley_pos.y < clip.top());
    for (row_index, row) in out.galley.rows.iter().enumerate().skip(first_row) {
        let rect = row.rect().translate(out.galley_pos.to_vec2());
        if rect.top() > clip.bottom() {
            break;
        }
        let Some(&offset) = row_offsets.get(row_index) else {
            break;
        };
        let end = offset + row.char_count_including_newline().0;
        if clip.intersects(rect) {
            let first = state.highlights.partition_point(|r| r.end <= offset);
            for found in state.highlights[first..]
                .iter()
                .take_while(|r| r.start < end)
            {
                let left = row.x_offset(egui::text::CharIndex(found.start.saturating_sub(offset)));
                let right = row.x_offset(egui::text::CharIndex(found.end.min(end) - offset));
                let rect = egui::Rect::from_x_y_ranges(
                    (out.galley_pos.x + row.pos.x + left)..=(out.galley_pos.x + row.pos.x + right),
                    rect.y_range(),
                );
                let color = if selection.as_ref() == Some(found) {
                    super::theme::SEARCH_CURRENT_MATCH
                } else {
                    super::theme::SEARCH_MATCH
                };
                shapes.push(egui::Shape::rect_filled(rect, 1.0, color));
            }
        }
    }
    ui.painter().set(shape, egui::Shape::Vec(shapes));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_map_unicode_around_hidden_ranges_and_refresh_after_unfolding() {
        let sql = "żółw SELECT hidden SELECT tail SELECT";
        let mut state = SearchState::default();
        state.query.push_str("SELECT");
        state.visible = true;
        refresh(&mut state, sql, 0);
        let fold = sql.find("hidden").unwrap()..sql.find("tail").unwrap();
        let folds = std::slice::from_ref(&fold);
        project(&mut state, sql, folds);
        let visible = crate::folding::visible(sql, folds);
        assert_eq!(state.highlights.len(), 2);
        for range in &state.highlights {
            assert_eq!(
                visible
                    .chars()
                    .skip(range.start)
                    .take(range.len())
                    .collect::<String>(),
                "SELECT"
            );
        }
        let allocation = state.highlights.as_ptr();
        project(&mut state, sql, folds);
        assert_eq!(allocation, state.highlights.as_ptr());
        project(&mut state, sql, &[]);
        assert_eq!(
            state.highlights,
            state
                .matches
                .iter()
                .map(|m| m.chars.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn selections_retain_character_offsets_after_multiple_folds() {
        let sql = "α hidden β hidden 😀 target";
        let first = sql.find("hidden").unwrap();
        let last = sql.rfind("hidden").unwrap();
        let folds = vec![first..first + 6, last..last + 6];
        let real = sql[..sql.find("target").unwrap()].chars().count();
        let visible = crate::folding::visible(sql, &folds);
        let at = to_visible(sql, &folds, real);
        assert_eq!(visible.chars().skip(at).collect::<String>(), "target");
        assert_eq!(
            crate::folding::to_real_char(&visible, sql, &folds, at),
            real
        );
    }
}
