//! Derived editor data, rebuilt on mutations rather than every repaint.

use super::{folding, highlight, theme};
use crate::{model::DriverKind, splitter};
use egui::{Galley, TextBuffer, Ui, text::CharIndex};
use std::{
    cell::{Cell, OnceCell},
    collections::HashMap,
    ops::Range,
    rc::Rc,
    sync::Arc,
};

/// TextEdit reports changes after layout. The revision lets its layouter notice
/// each mutation immediately, without comparing or cloning the whole buffer.
#[derive(Default)]
pub(super) struct EditorText {
    text: String,
    pub(super) revision: Rc<Cell<u64>>,
}

impl EditorText {
    pub(super) fn as_str(&self) -> &str {
        &self.text
    }

    fn replace_source(&mut self, text: String) {
        self.text = text;
        self.mark_changed();
    }

    fn mark_changed(&self) {
        self.revision.set(self.revision.get().wrapping_add(1));
    }

    pub(super) fn edit(&mut self, edit: impl FnOnce(&mut String) -> bool) -> bool {
        let changed = edit(&mut self.text);
        if changed {
            self.mark_changed();
        }
        changed
    }
}

impl TextBuffer for EditorText {
    fn is_mutable(&self) -> bool {
        true
    }
    fn as_str(&self) -> &str {
        &self.text
    }
    fn insert_text(&mut self, text: &str, index: CharIndex) -> usize {
        let inserted = self.text.insert_text(text, index);
        if inserted != 0 {
            self.mark_changed();
        }
        inserted
    }
    fn delete_char_range(&mut self, range: Range<CharIndex>) {
        let before = self.text.len();
        self.text.delete_char_range(range);
        if self.text.len() != before {
            self.mark_changed();
        }
    }
    fn clear(&mut self) {
        if !self.text.is_empty() {
            self.text.clear();
            self.mark_changed();
        }
    }
    fn replace_with(&mut self, text: &str) {
        self.text.replace_with(text);
        self.mark_changed();
    }
    fn take(&mut self) -> String {
        self.mark_changed();
        std::mem::take(&mut self.text)
    }
    fn type_id(&self) -> std::any::TypeId {
        std::any::TypeId::of::<Self>()
    }
}

pub(super) struct FoldSummary {
    pub(super) text: String,
    pub(super) width: f32,
    pub(super) preview: OnceCell<String>,
}

#[derive(Default)]
pub(super) struct Projection {
    revision: Option<u64>,
    line_starts: Vec<(usize, usize)>,
    fold_offsets: Vec<(usize, usize)>,
    char_count: usize,
    pub(super) placeholders: Vec<(usize, usize)>,
    statements: Vec<Range<usize>>,
}

impl Projection {
    pub(super) fn refresh(
        &mut self,
        text: &EditorText,
        sql: &str,
        folds: &[Range<usize>],
        driver: DriverKind,
    ) {
        if self.revision == Some(text.revision.get()) {
            return;
        }
        self.line_starts.clear();
        self.line_starts.push((0, 0));
        self.fold_offsets.clear();
        self.placeholders.clear();
        let (mut line, mut hidden_chars) = (0, 0);
        self.char_count = 0;
        for (index, ch) in text.as_str().chars().enumerate() {
            let previous_line = line;
            self.char_count += 1;
            if let Some((fold_index, fold)) =
                folding::placeholder_index(ch).and_then(|i| folds.get(i).map(|f| (i, f)))
            {
                self.placeholders.push((index, fold_index));
                line += sql[fold.clone()].matches('\n').count();
                hidden_chars += sql[fold.clone()].chars().count().saturating_sub(1);
                self.fold_offsets.push((index + 1, hidden_chars));
            } else {
                line += usize::from(ch == '\n');
            }
            if line != previous_line {
                self.line_starts.push((index + 1, line));
            }
        }
        self.statements.clear();
        let mut bytes = text.as_str().char_indices().peekable();
        let mut char_index = 0;
        self.statements
            .extend(
                splitter::split(text.as_str(), driver)
                    .into_iter()
                    .map(|statement| {
                        let mut char_at = |byte| {
                            while bytes.peek().is_some_and(|&(offset, _)| offset < byte) {
                                bytes.next();
                                char_index += 1;
                            }
                            char_index
                        };
                        char_at(statement.range.start)..char_at(statement.range.end)
                    }),
            );
        self.revision = Some(text.revision.get());
    }

