//! Read-only DDL snapshots. Fetching, line alignment, and bounded previews belong
//! to the worker; this view lays out only the visible lines.

use super::theme;
use crate::{
    ddl_diff::{ComparedDdl, DdlChange, DdlObjectIdentity},
    search::{SearchOptions, SearchPattern},
};
use egui::{Align, Color32, Id, Key, Layout, Modifiers, RichText, Sense, Ui};
use std::{
    collections::HashMap,
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);
const ROW_HEIGHT: f32 = 24.0;
const GUTTER: f32 = 44.0;
const GAP: f32 = 16.0;

pub(super) struct StructureCompareSource {
    pub(super) identity: DdlObjectIdentity,
    pub(super) label: String,
    pub(super) location: String,
    pub(super) blocked: Option<String>,
}

pub(super) enum StructureIntent {
    Compare {
        request_id: u64,
        origin: DdlObjectIdentity,
        target: DdlObjectIdentity,
        cancelled: Arc<AtomicBool>,
    },
}

struct DisplaySource {
    label: String,
    location: String,
}
#[derive(Clone)]
struct Labels {
    origin: String,
    target: String,
    origin_location: String,
    target_location: String,
}
struct Pending {
    id: u64,
    origin: DdlObjectIdentity,
    target: DdlObjectIdentity,
    labels: Labels,
    cancelled: Arc<AtomicBool>,
}
struct Dialog {
    id: u64,
    sources: Vec<StructureCompareSource>,
    display: Vec<DisplaySource>,
    origin: usize,
    target: Option<usize>,
    query: String,
    matches: Vec<usize>,
    focus_query: bool,
    reveal_target: bool,
    reason: Option<String>,
    error: Option<String>,
    pending: Option<Pending>,
    diff: Option<DiffView>,
    focus: Option<Id>,
    focus_captured: bool,
    #[cfg(test)]
    searches: usize,
}

#[derive(Default)]
pub(super) struct StructureCompareState {
    dialog: Option<Dialog>,
    restore_focus: Option<Id>,
}
impl Drop for StructureCompareState {
    fn drop(&mut self) {
        self.cancel();
    }
}
impl StructureCompareState {
    pub(super) fn open(&mut self, sources: Vec<StructureCompareSource>, origin: DdlObjectIdentity) {
        self.cancel();
        self.restore_focus = None;
        let Some(origin) = sources.iter().position(|source| source.identity == origin) else {
            return;
        };
        let display = sources
            .iter()
            .map(|source| DisplaySource {
                label: bounded(&source.label, 1024),
                location: bounded(&source.location, 2048),
            })
            .collect();
        let mut dialog = Dialog {
            id: NEXT_REQUEST.fetch_add(1, Ordering::Relaxed),
            sources,
            display,
            origin,
            target: None,
            query: String::new(),
            matches: Vec::new(),
            focus_query: true,
            reveal_target: false,
            reason: None,
            error: None,
            pending: None,
            diff: None,
            focus: None,
            focus_captured: false,
            #[cfg(test)]
            searches: 0,
        };
        dialog.refresh_matches();
        dialog.refresh_reason();
        self.dialog = Some(dialog);
    }

    pub(super) fn is_open(&self) -> bool {
        self.dialog.is_some()
    }

    pub(super) fn pending_identities(
        &self,
        request_id: u64,
    ) -> Option<(&DdlObjectIdentity, &DdlObjectIdentity)> {
        let pending = self.dialog.as_ref()?.pending.as_ref()?;
        (pending.id == request_id).then_some((&pending.origin, &pending.target))
    }

    pub(super) fn cancel(&mut self) {
        if let Some(mut dialog) = self.dialog.take() {
            dialog.cancel_pending();
            self.restore_focus = dialog.focus;
        }
    }

    /// The owner restores this after the modal layer disappears next frame.
    pub(super) fn take_restore_focus(&mut self) -> Option<Id> {
        self.restore_focus.take()
    }

    pub(super) fn done(&mut self, request_id: u64, result: Result<ComparedDdl, String>) {
        let Some(dialog) = &mut self.dialog else {
            return;
        };
        if !dialog.pending.as_ref().is_some_and(|p| p.id == request_id) {
            return;
        }
        let pending = dialog.pending.take().unwrap();
        match result {
            Ok(data)
                if data.origin.identity == pending.origin
                    && data.target.identity == pending.target =>
            {
                dialog.diff = Some(DiffView::new(data, pending.labels, request_id));
                dialog.error = None;
            }
            Ok(_) => {
                dialog.error =
                    Some("The returned definitions do not match the selected objects".into());
            }
            Err(error) => dialog.error = Some(bounded(&error, 4096)),
        }
    }

