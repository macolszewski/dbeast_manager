//! Palette commands use the same source text, history and search state as the editor.

use super::{App, Field, Tab, TabKind, field_id, format_console, search_actions};
use crate::model::DriverKind;
use crate::ui::actions::ActionId;
use egui::Context;
use std::ops::Range;

/// Clamp a source caret inside a collapsed region to its placeholder. Work is
/// performed on an explicit command, never during an ordinary editor repaint.
fn visible_offset(sql: &str, folds: &[Range<usize>], source: usize) -> usize {
    let (mut byte, mut real, mut removed) = (0, 0, 0);
    for fold in folds {
        let start = real + sql[byte..fold.start].chars().count();
        if source <= start {
            return source.saturating_sub(removed);
        }
        let end = start + sql[fold.clone()].chars().count();
        if source < end {
            return start - removed;
        }
        removed += end - start - 1;
        byte = fold.end;
        real = end;
    }
    source
        .min(real + sql[byte..].chars().count())
        .saturating_sub(removed)
}

fn restore_source_selection(tab: &Tab, ctx: &Context) {
    if let (TabKind::Console { sql }, Some(selection)) = (&tab.kind, &tab.cursor) {
        let visible = visible_offset(sql, &tab.folds, selection.start)
            ..visible_offset(sql, &tab.folds, selection.end);
        search_actions::set_selection(ctx, field_id(tab.id, Field::Console), visible);
    }
}

pub(in crate::ui) fn request_action_focus(tab: &Tab, ctx: &Context) {
    ctx.memory_mut(|memory| memory.request_focus(field_id(tab.id, Field::Console)));
}

pub(in crate::ui) fn action_has_focus(tab: &Tab, ctx: &Context) -> bool {
    matches!(tab.kind, TabKind::Console { .. })
        && (ctx.memory(|memory| memory.has_focus(field_id(tab.id, Field::Console)))
            || tab.search.action_has_focus(ctx, tab.id))
}

impl App {
    pub(in crate::ui) fn apply_console_action(
        &mut self,
        index: usize,
        action: ActionId,
        ctx: &Context,
    ) -> bool {
        let Some(tab) = self.tabs.get(index) else {
            return false;
        };
        if !matches!(tab.kind, TabKind::Console { .. })
            || (tab.bound_query.is_some()
                && matches!(action, ActionId::Replace | ActionId::ReformatCode))
        {
            return false;
        }
        let driver = self
            .source(&tab.ds_id)
            .map_or(DriverKind::Postgres, |ds| ds.driver);
        let tab = &mut self.tabs[index];
        let mut save = false;
        match action {
            ActionId::Find => search_actions::open(ctx, tab, false),
            ActionId::Replace => search_actions::open(ctx, tab, true),
            ActionId::GoToLineColumn => search_actions::open_goto(tab),
            ActionId::ReformatCode => {
                let time = ctx.input(|input| input.time);
                let TabKind::Console { sql } = &tab.kind else {
                    unreachable!()
                };
                tab.editor_history
                    .sync(sql, tab.cursor.clone().unwrap_or(0..0), time, false);
                tab.editor_history.checkpoint();
                match format_console(tab, driver) {
                    Ok(()) => {
                        tab.editor_cache.invalidate();
                        tab.search.dirty = true;
                        let TabKind::Console { sql } = &tab.kind else {
                            unreachable!()
                        };
                        tab.editor_history.sync(
                            sql,
                            tab.cursor.clone().unwrap_or(0..0),
                            time,
                            true,
                        );
                        tab.editor_history.checkpoint();
                        save = true;
                    }
                    Err(error) => tab.log(crate::ui::results::LogKind::Error, error),
                }
                restore_source_selection(tab, ctx);
                ctx.memory_mut(|memory| memory.request_focus(field_id(tab.id, Field::Console)));
            }
            ActionId::FoldAll | ActionId::UnfoldAll => {
                tab.folds.clear();
                if action == ActionId::FoldAll {
                    let TabKind::Console { sql } = &tab.kind else {
                        unreachable!()
                    };
                    let mut regions = crate::folding::regions(sql, driver);
                    regions.sort_by_key(|region| {
                        (region.range.start, std::cmp::Reverse(region.range.end))
                    });
                    for region in regions {
                        if tab
                            .folds
                            .last()
                            .is_none_or(|last| last.end <= region.range.start)
                        {
                            tab.folds.push(region.range);
                        }
                    }
                }
                tab.editor_cache.invalidate_folds();
                restore_source_selection(tab, ctx);
                ctx.memory_mut(|memory| memory.request_focus(field_id(tab.id, Field::Console)));
            }
            _ => return false,
        }
        if save {
            self.save_config();
        }
        if matches!(
            action,
            ActionId::ReformatCode | ActionId::FoldAll | ActionId::UnfoldAll
        ) {
            self.focus_active_editor(ctx);
        }
        ctx.request_repaint();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::visible_offset;

    #[test]
    fn commands_project_source_carets_across_unicode_and_multiple_folds() {
        let sql = "ą first\nżółw\n last β hidden end";
        let folds = [
            sql.find("first").unwrap()..sql.find(" last").unwrap(),
            sql.find("hidden").unwrap()..sql.find(" end").unwrap(),
        ];
        let visible = crate::folding::visible(sql, &folds);
        for source in 0..=sql.chars().count() {
            let projected = visible_offset(sql, &folds, source);
            assert!(projected <= visible.chars().count());
            let restored = crate::folding::to_real_char(&visible, sql, &folds, projected);
            let byte = sql
                .char_indices()
                .nth(source)
                .map_or(sql.len(), |(byte, _)| byte);
            let expected = folds
                .iter()
                .find(|fold| fold.start < byte && byte < fold.end)
                .map_or(source, |fold| sql[..fold.start].chars().count());
            assert_eq!(restored, expected);
        }
        assert_eq!(visible_offset(sql, &[], usize::MAX), sql.chars().count());
    }
}