    pub(super) fn char_count(&self) -> usize {
        self.char_count
    }

    pub(super) fn real_line(&self, index: usize) -> usize {
        let count = self
            .line_starts
            .partition_point(|&(start, _)| start <= index);
        count.checked_sub(1).map_or(0, |i| self.line_starts[i].1)
    }

    pub(super) fn real_char(&self, index: usize) -> usize {
        let index = index.min(self.char_count);
        let count = self
            .fold_offsets
            .partition_point(|&(start, _)| start <= index);
        index + count.checked_sub(1).map_or(0, |i| self.fold_offsets[i].1)
    }

    pub(super) fn statement(&self, caret: usize) -> Option<&Range<usize>> {
        let index = self
            .statements
            .partition_point(|statement| statement.start <= caret)
            .saturating_sub(1);
        self.statements.get(index)
    }
}

#[derive(Default)]
pub(super) struct EditorLayout {
    revision: Option<u64>,
    font_size: f32,
    galley: Option<Arc<Galley>>,
    wrap: f32,
    kept_pass: Option<u64>,
    pub(super) row_offsets: Vec<usize>,
}

impl EditorLayout {
    /// TextEdit keeps the empty hint galley for the first input frame, even
    /// though its layouter has already cached the inserted text. Selection
    /// painting can clone a galley, so compare the shared layout jobs.
    pub(super) fn matches_output(&self, galley: &Galley) -> bool {
        self.galley
            .as_ref()
            .is_some_and(|cached| Arc::ptr_eq(&cached.job, &galley.job))
    }

    fn invalidate(&mut self) {
        self.revision = None;
        self.galley = None;
    }

    pub(super) fn layout(
        &mut self,
        ui: &Ui,
        text: &dyn TextBuffer,
        revision: u64,
        summaries: &[FoldSummary],
        wrap: f32,
    ) -> Arc<Galley> {
        let pass = ui.ctx().cumulative_pass_nr();
        let font_size = theme::appearance(ui.ctx()).editor_font_size;
        let job = if self.revision != Some(revision) || self.font_size != font_size {
            let width = |ch| {
                folding::placeholder_index(ch)
                    .and_then(|i| summaries.get(i))
                    .map(|summary| summary.width)
            };
            self.revision = Some(revision);
            self.font_size = font_size;
            Some(highlight::layout_with_folds(text.as_str(), font_size, &width))
        } else if self.wrap != wrap || self.kept_pass != Some(pass) {
            // Epaint evicts paragraph layouts not touched during the current
            // pass, even while we retain their parent Arc<Galley>. There is no
            // public keepalive API. Reusing the already highlighted job costs
            // a clone/hash, but preserves fast typing after an idle pause.
            Some(self.galley.as_ref().unwrap().job.as_ref().clone())
        } else {
            None
        };
        if let Some(mut job) = job {
            job.wrap.max_width = wrap;
            let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
            if self
                .galley
                .as_ref()
                .is_none_or(|old| !Arc::ptr_eq(old, &galley))
            {
                self.row_offsets.clear();
                let mut index = 0;
                self.row_offsets.extend(galley.rows.iter().map(|row| {
                    let start = index;
                    index += row.char_count_including_newline().0;
                    start
                }));
            }
            self.wrap = wrap;
            self.kept_pass = Some(pass);
            self.galley = Some(galley);
        }
        Arc::clone(self.galley.as_ref().unwrap())
    }
}

/// Epaint's general cursor lookup walks from row zero. Our cached row starts
/// let overlays resolve a character near the end of a large script directly.
pub(super) fn cursor_rect(
    galley: &Galley,
    row_offsets: &[usize],
    cursor: egui::text::CCursor,
) -> egui::Rect {
    let mut row = row_offsets
        .partition_point(|&start| start <= cursor.index.0)
        .saturating_sub(1);
    if !cursor.prefer_next_row
        && row > 0
        && row_offsets[row] == cursor.index.0
        && !galley.rows[row - 1].ends_with_newline
    {
        row -= 1;
    }
    let column = cursor
        .index
        .0
        .saturating_sub(row_offsets.get(row).copied().unwrap_or(0));
    let column = galley
        .rows
        .get(row)
        .map_or(0, |row| column.min(row.char_count_excluding_newline().0));
    galley.pos_from_layout_cursor(&egui::epaint::text::cursor::LayoutCursor {
        row,
        column: CharIndex(column),
    })
}