    pub(super) fn show(&mut self, ctx: &egui::Context) -> Option<StructureIntent> {
        let dialog = self.dialog.as_mut()?;
        if !dialog.focus_captured {
            dialog.focus = ctx.memory(|memory| memory.focused());
            dialog.focus_captured = true;
        }
        let mut close = false;
        let mut start = false;
        if !ctx.any_popup_open() {
            ctx.input_mut(|input| {
                close = input.consume_key(Modifiers::NONE, Key::Escape);
                if let Some(diff) = &mut dialog.diff {
                    if input.consume_key(Modifiers::SHIFT, Key::F7) {
                        diff.navigate(true);
                    }
                    if input.consume_key(Modifiers::NONE, Key::F7) {
                        diff.navigate(false);
                    }
                } else if dialog.pending.is_none() {
                    if input.consume_key(Modifiers::NONE, Key::ArrowDown) {
                        dialog.navigate(false);
                    }
                    if input.consume_key(Modifiers::NONE, Key::ArrowUp) {
                        dialog.navigate(true);
                    }
                    start = input.consume_key(Modifiers::NONE, Key::Enter);
                }
            });
        }
        let modal = theme::Dialog::new(Id::new("ddl_diff_modal"), "DDL Diff")
            .default_size([1120.0, 680.0])
            .min_size([360.0, 280.0])
            .show(ctx, |ui| {
            ui.add(
                egui::Label::new(
                    RichText::new("Read-only DDL comparison · Definitions fetched independently")
                        .small()
                        .color(theme::TEXT_MUTED),
                )
                .truncate(),
            );
            ui.separator();
            // The body takes what the resizable dialog leaves above the buttons.
            let width = ui.available_width();
            let body_height = (ui.available_height() - 56.0).max(120.0);
            let (body, _) = ui.allocate_exact_size(egui::vec2(width, body_height), Sense::hover());
            let mut content = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt(dialog.id)
                    .max_rect(body)
                    .layout(Layout::top_down(Align::Min)),
            );
            content.set_clip_rect(ui.clip_rect().intersect(body));
            if let Some(diff) = &mut dialog.diff {
                diff.show(&mut content);
            } else {
                start |= dialog.setup(&mut content);
            }
            theme::dialog_footer(
                ui,
                |ui| {
                    if dialog.diff.is_some() && ui.button("Change Objects").clicked() {
                        dialog.diff = None;
                        dialog.error = None;
                        dialog.focus_query = true;
                    }
                    if dialog.pending.is_some() && ui.button("Cancel Comparison").clicked() {
                        dialog.cancel_pending();
                    }
                },
                |ui| {
                    close |= theme::dialog_button(ui, "Close").clicked();
                },
            );
        });
        close |= modal.should_close();
        if close {
            self.cancel();
            return None;
        }
        if start {
            self.dialog.as_mut()?.start()
        } else {
            None
        }
    }
}

impl Dialog {
    fn refresh_matches(&mut self) {
        #[cfg(test)]
        {
            self.searches += 1;
        }
        self.matches.clear();
        let pattern = SearchPattern::new(&self.query, SearchOptions::default());
        let origin = &self.sources[self.origin].identity;
        self.matches.extend(
            self.sources
                .iter()
                .enumerate()
                .filter_map(|(index, source)| {
                    (index != self.origin
                        && source.identity != *origin
                        && source.identity.kind == origin.kind
                        && source.identity.source.driver == origin.source.driver
                        && (self.query.is_empty() || pattern.is_match(&source.label)))
                    .then_some(index)
                }),
        );
        self.reveal_target = true;
    }

    fn refresh_reason(&mut self) {
        self.reason = match self.target {
            None => Some("Select a target object".into()),
            Some(target) => {
                let origin = &self.sources[self.origin];
                let target = &self.sources[target];
                if origin.identity == target.identity {
                    Some("Choose two different objects".into())
                } else if origin.identity.kind != target.identity.kind
                    || origin.identity.source.driver != target.identity.source.driver
                {
                    Some("Choose objects of the same kind and database driver".into())
                } else {
                    origin
                        .blocked
                        .as_ref()
                        .or(target.blocked.as_ref())
                        .map(|s| bounded(s, 2048))
                }
            }
        };
    }

    fn navigate(&mut self, backwards: bool) {
        if self.matches.is_empty() {
            return;
        }
        let current = self
            .target
            .and_then(|target| self.matches.binary_search(&target).ok());
        let position = match current {
            Some(position) if backwards => (position + self.matches.len() - 1) % self.matches.len(),
            Some(position) => (position + 1) % self.matches.len(),
            None if backwards => self.matches.len() - 1,
            None => 0,
        };
        self.target = Some(self.matches[position]);
        self.refresh_reason();
        self.reveal_target = true;
    }

    fn cancel_pending(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.cancelled.store(true, Ordering::Relaxed);
        }
    }

    fn start(&mut self) -> Option<StructureIntent> {
        if self.pending.is_some() || self.diff.is_some() {
            return None;
        }
        self.refresh_reason();
        if self.reason.is_some() {
            return None;
        }
        let target = self.target?;
        let id = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
        let origin = self.sources[self.origin].identity.clone();
        let target_identity = self.sources[target].identity.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        self.pending = Some(Pending {
            id,
            origin: origin.clone(),
            target: target_identity.clone(),
            labels: Labels {
                origin: self.display[self.origin].label.clone(),
                target: self.display[target].label.clone(),
                origin_location: self.display[self.origin].location.clone(),
                target_location: self.display[target].location.clone(),
            },
            cancelled: Arc::clone(&cancelled),
        });
        self.error = None;
        Some(StructureIntent::Compare {
            request_id: id,
            origin,
            target: target_identity,
            cancelled,
        })
    }

    fn setup(&mut self, ui: &mut Ui) -> bool {
        let busy = self.pending.is_some();
        let mut start = false;
        source_caption(ui, "Origin", &self.display[self.origin]);
        ui.horizontal(|ui| {
            ui.strong("Target");
            ui.add(
                egui::Label::new(
                    self.target
                        .map(|i| self.display[i].label.as_str())
                        .unwrap_or("Select a loaded object"),
                )
                .truncate(),
            );
        });
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!busy && self.target.is_some(), egui::Button::new("Swap"))
                .clicked()
            {
                let old_origin = self.origin;
                self.origin = self.target.unwrap();
                self.target = Some(old_origin);
                self.refresh_matches();
                self.refresh_reason();
                self.error = None;
            }
            ui.label(
                RichText::new("Loaded objects only · Exact text and names")
                    .small()
                    .color(theme::TEXT_MUTED),
            );
        });
        let response = ui.add_enabled(
            !busy,
            egui::TextEdit::singleline(&mut self.query)
                .id(Id::new(("ddl_find_object", self.id)))
                .hint_text("Find Object")
                .desired_width(f32::INFINITY),
        );
        response.widget_info(|| {
            let mut info =
                egui::WidgetInfo::text_edit(!busy, &self.query, &self.query, "Find Object");
            info.label = Some("Find Object".into());
            info
        });
        ui.memory_mut(|memory| {
            memory.set_focus_lock_filter(
                response.id,
                egui::EventFilter {
                    vertical_arrows: true,
                    ..Default::default()
                },
            )
        });
        if self.focus_query {
            response.request_focus();
            self.focus_query = false;
        }
        if response.changed() {
            self.refresh_matches();
        }
        let list_height = (ui.available_height() - 72.0).max(24.0);
        let mut scroll = egui::ScrollArea::vertical()
            .id_salt("ddl_targets")
            .max_height(list_height)
            .min_scrolled_height(list_height)
            .auto_shrink([false, false]);
        if std::mem::take(&mut self.reveal_target)
            && let Some(position) = self
                .target
                .and_then(|target| self.matches.binary_search(&target).ok())
        {
            scroll = scroll.vertical_scroll_offset(
                (position as f32 * (ROW_HEIGHT + ui.spacing().item_spacing.y) - list_height * 0.5)
                    .max(0.0),
            );
        }
        let mut selected = None;
        ui.add_enabled_ui(!busy, |ui| {
            scroll.show_rows(ui, ROW_HEIGHT, self.matches.len(), |ui, range| {
                if self.matches.is_empty() {
                    ui.label("No compatible loaded objects. Load another table or view in Database Explorer.");
                }
                for position in range {
                    let index = self.matches[position];
                    let source = &self.display[index];
                    let response = ui.add(egui::Button::selectable(self.target == Some(index), &source.label)
                        .truncate().min_size(egui::vec2(ui.available_width(), ROW_HEIGHT)))
                        .on_hover_text(&source.location);
                    if let Some(reason) = &self.sources[index].blocked {
                        ui.ctx().accesskit_node_builder(response.id, |node| node.set_description(reason.as_str()));
                    }
                    if response.clicked() { selected = Some(index); }
                    if response.double_clicked() { selected = Some(index); start = true; }
                }
            });
        });
        if let Some(selected) = selected {
            self.target = Some(selected);
            self.refresh_reason();
            self.error = None;
        }
        if let Some(error) = &self.error {
            ui.add(egui::Label::new(RichText::new(error).color(theme::RED)).truncate())
                .on_hover_text(error);
        } else if let Some(reason) = &self.reason {
            ui.add(egui::Label::new(RichText::new(reason).color(theme::TEXT_MUTED)).truncate())
                .on_hover_text(reason);
        } else {
            ui.label(
                RichText::new("Fetches current definitions from both sources.")
                    .small()
                    .color(theme::TEXT_MUTED),
            );
        }
        ui.horizontal(|ui| {
            start |= ui
                .add_enabled(!busy && self.reason.is_none(), egui::Button::new("Compare"))
                .clicked();
            if busy {
                ui.spinner();
                ui.label("Fetching definitions…");
            }
        });
        start
    }
}

fn source_caption(ui: &mut Ui, title: &str, source: &DisplaySource) {
    ui.horizontal(|ui| {
        ui.strong(title);
        ui.add(egui::Label::new(&source.label).truncate())
            .on_hover_text(&source.location);
    });
}