pub(in crate::ui) struct EditorCache {
    dirty: bool,
    source_revision: u64,
    folds_dirty: bool,
    driver: Option<DriverKind>,
    font_probe: Option<Arc<Galley>>,
    pub(super) text: EditorText,
    pub(super) projection: Projection,
    pub(super) layout: EditorLayout,
    pub(super) summaries: Vec<FoldSummary>,
    pub(super) regions: Vec<folding::Region>,
    pub(super) markers: HashMap<usize, bool>,
    pub(super) line_count: usize,
    line_bytes: Vec<usize>,
    source_line_chars: Vec<usize>,
    source_char_count: usize,
    #[cfg(test)]
    rebuilds: usize,
}

impl Default for EditorCache {
    fn default() -> Self {
        Self {
            dirty: true,
            source_revision: 0,
            folds_dirty: true,
            driver: None,
            font_probe: None,
            text: Default::default(),
            projection: Default::default(),
            layout: Default::default(),
            summaries: Vec::new(),
            regions: Vec::new(),
            markers: HashMap::new(),
            line_count: 1,
            line_bytes: Vec::new(),
            source_line_chars: Vec::new(),
            source_char_count: 0,
            #[cfg(test)]
            rebuilds: 0,
        }
    }
}

impl EditorCache {
    pub(super) fn invalidate(&mut self) {
        self.dirty = true;
        self.source_revision = self.source_revision.wrapping_add(1);
    }

    /// Allows shared document snapshots to track edits without scanning their text.
    pub(in crate::ui) fn source_revision(&self) -> u64 {
        self.source_revision
    }

    pub(super) fn invalidate_folds(&mut self) {
        self.folds_dirty = true;
    }

    pub(in crate::ui) fn line_col(&self, index: usize) -> Option<(usize, usize)> {
        if self.dirty || self.driver.is_none() {
            return None;
        }
        let index = index.min(self.source_char_count);
        let line = self
            .source_line_chars
            .partition_point(|&start| start <= index)
            .saturating_sub(1);
        Some((line + 1, index - self.source_line_chars[line] + 1))
    }