struct LineDisplay {
    label: String,
    description: String,
    number: String,
    galley: Arc<egui::Galley>,
}
struct DiffView {
    id: u64,
    data: ComparedDdl,
    labels: Labels,
    origin_coverage: String,
    target_coverage: String,
    coverage: String,
    selected: Option<usize>,
    counter: String,
    reveal: bool,
    max_preview_bytes: usize,
    truncated: bool,
    visible: Range<usize>,
    cache: HashMap<(bool, usize), LineDisplay>,
    font: Option<(egui::FontId, f32)>,
    #[cfg(test)]
    painted_rows: usize,
}
impl DiffView {
    fn new(data: ComparedDdl, labels: Labels, id: u64) -> Self {
        let origin_coverage = bounded(&data.origin.coverage.join("; "), 2048);
        let target_coverage = bounded(&data.target.coverage.join("; "), 2048);
        let coverage = format!(
            "Origin: {origin_coverage}\n\nTarget: {target_coverage}\n\nNames, whitespace and final newlines are significant. Definitions are independent reads, not a complete schema comparison.\nLimits: 8 MiB and 100,000 lines per definition."
        );
        let mut max_preview_bytes = 0;
        let mut truncated = false;
        for line in data.origin.lines.iter().chain(&data.target.lines) {
            max_preview_bytes = max_preview_bytes.max(line.preview.len());
            truncated |= line.truncated;
        }
        let mut view = Self {
            id,
            selected: data.differences.first().copied(),
            data,
            labels,
            origin_coverage,
            target_coverage,
            coverage,
            counter: String::new(),
            reveal: true,
            max_preview_bytes,
            truncated,
            visible: 0..0,
            cache: HashMap::new(),
            font: None,
            #[cfg(test)]
            painted_rows: 0,
        };
        view.refresh_counter();
        view
    }
    fn refresh_counter(&mut self) {
        self.counter = if self.data.differences.is_empty() {
            "No differences in fetched DDL".into()
        } else {
            let current = self
                .selected
                .map(|row| self.data.differences.partition_point(|&start| start <= row))
                .unwrap_or(0);
            format!(
                "Difference {} of {}",
                current.max(1),
                self.data.differences.len()
            )
        };
    }
    fn navigate(&mut self, backwards: bool) {
        let differences = &self.data.differences;
        if differences.is_empty() {
            return;
        }
        let index = if backwards {
            let before = self.selected.map_or(differences.len(), |row| {
                differences.partition_point(|&start| start < row)
            });
            if before == 0 {
                differences.len() - 1
            } else {
                before - 1
            }
        } else {
            let after = self
                .selected
                .map_or(0, |row| differences.partition_point(|&start| start <= row));
            after % differences.len()
        };
        self.selected = Some(differences[index]);
        self.reveal = true;
        self.refresh_counter();
    }
    fn show(&mut self, ui: &mut Ui) {
        ui.columns(2, |columns| {
            for (column, (title, label, location, coverage)) in columns.iter_mut().zip([
                (
                    "Origin",
                    &self.labels.origin,
                    &self.labels.origin_location,
                    &self.origin_coverage,
                ),
                (
                    "Target",
                    &self.labels.target,
                    &self.labels.target_location,
                    &self.target_coverage,
                ),
            ]) {
                column.strong(title);
                column
                    .add(egui::Label::new(label).truncate())
                    .on_hover_text(location);
                column
                    .add(
                        egui::Label::new(RichText::new(coverage).small().color(theme::TEXT_MUTED))
                            .truncate(),
                    )
                    .on_hover_text(coverage);
            }
        });
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    !self.data.differences.is_empty(),
                    egui::Button::new("Previous Difference"),
                )
                .clicked()
            {
                self.navigate(true);
            }
            if ui
                .add_enabled(
                    !self.data.differences.is_empty(),
                    egui::Button::new("Next Difference"),
                )
                .clicked()
            {
                self.navigate(false);
            }
            if ui.button("Copy Origin DDL").clicked() {
                ui.ctx().copy_text(self.data.origin.sql.to_string());
            }
            if ui.button("Copy Target DDL").clicked() {
                ui.ctx().copy_text(self.data.target.sql.to_string());
            }
            ui.menu_button("Definition coverage", |ui| {
                ui.set_max_width(520.0);
                egui::ScrollArea::vertical()
                    .max_height(180.0)
                    .show(ui, |ui| {
                        ui.label(&self.coverage);
                    });
            });
        });
        ui.add(egui::Label::new(RichText::new(&self.counter).strong()).truncate());
        if self.data.alignment_limited {
            ui.add(
                egui::Label::new(
                    RichText::new("Large changed blocks shown without detailed line alignment")
                        .small()
                        .color(theme::TEXT_MUTED),
                )
                .truncate(),
            );
        }
        if self.truncated {
            ui.add(
                egui::Label::new(
                    RichText::new("Preview truncated. Copy DDL preserves the full definition.")
                        .small()
                        .color(theme::TEXT_MUTED),
                )
                .truncate(),
            );
        }
        self.grid(ui);
    }

    fn grid(&mut self, ui: &mut Ui) {
        #[cfg(test)]
        {
            self.painted_rows = 0;
        }
        let font = egui::TextStyle::Monospace.resolve(ui.style());
        let font_key = (font.clone(), ui.ctx().pixels_per_point());
        if self.font.as_ref() != Some(&font_key) {
            self.cache.clear();
            self.font = Some(font_key);
        }
        let glyph_width = ui.fonts_mut(|fonts| fonts.glyph_width(&font, 'M'));
        let width = ui.available_width();
        let pane_width = ((width - GAP - 16.0) * 0.5).max(100.0);
        let line_width =
            (self.max_preview_bytes as f32 * glyph_width + GUTTER + 8.0).max(pane_width);
        let content_width = line_width + pane_width + GAP + 16.0;
        let height = ui.available_height().max(24.0);
        let mut selected = None;
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let mut scroll = egui::ScrollArea::both()
                .id_salt(("ddl_lines", self.id))
                .max_height(height)
                .min_scrolled_height(height)
                .auto_shrink([false, false]);
            if std::mem::take(&mut self.reveal)
                && let Some(row) = self.selected
            {
                scroll = scroll
                    .vertical_scroll_offset((row as f32 * ROW_HEIGHT - height * 0.25).max(0.0));
            }
            scroll.show_rows(ui, ROW_HEIGHT, self.data.rows.len(), |ui, range| {
                ui.set_min_width(content_width);
                if self.visible != range {
                    self.cache.clear();
                    self.visible = range.clone();
                }
                let clip = ui.clip_rect();
                let offset = (clip.left() - ui.min_rect().left()).max(0.0);
                for index in range {
                    #[cfg(test)]
                    {
                        self.painted_rows += 1;
                    }
                    let row = self.data.rows[index];
                    let (rect, _) = ui
                        .allocate_exact_size(egui::vec2(content_width, ROW_HEIGHT), Sense::hover());
                    for (right, line_index) in [(false, row.origin), (true, row.target)] {
                        let pane = egui::Rect::from_min_size(
                            egui::pos2(
                                clip.left() + if right { pane_width + GAP } else { 0.0 },
                                rect.top(),
                            ),
                            egui::vec2(pane_width, ROW_HEIGHT),
                        );
                        let fill = match row.change {
                            DdlChange::Changed => Color32::from_rgb(39, 56, 76),
                            DdlChange::OriginOnly if !right => Color32::from_rgb(66, 49, 49),
                            DdlChange::TargetOnly if right => Color32::from_rgb(39, 66, 49),
                            _ => theme::BG_EDITOR,
                        };
                        ui.painter().rect_filled(pane, 0.0, fill);
                        if let Some(line_index) = line_index {
                            let snapshot = if right {
                                &self.data.target
                            } else {
                                &self.data.origin
                            };
                            let line = &snapshot.lines[line_index];
                            let display =
                                self.cache.entry((right, line_index)).or_insert_with(|| {
                                    LineDisplay {
                                        label: format!(
                                            "{} line {}: {}",
                                            if right { "Target" } else { "Origin" },
                                            line_index + 1,
                                            line.preview
                                        ),
                                        description: format!(
                                            "{} · {}{}",
                                            change_name(row.change),
                                            line_ending(snapshot.line(line_index)),
                                            if line.truncated {
                                                " · Preview truncated"
                                            } else {
                                                ""
                                            },
                                        ),
                                        number: (line_index + 1).to_string(),
                                        galley: ui.fonts_mut(|fonts| {
                                            fonts.layout_no_wrap(
                                                line.preview.clone(),
                                                font.clone(),
                                                theme::TEXT,
                                            )
                                        }),
                                    }
                                });
                            let old_clip = ui.clip_rect();
                            ui.set_clip_rect(old_clip.intersect(pane));
                            let gutter = egui::Rect::from_min_max(
                                pane.min,
                                egui::pos2(pane.left() + GUTTER - 6.0, pane.bottom()),
                            );
                            ui.new_child(
                                egui::UiBuilder::new()
                                    .max_rect(gutter)
                                    .layout(Layout::right_to_left(Align::Center)),
                            )
                            .label(
                                RichText::new(&display.number)
                                    .monospace()
                                    .color(theme::TEXT_MUTED),
                            );
                            let text_clip = egui::Rect::from_min_max(
                                egui::pos2(pane.left() + GUTTER, pane.top()),
                                pane.max,
                            );
                            ui.set_clip_rect(old_clip.intersect(text_clip));
                            let text_rect = egui::Rect::from_min_size(
                                egui::pos2(pane.left() + GUTTER - offset, pane.top()),
                                egui::vec2(line_width - GUTTER, ROW_HEIGHT),
                            );
                            let response = ui
                                .new_child(
                                    egui::UiBuilder::new()
                                        .max_rect(text_rect)
                                        .layout(Layout::left_to_right(Align::Center)),
                                )
                                .add(
                                    egui::Label::new(Arc::clone(&display.galley))
                                        .selectable(false)
                                        .sense(Sense::click()),
                                );
                            response.widget_info(|| {
                                egui::WidgetInfo::selected(
                                    egui::WidgetType::Button,
                                    true,
                                    self.selected == Some(index),
                                    &display.label,
                                )
                            });
                            ui.ctx().accesskit_node_builder(response.id, |node| {
                                node.set_description(display.description.as_str())
                            });
                            if response.clicked() {
                                selected = Some(index);
                            }
                            response.on_hover_text(&display.description);
                            ui.set_clip_rect(old_clip);
                        }
                        if self.selected == Some(index) {
                            ui.painter().rect_stroke(
                                pane,
                                0.0,
                                egui::Stroke::new(1.0, theme::ACCENT),
                                egui::StrokeKind::Inside,
                            );
                        }
                    }
                }
            });
        });
        if let Some(selected) = selected
            && self.selected != Some(selected)
        {
            self.selected = Some(selected);
            self.refresh_counter();
        }
    }
}

fn change_name(change: DdlChange) -> &'static str {
    match change {
        DdlChange::Equal => "Equal",
        DdlChange::Changed => "Changed",
        DdlChange::OriginOnly => "Origin only",
        DdlChange::TargetOnly => "Target only",
    }
}
fn line_ending(line: &str) -> &'static str {
    if line.ends_with("\r\n") {
        "Line ending: CRLF"
    } else if line.ends_with('\n') {
        "Line ending: LF"
    } else {
        "No final newline"
    }
}
fn bounded(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        text.to_owned()
    } else {
        format!("{}…", &text[..text.floor_char_boundary(limit)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ddl_diff::{DdlDefinition, compare_ddl},
        model::{DataSource, DriverKind, ObjectKind},
    };
    use egui_kittest::kittest::Queryable;

    fn source(index: usize) -> StructureCompareSource {
        let mut ds = DataSource::new(DriverKind::Sqlite);
        ds.id = format!("ddl-{index}");
        ds.file = format!("/fixture/{index}.sqlite");
        StructureCompareSource {
            identity: DdlObjectIdentity {
                ds_id: ds.id.clone(),
                source: ds.identity(),
                kind: ObjectKind::Table,
                path: vec!["main".into(), format!("table_{index}")],
            },
            label: format!("Source {index} / main.table_{index} [{index}]"),
            location: format!("SQLite /fixture/{index}.sqlite / main.table_{index}"),
            blocked: None,
        }
    }
    fn opened(count: usize) -> StructureCompareState {
        let sources = (0..count).map(source).collect::<Vec<_>>();
        let identity = sources[0].identity.clone();
        let mut state = StructureCompareState::default();
        state.open(sources, identity);
        state
    }
    fn start(state: &mut StructureCompareState) -> StructureIntent {
        let dialog = state.dialog.as_mut().unwrap();
        dialog.target = Some(1);
        dialog.start().unwrap()
    }
    fn compared(request: StructureIntent, left: String, right: String) -> (u64, ComparedDdl) {
        let StructureIntent::Compare {
            request_id,
            origin,
            target,
            cancelled,
        } = request;
        let data = compare_ddl(
            DdlDefinition {
                identity: origin,
                sql: left,
                coverage: vec!["Original CREATE definition".into()],
            },
            DdlDefinition {
                identity: target,
                sql: right,
                coverage: vec!["Original CREATE definition".into()],
            },
            &cancelled,
        )
        .unwrap();
        (request_id, data)
    }
    fn completed(count: usize) -> StructureCompareState {
        let mut state = opened(2);
        let request = start(&mut state);
        let left = (0..count)
            .map(|i| format!("column_{i} TEXT,\n"))
            .collect::<String>();
        let right = (0..count)
            .map(|i| {
                format!(
                    "column_{i} {},\n",
                    if i % 1000 == 0 { "INTEGER" } else { "TEXT" }
                )
            })
            .collect::<String>();
        let (id, data) = compared(request, left, right);
        state.done(id, Ok(data));
        state
    }

    #[test]
    fn choices_are_explicit_and_blocked_or_incompatible_objects_do_not_start() {
        let mut state = opened(3);
        let dialog = state.dialog.as_mut().unwrap();
        assert!(dialog.target.is_none());
        assert!(dialog.start().is_none());
        dialog.sources[1].blocked = Some("Wait for the data source file to finish saving".into());
        dialog.target = Some(1);
        assert!(dialog.start().is_none());
        assert!(dialog.reason.as_deref().unwrap().contains("saving"));
        dialog.sources[2].identity.kind = ObjectKind::View;
        dialog.refresh_matches();
        assert_eq!(dialog.matches, [1]);
        dialog.target = Some(2);
        assert!(dialog.start().is_none());
        assert!(dialog.reason.as_deref().unwrap().contains("same kind"));
    }

    #[test]
    fn cancellation_replacement_and_duplicate_ack_preserve_current_target() {
        let mut state = opened(2);
        let request = start(&mut state);
        let StructureIntent::Compare {
            request_id,
            cancelled,
            origin,
            target,
        } = &request;
        let old_id = *request_id;
        let cancelled = Arc::clone(cancelled);
        assert_eq!(state.pending_identities(old_id), Some((origin, target)));
        let (_, old_data) = compared(request, "a\n".into(), "b\n".into());
        state.open(vec![source(0), source(1)], source(0).identity);
        assert!(cancelled.load(Ordering::Relaxed));
        state.done(old_id, Ok(old_data));
        assert!(state.dialog.as_ref().unwrap().diff.is_none());
        let request = start(&mut state);
        let (id, data) = compared(request, "left\n".into(), "right\n".into());
        state.done(old_id, Err("stale failure".into()));
        assert!(state.pending_identities(id).is_some());
        state.done(id, Ok(data));
        state.done(id, Err("duplicate failure".into()));
        let dialog = state.dialog.as_ref().unwrap();
        assert!(dialog.error.is_none());
        assert_eq!(&*dialog.diff.as_ref().unwrap().data.origin.sql, "left\n");
        assert!(state.pending_identities(id).is_none());
    }

    #[test]
    fn returned_identity_is_checked_and_close_releases_snapshots_and_focus_once() {
        let mut state = opened(2);
        let request = start(&mut state);
        let (id, mut data) = compared(request, "a\n".into(), "b\n".into());
        data.target.identity.path.push("wrong".into());
        state.done(id, Ok(data));
        assert!(state.dialog.as_ref().unwrap().diff.is_none());
        assert!(
            state
                .dialog
                .as_ref()
                .unwrap()
                .error
                .as_deref()
                .unwrap()
                .contains("do not match")
        );
        let mut state = completed(2);
        let weak = Arc::downgrade(
            &state
                .dialog
                .as_ref()
                .unwrap()
                .diff
                .as_ref()
                .unwrap()
                .data
                .origin
                .sql,
        );
        let focus = Id::new("previous_editor");
        state.dialog.as_mut().unwrap().focus = Some(focus);
        state.cancel();
        assert!(!state.is_open());
        assert!(weak.upgrade().is_none());
        assert_eq!(state.take_restore_focus(), Some(focus));
        assert_eq!(state.take_restore_focus(), None);
        let mut state = opened(2);
        let StructureIntent::Compare { cancelled, .. } = start(&mut state);
        drop(state);
        assert!(cancelled.load(Ordering::Relaxed));
    }

    #[test]
    fn unicode_picker_caches_matches_and_keyboard_reveals_a_far_choice() {
        let mut state = opened(1000);
        let dialog = state.dialog.as_mut().unwrap();
        dialog.sources[999].label = "Source 999 / main.Łódź [999]".into();
        dialog.display[999]
            .label
            .clone_from(&dialog.sources[999].label);
        let mut h = egui_kittest::Harness::builder()
            .with_size([1100.0, 500.0])
            .build_ui_state(
                |ui, state: &mut StructureCompareState| {
                    state.show(ui.ctx());
                },
                state,
            );
        h.run_steps(5);
        assert!(
            h.query_by_role_and_label(
                egui::accesskit::Role::Button,
                "Source 999 / main.Łódź [999]"
            )
            .is_none()
        );
        h.key_press(Key::ArrowUp);
        h.run_steps(5);
        assert_eq!(h.state().dialog.as_ref().unwrap().target, Some(999));
        assert!(
            h.query_by_role_and_label(
                egui::accesskit::Role::Button,
                "Source 999 / main.Łódź [999]"
            )
            .is_some()
        );
        h.get_by_label("Find Object").type_text("ŁÓDŹ");
        h.run_steps(5);
        assert_eq!(h.state().dialog.as_ref().unwrap().matches, [999]);
        let searches = h.state().dialog.as_ref().unwrap().searches;
        h.run_steps(8);
        assert_eq!(h.state().dialog.as_ref().unwrap().searches, searches);
        for label in ["Compare", "Close"] {
            let rect = h.get_by_label(label).rect();
            assert!(
                rect.right() <= 1100.0 && rect.bottom() <= 500.0,
                "{label}: {rect:?}"
            );
        }
    }

    #[test]
    fn virtual_lines_reuse_galleys_and_f7_reveals_the_next_hunk() {
        let mut h = egui_kittest::Harness::builder()
            .with_size([1100.0, 500.0])
            .build_ui_state(
                |ui, state: &mut StructureCompareState| {
                    state.show(ui.ctx());
                },
                completed(10000),
            );
        h.run_steps(6);
        let view = h.state().dialog.as_ref().unwrap().diff.as_ref().unwrap();
        assert!(view.painted_rows < 20);
        assert!(view.cache.len() < 40);
        let key = *view.cache.keys().next().unwrap();
        let pointer = view.cache[&key].label.as_ptr();
        let galley = Arc::as_ptr(&view.cache[&key].galley);
        h.run_steps(8);
        let view = h.state().dialog.as_ref().unwrap().diff.as_ref().unwrap();
        assert_eq!(view.cache[&key].label.as_ptr(), pointer);
        assert_eq!(Arc::as_ptr(&view.cache[&key].galley), galley);
        h.key_press(Key::F7);
        h.run_steps(5);
        assert_eq!(
            h.state()
                .dialog
                .as_ref()
                .unwrap()
                .diff
                .as_ref()
                .unwrap()
                .selected,
            Some(1000)
        );
        let left = h.get_by_label("Origin line 1001: column_1000 TEXT,").rect();
        let right = h
            .get_by_label("Target line 1001: column_1000 INTEGER,")
            .rect();
        assert!((left.center().y - right.center().y).abs() < 1.0);
        assert!(left.top() >= 0.0 && right.bottom() <= 500.0);
        for label in [
            "Copy Origin DDL",
            "Copy Target DDL",
            "Change Objects",
            "Close",
        ] {
            let rect = h.get_by_label(label).rect();
            assert!(
                rect.right() <= 1100.0 && rect.bottom() <= 500.0,
                "{label}: {rect:?}"
            );
        }
    }

    #[test]
    fn endings_are_explained_and_copy_keeps_long_original_text() {
        assert_eq!(line_ending("a\n"), "Line ending: LF");
        assert_eq!(line_ending("a\r\n"), "Line ending: CRLF");
        assert_eq!(line_ending("a"), "No final newline");
        let mut state = opened(2);
        let request = start(&mut state);
        let left = format!("{}\r\n", "Żółw ".repeat(2000));
        let right = left.trim_end_matches(['\r', '\n']).to_owned();
        let (id, data) = compared(request, left.clone(), right);
        state.done(id, Ok(data));
        let mut h = egui_kittest::Harness::builder()
            .with_size([1100.0, 500.0])
            .build_ui_state(
                |ui, state: &mut StructureCompareState| {
                    state.show(ui.ctx());
                },
                state,
            );
        h.run_steps(5);
        let view = h.state().dialog.as_ref().unwrap().diff.as_ref().unwrap();
        assert!(view.truncated);
        assert!(view.cache[&(false, 0)].description.contains("CRLF"));
        assert!(
            view.cache[&(true, 0)]
                .description
                .contains("No final newline")
        );
        assert!(view.cache[&(false, 0)].label.len() < 4200);
        h.get_by_label("Copy Origin DDL").click();
        h.run_steps(1);
        assert!(h.output().platform_output.commands.iter().any(
            |command| matches!(command, egui::OutputCommand::CopyText(text) if text == &left)
        ));
    }

    #[test]
    #[ignore = "release CPU DDL view benchmark; no GPU timing"]
    fn benchmark_ddl_view_cache() {
        #[allow(clippy::assertions_on_constants)]
        {
            assert!(!cfg!(debug_assertions), "run with --release");
        }
        fn median(times: &mut [std::time::Duration]) -> std::time::Duration {
            times.sort_unstable();
            times[times.len() / 2]
        }
        for count in [500, 5000, 100000] {
            let mut state = completed(count);
            let view = state.dialog.as_mut().unwrap().diff.take().unwrap();
            let labels = view.labels.clone();
            let mut data = view.data;
            let mut build = Vec::new();
            for sample in 0..8 {
                let labels = labels.clone();
                let start = std::time::Instant::now();
                let view = DiffView::new(data, labels, 1);
                if sample > 0 {
                    build.push(start.elapsed());
                }
                data = view.data;
            }
            state.dialog.as_mut().unwrap().diff = Some(DiffView::new(data, labels, 1));
            let mut h = egui_kittest::Harness::builder()
                .with_size([1100.0, 500.0])
                .build_ui_state(
                    |ui, state: &mut StructureCompareState| {
                        state.show(ui.ctx());
                    },
                    state,
                );
            h.run_steps(10);
            let mut idle = Vec::new();
            let mut scroll = Vec::new();
            for sample in 0..7 {
                let start = std::time::Instant::now();
                h.run_steps(100);
                idle.push(start.elapsed() / 100);
                let start = std::time::Instant::now();
                for step in 0..50 {
                    let view = h
                        .state_mut()
                        .dialog
                        .as_mut()
                        .unwrap()
                        .diff
                        .as_mut()
                        .unwrap();
                    view.selected = Some((sample * 50 + step) * 17 % count);
                    view.reveal = true;
                    h.run_steps(1);
                }
                scroll.push(start.elapsed() / 50);
            }
            eprintln!(
                "ddl_view lines={count} samples=7 median_new={:?} median_idle_frame={:?} median_reveal_frame={:?}",
                median(&mut build),
                median(&mut idle),
                median(&mut scroll)
            );
        }
    }
}