    pub(super) fn prepare(
        &mut self,
        ui: &Ui,
        sql: &str,
        folds: &[Range<usize>],
        driver: DriverKind,
    ) {
        // Epaint recreates its atlas on font/options changes and when full.
        // A tiny cached galley detects those resets without hashing font data.
        // Requesting it every frame also keeps it alive in epaint's cache.
        let probe = ui.fonts_mut(|fonts| {
            fonts.layout_no_wrap("DBI font cache".into(), theme::mono(13.5), theme::TEXT)
        });
        let fonts_changed = self
            .font_probe
            .as_ref()
            .is_none_or(|old| !Arc::ptr_eq(old, &probe));
        self.font_probe = Some(probe);
        let source_changed = self.dirty || self.driver != Some(driver);
        if source_changed {
            self.regions = folding::regions(sql, driver);
            self.line_bytes.clear();
            self.source_line_chars.clear();
            self.source_line_chars.push(0);
            self.source_char_count = 0;
            for (index, (byte, ch)) in sql.char_indices().enumerate() {
                self.source_char_count += 1;
                if ch == '\n' {
                    self.line_bytes.push(byte);
                    self.source_line_chars.push(index + 1);
                }
            }
            self.line_count = self.line_bytes.len() + 1;
            self.driver = Some(driver);
            self.dirty = false;
            #[cfg(test)]
            {
                self.rebuilds += 1;
            }
        }
        let projection_changed = source_changed || self.folds_dirty;
        if projection_changed {
            self.text.replace_source(folding::visible(sql, folds));
            self.folds_dirty = false;
            self.summaries.clear();
            self.summaries.extend(folds.iter().map(|fold| FoldSummary {
                text: folding::summary(&sql[fold.clone()]),
                width: 0.0,
                preview: OnceCell::new(),
            }));
            self.markers.clear();
            for region in &self.regions {
                let count = folds.partition_point(|fold| fold.start <= region.range.start);
                if count
                    .checked_sub(1)
                    .is_none_or(|i| region.range.end > folds[i].end)
                {
                    self.markers.insert(region.line, false);
                }
            }
            for fold in folds {
                self.markers.insert(
                    self.line_bytes.partition_point(|&byte| byte < fold.start),
                    true,
                );
            }
        }
        if projection_changed || fonts_changed {
            for summary in &mut self.summaries {
                summary.width = ui.fonts_mut(|fonts| {
                    fonts
                        .layout_no_wrap(summary.text.clone(), theme::mono(12.5), theme::TEXT)
                        .size()
                        .x
                }) + 14.0;
            }
            self.layout.invalidate();
        }
        self.projection.refresh(&self.text, sql, folds, driver);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(ctx: &egui::Context, run: impl FnOnce(&mut Ui)) {
        let mut run = Some(run);
        let mut output = ctx.run_ui(Default::default(), |ui| run.take().unwrap()(ui));
        output.textures_delta.clear();
    }

    fn layout(cache: &mut EditorCache, ui: &Ui, wrap: f32) -> Arc<Galley> {
        cache.layout.layout(
            ui,
            &cache.text,
            cache.text.revision.get(),
            &cache.summaries,
            wrap,
        )
    }

    #[test]
    fn idle_frames_keep_text_parse_and_layout_allocations() {
        let ctx = egui::Context::default();
        let mut cache = EditorCache::default();
        let sql = "SELECT 'żółć 😀',\n  1;\nSELECT 2;";
        let mut initial = None;
        let mut text_address = 0;
        for _ in 0..20 {
            frame(&ctx, |ui| {
                cache.prepare(ui, sql, &[], DriverKind::Postgres);
                let galley = layout(&mut cache, ui, 600.0);
                if let Some(initial) = &initial {
                    assert!(Arc::ptr_eq(initial, &galley));
                    assert_eq!(text_address, cache.text.as_str().as_ptr() as usize);
                } else {
                    text_address = cache.text.as_str().as_ptr() as usize;
                    initial = Some(galley);
                }
            });
        }
        assert_eq!(cache.rebuilds, 1);
        assert_eq!(cache.text.revision.get(), 1);
        assert_eq!(cache.projection.statement(0), Some(&(0..20)));
    }

    #[test]
    fn text_mutations_are_tracked_in_the_same_layout_pass() {
        let ctx = egui::Context::default();
        let mut cache = EditorCache::default();
        frame(&ctx, |ui| {
            cache.prepare(ui, "SELECT 'ą';", &[], DriverKind::Postgres);
            let original = layout(&mut cache, ui, 600.0);
            cache.text.insert_text("😀", CharIndex(8));
            let edited = layout(&mut cache, ui, 600.0);
            assert!(!Arc::ptr_eq(&original, &edited));
            assert_eq!(edited.job.text, "SELECT '😀ą';");
            cache.text.delete_char_range(CharIndex(8)..CharIndex(9));
            cache.text.edit(|text| {
                text.push_str(" -- completion");
                true
            });
            let completed = layout(&mut cache, ui, 600.0);
            assert_eq!(completed.job.text, "SELECT 'ą'; -- completion");
            let revision = cache.text.revision.get();
            cache.text.insert_text("", CharIndex(0));
            cache.text.delete_char_range(CharIndex(0)..CharIndex(0));
            cache.text.edit(|_| false);
            assert_eq!(revision, cache.text.revision.get());
        });
    }

    #[test]
    fn typing_after_idle_reuses_unchanged_paragraph_layouts() {
        let ctx = egui::Context::default();
        let mut cache = EditorCache::default();
        let sql = "SELECT 'unchanged';\nSELECT 'edited';";
        let mut initial = None;
        for _ in 0..10 {
            frame(&ctx, |ui| {
                cache.prepare(ui, sql, &[], DriverKind::Postgres);
                let galley = layout(&mut cache, ui, 600.0);
                initial.get_or_insert(galley);
            });
        }
        frame(&ctx, |ui| {
            cache.prepare(ui, sql, &[], DriverKind::Postgres);
            let original = initial.as_ref().unwrap();
            cache.text.insert_text("1", CharIndex(sql.chars().count()));
            let edited = layout(&mut cache, ui, 600.0);
            assert!(!Arc::ptr_eq(original, &edited));
            assert!(Arc::ptr_eq(&original.rows[0].row, &edited.rows[0].row));
            assert!(!Arc::ptr_eq(&original.rows[1].row, &edited.rows[1].row));
        });
    }

    #[test]
    fn sparse_projection_matches_unicode_folds_and_statement_boundaries() {
        let ctx = egui::Context::default();
        let mut cache = EditorCache::default();
        let sql = "-- 😀\nSELECT (\n  'Łódź',\n  2\n);\nSELECT 'café';";
        let start = sql.find("'Łódź'").unwrap();
        let end = sql.find("\n)").unwrap();
        let fold = start..end;
        let folds = std::slice::from_ref(&fold);
        frame(&ctx, |ui| {
            cache.prepare(ui, sql, folds, DriverKind::Postgres)
        });
        let visible = cache.text.as_str();
        let mut line = 0;
        for (index, ch) in visible.chars().enumerate() {
            assert_eq!(
                cache.projection.real_char(index),
                folding::to_real_char(visible, sql, folds, index)
            );
            assert_eq!(cache.projection.real_line(index), line);
            if let Some(fold) = folding::placeholder_index(ch).and_then(|i| folds.get(i)) {
                line += sql[fold.clone()].matches('\n').count();
            } else if ch == '\n' {
                line += 1;
            }
        }
        assert_eq!(
            cache.projection.real_char(visible.chars().count()),
            sql.chars().count()
        );
        for (index, byte) in visible
            .char_indices()
            .map(|(byte, _)| byte)
            .chain([visible.len()])
            .enumerate()
        {
            let expected =
                splitter::statement_at(visible, DriverKind::Postgres, byte).map(|statement| {
                    visible[..statement.range.start].chars().count()
                        ..visible[..statement.range.end].chars().count()
                });
            assert_eq!(cache.projection.statement(index), expected.as_ref());
        }
        assert!(cache.projection.line_starts.len() < visible.chars().count() / 2);
        assert_eq!(cache.projection.fold_offsets.len(), 1);
    }

    #[test]
    fn source_driver_folds_wrap_and_fonts_invalidate_only_their_derived_data() {
        let ctx = egui::Context::default();
        let mut cache = EditorCache::default();
        let sql = "SELECT (\n  'żółć', 123456789\n);\nSELECT 2;";
        let mut original = None;
        frame(&ctx, |ui| {
            cache.prepare(ui, sql, &[], DriverKind::Postgres);
            original = Some(layout(&mut cache, ui, 600.0));
            let narrow = layout(&mut cache, ui, 60.0);
            assert!(narrow.rows.len() > original.as_ref().unwrap().rows.len());
            assert_eq!(cache.rebuilds, 1);
            cache.invalidate_folds();
            cache.prepare(
                ui,
                sql,
                std::slice::from_ref(&(8..26)),
                DriverKind::Postgres,
            );
            assert_eq!(cache.rebuilds, 1);
            assert_eq!(cache.summaries.len(), 1);
            assert!(cache.summaries[0].preview.get().is_none());
            cache.prepare(ui, sql, &[], DriverKind::MySql);
            assert_eq!(cache.rebuilds, 2);
            cache.invalidate();
            cache.prepare(ui, "SELECT 3;", &[], DriverKind::MySql);
            assert_eq!(cache.rebuilds, 3);
            assert_eq!(cache.text.as_str(), "SELECT 3;");
            original = Some(layout(&mut cache, ui, 600.0));
        });
        // Changing definitions recreates the font atlas. Cached galleys must
        // never retain old atlas coordinates, even if the SQL is unchanged.
        let mut definitions = egui::FontDefinitions::default();
        definitions
            .families
            .insert(egui::FontFamily::Name("cache test".into()), Vec::new());
        ctx.set_fonts(definitions);
        frame(&ctx, |ui| {
            cache.prepare(ui, "SELECT 3;", &[], DriverKind::MySql);
            let refreshed = layout(&mut cache, ui, 600.0);
            assert!(!Arc::ptr_eq(original.as_ref().unwrap(), &refreshed));
            assert_eq!(cache.rebuilds, 3);
        });
    }

    #[test]
    fn cached_cursor_positions_match_epaint_for_wrapping_and_unicode() {
        let ctx = egui::Context::default();
        let mut cache = EditorCache::default();
        let sql = "SELECT 'żółć 😀', abcdefghijklmnopqrstuvwxyz;\n\nSELECT 2;\n";
        frame(&ctx, |ui| {
            cache.prepare(ui, sql, &[], DriverKind::Postgres);
            for width in [60.0, 180.0, 600.0] {
                let galley = layout(&mut cache, ui, width);
                for index in 0..=sql.chars().count() {
                    for prefer_next_row in [false, true] {
                        let cursor = egui::text::CCursor {
                            index: CharIndex(index),
                            prefer_next_row,
                        };
                        assert_eq!(
                            cursor_rect(&galley, &cache.layout.row_offsets, cursor),
                            galley.pos_from_cursor(cursor),
                            "index={index}, prefer_next={prefer_next_row}, width={width}"
                        );
                    }
                }
            }
        });
    }

    #[test]
    fn status_line_and_column_use_sparse_source_offsets() {
        let ctx = egui::Context::default();
        let mut cache = EditorCache::default();
        let sql = "😀\nżółć\n";
        assert_eq!(cache.line_col(0), None);
        frame(&ctx, |ui| cache.prepare(ui, sql, &[], DriverKind::Postgres));
        for (index, expected) in [
            (0, (1, 1)),
            (1, (1, 2)),
            (2, (2, 1)),
            (6, (2, 5)),
            (7, (3, 1)),
            (100, (3, 1)),
        ] {
            assert_eq!(cache.line_col(index), Some(expected));
        }
        cache.invalidate_folds();
        assert_eq!(cache.line_col(6), Some((2, 5)));
        cache.invalidate();
        assert_eq!(cache.line_col(0), None);
    }

    #[test]
    #[ignore = "release benchmark: cargo test --release --lib editor_layout_cost -- --ignored --nocapture"]
    fn editor_layout_cost_benchmark() {
        use std::time::{Duration, Instant};
        let block = "-- Batch: Łódź, żółć and 😀\nSELECT item_id, 'Łódź' AS city,\n       (SELECT count(*) FROM inventory WHERE active = true) AS total\nFROM inventory WHERE item_id = 0;\n\n";
        for target in [100_000usize, 1_000_000] {
            let mut sql = block.repeat(target / block.len() + 1);
            let ctx = egui::Context::default();
            let mut cache = EditorCache::default();
            for _ in 0..3 {
                frame(&ctx, |ui| {
                    cache.prepare(ui, &sql, &[], DriverKind::Postgres);
                    layout(&mut cache, ui, 900.0);
                });
            }
            let percentiles = |mut samples: Vec<Duration>| {
                samples.sort();
                let at = |p: f64| samples[((samples.len() - 1) as f64 * p).round() as usize];
                format!("p50={:?} p95={:?} p99={:?}", at(0.5), at(0.95), at(0.99))
            };
            let (mut idle, mut typing, mut highlight) = (Vec::new(), Vec::new(), Vec::new());
            for i in 0..30 {
                // A new pass with unchanged text: the keep-alive relayout.
                frame(&ctx, |ui| {
                    cache.prepare(ui, &sql, &[], DriverKind::Postgres);
                    let start = Instant::now();
                    layout(&mut cache, ui, 900.0);
                    idle.push(start.elapsed());
                });
                // A typed character, as the console sees it on the next pass:
                // the source changed, so folding regions, projection,
                // highlighting and layout are rebuilt.
                sql.insert(i, 'x');
                frame(&ctx, |ui| {
                    let start = Instant::now();
                    cache.invalidate();
                    cache.prepare(ui, &sql, &[], DriverKind::Postgres);
                    layout(&mut cache, ui, 900.0);
                    typing.push(start.elapsed());
                });
                let start = Instant::now();
                let job = crate::ui::highlight::layout_with_folds(&sql, 13.0, &|_| None);
                highlight.push(start.elapsed());
                drop(job);
            }
            println!(
                "editor layout {} bytes: idle keep-alive {}; typed char {}; highlight only {}",
                sql.len(),
                percentiles(idle),
                percentiles(typing),
                percentiles(highlight)
            );
        }
    }
}
