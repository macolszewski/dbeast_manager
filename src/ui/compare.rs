//! Read-only comparisons of captured result pages; data work stays on the worker.

use super::theme;
use crate::{
    compare::{ColumnPair, CompareRequest, CompareSpec, ComparedData, Difference},
    model::{QueryResult, Value},
};
use egui::{Align, Color32, Id, RichText, Sense, Ui};
use std::{
    collections::HashMap,
    fmt::Write,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);
const ROW_HEIGHT: f32 = 26.0;
const COLUMN_WIDTH: f32 = 140.0;
const STATUS_WIDTH: f32 = 76.0;
const CELL_BYTES: usize = 256;

pub(super) struct CompareSource {
    pub(super) id: (u64, u64),
    pub(super) label: String,
    pub(super) sql: String,
    pub(super) scope: String,
    pub(super) data: Arc<QueryResult>,
    pub(super) key: Vec<usize>,
    pub(super) blocked: Option<String>,
}

struct SourceDisplay {
    label: String,
    sql: String,
    scope: String,
    columns: Vec<String>,
    mapping_labels: Vec<String>,
}
struct Mapping {
    right: Option<usize>,
    key: bool,
    key_label: String,
}
#[derive(Clone)]
struct Labels {
    left: String,
    right: String,
    left_scope: String,
    right_scope: String,
    excluded: String,
}
struct Pending {
    id: u64,
    cancelled: Arc<AtomicBool>,
    labels: Labels,
}

struct Dialog {
    sources: Vec<CompareSource>,
    display: Vec<SourceDisplay>,
    left: usize,
    right: Option<usize>,
    mapping: Vec<Mapping>,
    spec: CompareSpec,
    reason: Option<String>,
    excluded: String,
    error: Option<String>,
    pending: Option<Pending>,
    diff: Option<DiffView>,
    focus: Option<Id>,
}

#[derive(Default)]
pub(super) struct CompareState {
    dialog: Option<Dialog>,
}

impl Drop for CompareState {
    fn drop(&mut self) {
        self.close();
    }
}

impl CompareState {
    pub(super) fn open(&mut self, sources: Vec<CompareSource>, left: (u64, u64)) {
        self.close();
        if sources.is_empty() {
            return;
        }
        let left = sources
            .iter()
            .position(|source| source.id == left)
            .unwrap_or(0);
        let right = sources
            .iter()
            .enumerate()
            .find(|(index, source)| *index != left && source.blocked.is_none())
            .or_else(|| sources.iter().enumerate().find(|(index, _)| *index != left))
            .map(|(index, _)| index);
        let display = sources
            .iter()
            .map(|source| SourceDisplay {
                label: bounded(&source.label, 512),
                sql: bounded(&source.sql, 4096),
                scope: bounded(&source.scope, 2048),
                mapping_labels: source
                    .data
                    .columns
                    .iter()
                    .enumerate()
                    .map(|(index, column)| {
                        format!(
                            "Map left column: {} {}",
                            index + 1,
                            bounded(&column.name, 256)
                        )
                    })
                    .collect(),
                columns: source
                    .data
                    .columns
                    .iter()
                    .enumerate()
                    .map(|(index, column)| format!("{}: {}", index + 1, bounded(&column.name, 256)))
                    .collect(),
            })
            .collect();
        let mut dialog = Dialog {
            sources,
            display,
            left,
            right,
            mapping: Vec::new(),
            spec: CompareSpec {
                columns: Vec::new(),
                key_pairs: Vec::new(),
            },
            reason: None,
            excluded: String::new(),
            error: None,
            pending: None,
            diff: None,
            focus: None,
        };
        dialog.propose_mapping();
        self.dialog = Some(dialog);
    }

    fn close(&mut self) {
        if let Some(mut dialog) = self.dialog.take() {
            dialog.cancel_pending();
        }
    }

    pub(super) fn is_open(&self) -> bool {
        self.dialog.is_some()
    }

    pub(super) fn done(&mut self, request_id: u64, result: Result<ComparedData, String>) {
        let Some(dialog) = &mut self.dialog else {
            return;
        };
        if !dialog
            .pending
            .as_ref()
            .is_some_and(|pending| pending.id == request_id)
        {
            return;
        }
        let pending = dialog.pending.take().unwrap();
        match result {
            Ok(data) => {
                dialog.diff = Some(DiffView::new(data, pending.labels, request_id));
                dialog.error = None;
            }
            Err(error) => dialog.error = Some(bounded(&error, 4096)),
        }
    }

    pub(super) fn show(&mut self, ctx: &egui::Context) -> Option<CompareRequest> {
        let dialog = self.dialog.as_mut()?;
        if dialog.focus.is_none() {
            dialog.focus = ctx.memory(|memory| memory.focused());
        }
        let screen = ctx.content_rect();
        let mut open = true;
        let mut close = !ctx.any_popup_open()
            && ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        if close
            && let Some(diff) = &mut dialog.diff
            && diff.value.is_some()
        {
            diff.value = None;
            close = false;
        }
        let mut request = None;
        let width = (screen.width() - 48.0).clamp(420.0, 1050.0);
        let height = (screen.height() - 110.0).clamp(280.0, 620.0);
        theme::show_window(ctx, "Compare Data", &mut open, |window| window
            .id(Id::new("compare_data_window")).collapsible(false)
            .default_pos(egui::pos2(screen.center().x - width * 0.5, screen.top() + 90.0))
            .default_size(egui::vec2(width, height)).min_size(egui::vec2(420.0, 260.0))
            .max_size(screen.size() - egui::vec2(24.0, 64.0)), |ui| {
                ui.label(RichText::new("Fetched pages · Exact typed values").strong());
                ui.add(egui::Label::new(RichText::new("Snapshots captured when Compare Data opened. Local filters and sorting are ignored.").small().color(theme::TEXT_MUTED)).truncate())
                    .on_hover_text("Left only and Right only refer to the other fetched page, not the entire table.");
                if let Some(diff) = &mut dialog.diff {
                    // The footer is reserved first: the diff takes all the height left.
                    egui::Panel::bottom("compare_data_footer")
                        .resizable(false)
                        .frame(egui::Frame::new())
                        .show(ui, |ui| {
                            theme::dialog_footer(ui, |_| {}, |ui| {
                                close |= theme::dialog_button(ui, "Close").clicked();
                            });
                        });
                    let back = diff.show(ui);
                    if back { dialog.diff = None; }
                } else {
                    let (start, cancel) = dialog.setup_ui(ui);
                    if start { request = dialog.start(); }
                    close |= cancel;
                }
            });
        if !open || close {
            let focus = dialog.focus;
            self.close();
            if let Some(focus) = focus {
                ctx.memory_mut(|memory| memory.request_focus(focus));
            }
        }
        request
    }
}

impl Dialog {
    fn cancel_pending(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.cancelled.store(true, Ordering::Relaxed);
        }
    }

    fn propose_mapping(&mut self) {
        self.cancel_pending();
        self.error = None;
        self.mapping = self.sources[self.left]
            .data
            .columns
            .iter()
            .map(|_| Mapping {
                right: None,
                key: false,
                key_label: String::new(),
            })
            .collect();
        if let Some(right) = self.right {
            for pair in crate::compare::matching_columns(
                &self.sources[self.left].data,
                &self.sources[right].data,
            ) {
                self.mapping[pair.left].right = Some(pair.right);
            }
            let left_key = &self.sources[self.left].key;
            let right_key = &self.sources[right].key;
            let full_key = !left_key.is_empty()
                && left_key.len() == right_key.len()
                && left_key.iter().all(|&left| {
                    self.mapping
                        .get(left)
                        .and_then(|entry| entry.right)
                        .is_some_and(|right| right_key.contains(&right))
                });
            if full_key {
                for &index in left_key {
                    self.mapping[index].key = true;
                }
            }
        }
        self.refresh_spec();
    }

    fn refresh_spec(&mut self) {
        self.spec.columns.clear();
        self.spec.key_pairs.clear();
        for (left, entry) in self.mapping.iter_mut().enumerate() {
            entry.key_label = format!(
                "Key: {} = {}",
                bounded(&self.sources[self.left].data.columns[left].name, 128),
                self.right
                    .and_then(|right| entry
                        .right
                        .map(|column| bounded(&self.sources[right].data.columns[column].name, 128)))
                    .unwrap_or_else(|| "<excluded>".into())
            );
            if let Some(right) = entry.right {
                if entry.key {
                    self.spec.key_pairs.push(self.spec.columns.len());
                }
                self.spec.columns.push(ColumnPair { left, right });
            }
        }
        self.reason = if let Some(right) = self.right {
            if self.left == right {
                Some("Choose two different result snapshots".into())
            } else if let Some(reason) = &self.sources[self.left].blocked {
                Some(reason.clone())
            } else if let Some(reason) = &self.sources[right].blocked {
                Some(reason.clone())
            } else {
                crate::compare::validate_spec(
                    &self.sources[self.left].data,
                    &self.sources[right].data,
                    &self.spec,
                )
                .err()
            }
        } else {
            Some("Open a second result to compare".into())
        };
        let excluded_left: Vec<_> = self
            .mapping
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.right.is_none())
            .map(|(index, _)| self.display[self.left].columns[index].as_str())
            .collect();
        let excluded_right: Vec<_> = self
            .right
            .map(|right| {
                self.display[right]
                    .columns
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| {
                        !self.mapping.iter().any(|entry| entry.right == Some(*index))
                    })
                    .map(|(_, name)| name.as_str())
                    .collect()
            })
            .unwrap_or_default();
        self.excluded = format!(
            "Excluded columns · Left: {} · Right: {}",
            if excluded_left.is_empty() {
                "none".into()
            } else {
                excluded_left.join(", ")
            },
            if excluded_right.is_empty() {
                "none".into()
            } else {
                excluded_right.join(", ")
            }
        );
        self.excluded = bounded(&self.excluded, 4096);
    }

    fn start(&mut self) -> Option<CompareRequest> {
        if self.reason.is_some() || self.pending.is_some() {
            return None;
        }
        let right = self.right?;
        let id = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
        let cancelled = Arc::new(AtomicBool::new(false));
        let labels = Labels {
            left: self.display[self.left].label.clone(),
            right: self.display[right].label.clone(),
            left_scope: self.display[self.left].scope.clone(),
            right_scope: self.display[right].scope.clone(),
            excluded: self.excluded.clone(),
        };
        self.pending = Some(Pending {
            id,
            cancelled: Arc::clone(&cancelled),
            labels,
        });
        self.error = None;
        Some(CompareRequest {
            request_id: id,
            left: Arc::clone(&self.sources[self.left].data),
            right: Arc::clone(&self.sources[right].data),
            spec: self.spec.clone(),
            cancelled,
        })
    }

    fn setup_ui(&mut self, ui: &mut Ui) -> (bool, bool) {
        let busy = self.pending.is_some();
        let mut sources_changed = false;
        ui.add_enabled_ui(!busy, |ui| {
            ui.columns(2, |columns| {
                sources_changed |= source_combo(
                    &mut columns[0],
                    "Left result",
                    &mut self.left,
                    &self.display,
                    &self.sources,
                );
                source_info(&mut columns[0], &self.display[self.left]);
                if let Some(right) = &mut self.right {
                    sources_changed |= source_combo(
                        &mut columns[1],
                        "Right result",
                        right,
                        &self.display,
                        &self.sources,
                    );
                    source_info(&mut columns[1], &self.display[*right]);
                } else {
                    columns[1].label("Open a second result to compare");
                }
            });
        });
        if sources_changed {
            self.propose_mapping();
        }
        ui.separator();
        let mut start = false;
        let mut cancel = false;
        theme::section_header(ui, "Column mapping");
        if let Some(reason) = self.error.as_ref().or(self.reason.as_ref()) {
            ui.add(egui::Label::new(RichText::new(reason).color(theme::SQL_KEYWORD)).truncate())
                .on_hover_text(reason);
        }
        ui.add(
            egui::Label::new(
                RichText::new(&self.excluded)
                    .small()
                    .color(theme::TEXT_MUTED),
            )
            .truncate(),
        )
        .on_hover_text(&self.excluded);
        // Compare and Cancel live in the footer, right-aligned.
        let footer = |this: &mut Self, ui: &mut Ui, start: &mut bool, cancel: &mut bool| {
            let ready = this.reason.is_none();
            let mut stop = false;
            theme::dialog_footer(
                ui,
                |ui| {
                    if busy {
                        ui.spinner();
                        ui.label(RichText::new("Comparing fetched pages…").color(theme::TEXT_MUTED));
                        stop = ui.button("Cancel Comparison").clicked();
                    }
                },
                |ui| {
                    *cancel = theme::dialog_button(ui, "Cancel").clicked();
                    if !busy {
                        *start = ui
                            .add_enabled(
                                ready,
                                egui::Button::new(RichText::new("Compare").color(Color32::WHITE))
                                    .fill(theme::ACCENT)
                                    .min_size(egui::vec2(72.0, 26.0)),
                            )
                            .clicked();
                    }
                },
            );
            if stop {
                this.cancel_pending();
            }
        };
        let Some(right) = self.right else {
            footer(self, ui, &mut start, &mut cancel);
            return (start, cancel);
        };
        let mut mapping_changed = false;
        ui.add_enabled_ui(!busy, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("compare_mapping")
                .auto_shrink([false, false])
                .max_height((ui.available_height() - 56.0).max(80.0))
                .show_rows(ui, ROW_HEIGHT, self.mapping.len(), |ui, rows| {
                    for left in rows {
                        ui.push_id(left, |ui| {
                            ui.horizontal(|ui| {
                                let label = &self.display[self.left].mapping_labels[left];
                                ui.add_sized(
                                    [ui.available_width() * 0.27, ROW_HEIGHT],
                                    egui::Label::new(&self.display[self.left].columns[left])
                                        .halign(Align::LEFT)
                                        .truncate(),
                                );
                                let entry = &mut self.mapping[left];
                                let old = entry.right;
                                let response = egui::ComboBox::from_id_salt("column_map")
                                    .width((ui.available_width() * 0.5).max(120.0))
                                    .truncate()
                                    .selected_text(
                                        entry
                                            .right
                                            .map(|index| {
                                                self.display[right].columns[index].as_str()
                                            })
                                            .unwrap_or("<excluded>"),
                                    )
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(&mut entry.right, None, "<excluded>");
                                        for (index, name) in
                                            self.display[right].columns.iter().enumerate()
                                        {
                                            ui.selectable_value(
                                                &mut entry.right,
                                                Some(index),
                                                name,
                                            );
                                        }
                                    })
                                    .response;
                                response.widget_info(|| {
                                    egui::WidgetInfo::labeled(
                                        egui::WidgetType::ComboBox,
                                        !busy,
                                        label,
                                    )
                                });
                                if entry.right != old {
                                    mapping_changed = true;
                                    if entry.right.is_none() {
                                        entry.key = false;
                                    }
                                }
                                let response = ui.add_enabled(
                                    entry.right.is_some(),
                                    egui::Checkbox::new(&mut entry.key, "Key"),
                                );
                                response.widget_info(|| {
                                    egui::WidgetInfo::selected(
                                        egui::WidgetType::Checkbox,
                                        !busy && entry.right.is_some(),
                                        entry.key,
                                        &entry.key_label,
                                    )
                                });
                                response.clone().on_hover_text(&entry.key_label);
                                mapping_changed |= response.changed();
                            });
                        });
                    }
                });
        });
        if mapping_changed {
            self.refresh_spec();
            self.error = None;
            ui.ctx().request_repaint();
        }
        footer(self, ui, &mut start, &mut cancel);
        (start, cancel)
    }
}

fn source_combo(
    ui: &mut Ui,
    label: &str,
    selected: &mut usize,
    display: &[SourceDisplay],
    sources: &[CompareSource],
) -> bool {
    let old = *selected;
    let response = egui::ComboBox::from_id_salt(label)
        .width(ui.available_width())
        .truncate()
        .selected_text(&display[*selected].label)
        .show_ui(ui, |ui| {
            for (index, source) in display.iter().enumerate() {
                let response = ui.selectable_value(selected, index, &source.label);
                if let Some(reason) = &sources[index].blocked {
                    response.on_hover_text(reason);
                }
            }
        })
        .response;
    response.widget_info(|| {
        let mut info =
            egui::WidgetInfo::labeled(egui::WidgetType::ComboBox, ui.is_enabled(), label);
        info.current_text_value = Some(display[*selected].label.clone());
        info
    });
    old != *selected
}

fn source_info(ui: &mut Ui, display: &SourceDisplay) {
    ui.add(
        egui::Label::new(
            RichText::new(&display.scope)
                .small()
                .color(theme::TEXT_MUTED),
        )
        .wrap(),
    )
    .on_hover_ui(|ui| {
        ui.label(&display.scope);
        ui.label(RichText::new(&display.sql).monospace());
    });
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
struct CellKey {
    right: bool,
    row: usize,
    column: usize,
}
struct ValuePreview {
    key: CellKey,
    label: String,
    preview: String,
}
struct CellPreview {
    text: String,
    label: String,
}
struct DiffView {
    data: ComparedData,
    counts: [String; 4],
    column_names: Vec<(String, String)>,
    labels: Labels,
    id: u64,
    visible: Vec<usize>,
    differences: Vec<usize>,
    show_identical: bool,
    selected: Option<usize>,
    reveal: bool,
    previews: HashMap<CellKey, CellPreview>,
    preview_range: (std::ops::Range<usize>, std::ops::Range<usize>),
    value: Option<ValuePreview>,
    #[cfg(test)]
    rebuilds: usize,
    #[cfg(test)]
    painted_rows: usize,
}

impl DiffView {
    fn new(data: ComparedData, labels: Labels, id: u64) -> Self {
        let differences: Vec<_> = data
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.difference != Difference::Equal)
            .map(|(index, _)| index)
            .collect();
        let selected = differences.first().copied();
        Self {
            counts: [
                format!("Equal: {}", data.counts.equal),
                format!("Changed: {}", data.counts.changed),
                format!("Left only: {}", data.counts.left_only),
                format!("Right only: {}", data.counts.right_only),
            ],
            column_names: data
                .spec
                .columns
                .iter()
                .map(|pair| {
                    (
                        bounded(&data.left.columns[pair.left].name, 256),
                        bounded(&data.right.columns[pair.right].name, 256),
                    )
                })
                .collect(),
            data,
            labels,
            id,
            visible: differences.clone(),
            differences,
            show_identical: false,
            selected,
            reveal: false,
            previews: HashMap::new(),
            preview_range: (0..0, 0..0),
            value: None,
            #[cfg(test)]
            rebuilds: 1,
            #[cfg(test)]
            painted_rows: 0,
        }
    }

    fn refresh_visible(&mut self) {
        self.visible.clear();
        if self.show_identical {
            self.visible.extend(0..self.data.rows.len());
        } else {
            self.visible.extend_from_slice(&self.differences);
        }
        if self
            .selected
            .is_some_and(|selected| self.visible.binary_search(&selected).is_err())
        {
            self.selected = self.visible.first().copied();
        }
        self.previews.clear();
        #[cfg(test)]
        {
            self.rebuilds += 1;
        }
    }

    fn next(&mut self, previous: bool) {
        if self.differences.is_empty() {
            return;
        }
        let index = if previous {
            self.differences
                .partition_point(|&index| self.selected.is_none_or(|selected| index < selected))
                .checked_sub(1)
                .unwrap_or(self.differences.len() - 1)
        } else {
            self.differences
                .partition_point(|&index| self.selected.is_some_and(|selected| index <= selected))
                % self.differences.len()
        };
        self.selected = Some(self.differences[index]);
        self.reveal = true;
    }

    fn show(&mut self, ui: &mut Ui) -> bool {
        let mut back = false;
        ui.push_id(self.id, |ui| {
            ui.horizontal(|ui| {
                ui.strong("Diff Viewer");
                for label in &self.counts {
                    ui.label(label);
                }
            });
            ui.horizontal(|ui| {
                if ui
                    .checkbox(&mut self.show_identical, "Show identical")
                    .changed()
                {
                    self.refresh_visible();
                }
                if ui
                    .add_enabled(
                        !self.differences.is_empty(),
                        egui::Button::new("Previous Difference"),
                    )
                    .clicked()
                {
                    self.next(true);
                }
                if ui
                    .add_enabled(
                        !self.differences.is_empty(),
                        egui::Button::new("Next Difference"),
                    )
                    .clicked()
                {
                    self.next(false);
                }
                back = ui.button("Change Comparison").clicked();
            });
            ui.columns(2, |columns| {
                for (ui, (label, scope)) in columns.iter_mut().zip([
                    (&self.labels.left, &self.labels.left_scope),
                    (&self.labels.right, &self.labels.right_scope),
                ]) {
                    ui.add(egui::Label::new(RichText::new(label).strong()).truncate())
                        .on_hover_text(label);
                    ui.add(egui::Label::new(RichText::new(scope).small()).wrap())
                        .on_hover_text(scope);
                }
            });
            ui.add(
                egui::Label::new(
                    RichText::new(&self.labels.excluded)
                        .small()
                        .color(theme::TEXT_MUTED),
                )
                .truncate(),
            )
            .on_hover_text(&self.labels.excluded);
            self.grid(ui);
            self.value_ui(ui.ctx());
        });
        back
    }

    fn grid(&mut self, ui: &mut Ui) {
        #[cfg(test)]
        {
            self.painted_rows = 0;
        }
        if self.visible.is_empty() {
            ui.label(if self.data.rows.is_empty() {
                "Both fetched pages are empty"
            } else {
                "No differences in mapped columns"
            });
            return;
        }
        let width = ui.available_width();
        let side_width = ((width - STATUS_WIDTH - 16.0) * 0.5).max(120.0);
        let content_width = (self.data.spec.columns.len() as f32 * COLUMN_WIDTH).max(side_width)
            + side_width
            + STATUS_WIDTH
            + 16.0;
        let (header, _) = ui.allocate_exact_size(egui::vec2(width, ROW_HEIGHT), Sense::hover());
        let mut scroll = egui::ScrollArea::both()
            .id_salt("diff_rows")
            .auto_shrink([false, false])
            .max_height((ui.available_height() - 36.0).max(60.0));
        if std::mem::take(&mut self.reveal)
            && let Some(position) = self
                .selected
                .and_then(|selected| self.visible.binary_search(&selected).ok())
        {
            scroll = scroll.vertical_scroll_offset(
                position as f32 * (ROW_HEIGHT + ui.spacing().item_spacing.y),
            );
        }
        let mut open_value = None;
        let output = scroll.show_rows(ui, ROW_HEIGHT, self.visible.len(), |ui, rows| {
            ui.set_min_width(content_width);
            let clip = ui.clip_rect();
            let offset = (clip.left() - ui.min_rect().left()).max(0.0);
            let first_column = (offset / COLUMN_WIDTH).floor() as usize;
            let last_column = ((offset + side_width) / COLUMN_WIDTH).ceil() as usize;
            let columns = first_column.min(self.data.spec.columns.len())
                ..last_column.min(self.data.spec.columns.len());
            if self.preview_range != (rows.clone(), columns.clone()) {
                self.previews.clear();
                self.preview_range = (rows.clone(), columns.clone());
            }
            for position in rows {
                #[cfg(test)]
                {
                    self.painted_rows += 1;
                }
                let index = self.visible[position];
                let compared = &self.data.rows[index];
                let (row_rect, _) =
                    ui.allocate_exact_size(egui::vec2(content_width, ROW_HEIGHT), Sense::hover());
                let left = egui::Rect::from_min_size(
                    egui::pos2(clip.left(), row_rect.top()),
                    egui::vec2(side_width, ROW_HEIGHT),
                );
                let right = left.translate(egui::vec2(side_width + STATUS_WIDTH, 0.0));
                let status = egui::Rect::from_min_size(
                    egui::pos2(left.right(), row_rect.top()),
                    egui::vec2(STATUS_WIDTH, ROW_HEIGHT),
                );
                ui.put(
                    status,
                    egui::Label::new(RichText::new(kind(compared.difference)).small()).truncate(),
                );
                for (is_right, source_row, pane) in
                    [(false, compared.left, left), (true, compared.right, right)]
                {
                    let source = if is_right {
                        &self.data.right
                    } else {
                        &self.data.left
                    };
                    let old_clip = ui.clip_rect();
                    ui.set_clip_rect(old_clip.intersect(pane));
                    let fill = match compared.difference {
                        Difference::Changed => Color32::from_rgb(39, 56, 76),
                        Difference::LeftOnly if !is_right => Color32::from_rgb(66, 49, 49),
                        Difference::RightOnly if is_right => Color32::from_rgb(39, 66, 49),
                        _ => theme::BG_EDITOR,
                    };
                    ui.painter().rect_filled(pane, 0.0, fill);
                    if let Some(source_row) = source_row {
                        for pair in columns.clone() {
                            let column = if is_right {
                                self.data.spec.columns[pair].right
                            } else {
                                self.data.spec.columns[pair].left
                            };
                            let key = CellKey {
                                right: is_right,
                                row: source_row,
                                column,
                            };
                            let value = &source.rows[source_row][column];
                            let column_name = if is_right {
                                &self.column_names[pair].1
                            } else {
                                &self.column_names[pair].0
                            };
                            let preview = self.previews.entry(key).or_insert_with(|| {
                                let text = preview_value(value, CELL_BYTES);
                                let label = format!(
                                    "{} row {}, {}: {}",
                                    if is_right { "Right" } else { "Left" },
                                    source_row + 1,
                                    column_name,
                                    text
                                );
                                CellPreview { text, label }
                            });
                            let rect = egui::Rect::from_min_size(
                                egui::pos2(
                                    pane.left() + pair as f32 * COLUMN_WIDTH - offset,
                                    pane.top(),
                                ),
                                egui::vec2(COLUMN_WIDTH, ROW_HEIGHT),
                            );
                            let changed = self.data.cell_changed(index, pair);
                            if changed {
                                ui.painter().rect_filled(
                                    rect.intersect(pane),
                                    0.0,
                                    Color32::from_rgb(69, 91, 123),
                                );
                            }
                            let response = aligned_label(
                                ui,
                                rect.shrink2(egui::vec2(4.0, 0.0)),
                                egui::Label::new(preview.text.as_str())
                                    .truncate()
                                    .sense(Sense::click()),
                                value.is_numeric(),
                            );
                            response.widget_info(|| {
                                egui::WidgetInfo::selected(
                                    egui::WidgetType::Button,
                                    true,
                                    self.selected == Some(index),
                                    &preview.label,
                                )
                            });
                            ui.ctx().accesskit_node_builder(response.id, |node| {
                                node.set_description(if changed {
                                    "Changed"
                                } else {
                                    kind(compared.difference)
                                })
                            });
                            if response.clicked() {
                                self.selected = Some(index);
                            }
                            if response.double_clicked() {
                                open_value = Some((key, preview.label.clone()));
                            }
                            response.context_menu(|ui| {
                                if ui.button("View Value").clicked() {
                                    open_value = Some((key, preview.label.clone()));
                                    ui.close();
                                }
                                if ui.button("Copy Full Value").clicked() {
                                    ui.ctx().copy_text(full_value(value));
                                    ui.close();
                                }
                            });
                        }
                    }
                    ui.set_clip_rect(old_clip);
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
        self.headers(ui, header, side_width, output.state.offset.x);
        if let Some((key, label)) = open_value {
            let data = if key.right {
                &self.data.right
            } else {
                &self.data.left
            };
            self.value = Some(ValuePreview {
                key,
                label,
                preview: preview_value(&data.rows[key.row][key.column], 4096),
            });
        }
    }

    fn headers(&self, ui: &mut Ui, header: egui::Rect, side_width: f32, offset: f32) {
        let first = (offset / COLUMN_WIDTH).floor() as usize;
        let end = ((offset + side_width) / COLUMN_WIDTH).ceil() as usize;
        for right in [false, true] {
            let left = header.left()
                + if right {
                    side_width + STATUS_WIDTH
                } else {
                    0.0
                };
            let pane = egui::Rect::from_min_size(
                egui::pos2(left, header.top()),
                egui::vec2(side_width, ROW_HEIGHT),
            );
            let old_clip = ui.clip_rect();
            ui.set_clip_rect(old_clip.intersect(pane));
            for pair in first..end.min(self.data.spec.columns.len()) {
                let name = if right {
                    &self.column_names[pair].1
                } else {
                    &self.column_names[pair].0
                };
                let rect = egui::Rect::from_min_size(
                    egui::pos2(left + pair as f32 * COLUMN_WIDTH - offset, header.top()),
                    egui::vec2(COLUMN_WIDTH, ROW_HEIGHT),
                );
                aligned_label(
                    ui,
                    rect.shrink2(egui::vec2(4.0, 0.0)),
                    egui::Label::new(RichText::new(name).strong()).truncate(),
                    false,
                );
            }
            ui.set_clip_rect(old_clip);
        }
    }

    fn value_ui(&mut self, ctx: &egui::Context) {
        let Some(value) = &self.value else {
            return;
        };
        let mut open = true;
        theme::show_window(ctx, "Compared Value", &mut open, |window| window
            .id(Id::new(("compared_value", self.id)))
            
            .default_width(480.0), |ui| {
                ui.add(egui::Label::new(&value.label).truncate());
                ui.label("Read-only preview. Copy Full Value preserves the complete value.");
                if ui.button("Copy Full Value").clicked() {
                    let source = if value.key.right {
                        &self.data.right
                    } else {
                        &self.data.left
                    };
                    ui.ctx()
                        .copy_text(full_value(&source.rows[value.key.row][value.key.column]));
                }
                egui::ScrollArea::both().max_height(260.0).show(ui, |ui| {
                    ui.label(RichText::new(&value.preview).monospace());
                });
            });
        if !open {
            self.value = None;
        }
    }
}

fn aligned_label(ui: &mut Ui, rect: egui::Rect, label: egui::Label, right: bool) -> egui::Response {
    // Ui::put/place use a centered layout even when Label::halign is set.
    // A non-justified horizontal layout anchors the actual widget rectangle.
    ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(if right {
        egui::Layout::right_to_left(Align::Center)
    } else {
        egui::Layout::left_to_right(Align::Center)
    }))
    .add(label)
}

fn kind(difference: Difference) -> &'static str {
    match difference {
        Difference::Equal => "Equal",
        Difference::Changed => "Changed",
        Difference::LeftOnly => "Left only",
        Difference::RightOnly => "Right only",
    }
}

fn bounded(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        text.to_owned()
    } else {
        format!("{}…", &text[..text.floor_char_boundary(limit)])
    }
}

fn preview_value(value: &Value, limit: usize) -> String {
    match value {
        Value::Null => "<null>".into(),
        Value::Text(text) | Value::Decimal(text) => bounded(text, limit)
            .replace(['\n', '\r'], "↵")
            .replace('\t', "⇥"),
        Value::Bytes(bytes) => {
            let mut text = String::with_capacity(96);
            text.push_str("0x");
            for byte in bytes.iter().take(32) {
                let _ = write!(text, "{byte:02X}");
            }
            if bytes.len() > 32 {
                let _ = write!(text, "… ({} bytes)", bytes.len());
            }
            text
        }
        _ => value.to_string(),
    }
}

fn full_value(value: &Value) -> String {
    if let Value::Bytes(bytes) = value {
        let mut text = String::with_capacity(bytes.len().saturating_mul(2).saturating_add(2));
        text.push_str("0x");
        for byte in bytes {
            let _ = write!(text, "{byte:02X}");
        }
        text
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Column;
    use egui_kittest::kittest::Queryable;

    fn source(id: u64, count: usize, changed: bool) -> CompareSource {
        CompareSource {
            id: (id, id),
            label: format!("Result {id}"),
            sql: "SELECT id, value FROM items".into(),
            scope: format!(
                "Fetched pages · source{id} / main · {count} rows · offset {} · limit {count} · more rows available",
                if id == 1 { 0 } else { 1000 }
            ),
            data: Arc::new(QueryResult {
                columns: ["id", "value"]
                    .into_iter()
                    .map(|name| Column {
                        name: name.into(),
                        type_name: "mixed".into(),
                    })
                    .collect(),
                rows: (0..count)
                    .map(|row| {
                        vec![
                            Value::Int(row as i64),
                            Value::Text(format!(
                                "{} {row}",
                                if changed { "after" } else { "before" }
                            )),
                        ]
                    })
                    .collect(),
                ..Default::default()
            }),
            key: vec![0],
            blocked: None,
        }
    }

    fn opened(count: usize) -> CompareState {
        let mut state = CompareState::default();
        state.open(
            vec![source(1, count, false), source(2, count, true)],
            (1, 1),
        );
        state
    }

    fn completed(count: usize) -> CompareState {
        let mut state = opened(count);
        let request = state.dialog.as_mut().unwrap().start().unwrap();
        let id = request.request_id;
        state.done(id, crate::compare::compare(request));
        state
    }

    #[test]
    fn replacement_cancels_pending_and_rejects_late_or_duplicate_results() {
        let mut state = opened(3);
        let request = state.dialog.as_mut().unwrap().start().unwrap();
        let old_id = request.request_id;
        let cancelled = Arc::clone(&request.cancelled);
        let old_result = crate::compare::compare(request).unwrap();
        state.open(vec![source(3, 4, false), source(4, 4, true)], (3, 3));
        assert!(cancelled.load(Ordering::Relaxed));
        state.done(old_id, Ok(old_result));
        assert!(state.dialog.as_ref().unwrap().diff.is_none());
        let request = state.dialog.as_mut().unwrap().start().unwrap();
        let id = request.request_id;
        state.done(old_id, Err("late failure".into()));
        assert_eq!(
            state.dialog.as_ref().unwrap().pending.as_ref().unwrap().id,
            id
        );
        state.done(id, crate::compare::compare(request));
        state.done(id, Err("duplicate failure".into()));
        assert!(state.dialog.as_ref().unwrap().error.is_none());
        assert_eq!(
            state
                .dialog
                .as_ref()
                .unwrap()
                .diff
                .as_ref()
                .unwrap()
                .data
                .counts
                .changed,
            4
        );
    }

    #[test]
    fn close_and_cancel_release_snapshots_without_mutating_sources() {
        let mut state = completed(3);
        let weak = Arc::downgrade(&state.dialog.as_ref().unwrap().sources[0].data);
        assert!(weak.upgrade().is_some());
        state.close();
        assert!(!state.is_open());
        assert!(weak.upgrade().is_none());
        let mut state = opened(3);
        let request = state.dialog.as_mut().unwrap().start().unwrap();
        state.close();
        assert!(request.cancelled.load(Ordering::Relaxed));
        assert!(crate::compare::compare(request).is_err());
    }

    #[test]
    fn mapping_is_explicit_and_only_a_full_matching_primary_key_is_proposed() {
        let mut left = source(1, 2, false);
        let mut right = source(2, 2, true);
        Arc::make_mut(&mut left.data).columns[1].name = "amount".into();
        Arc::make_mut(&mut right.data).columns[1].name = "balance".into();
        left.key = vec![0, 1];
        right.key = vec![0, 1];
        let mut state = CompareState::default();
        state.open(vec![left, right], (1, 1));
        let dialog = state.dialog.as_mut().unwrap();
        assert_eq!(dialog.mapping[1].right, None);
        assert!(
            dialog.spec.key_pairs.is_empty(),
            "a partial matching key is never proposed"
        );
        assert!(dialog.excluded.contains("amount"));
        assert!(dialog.excluded.contains("balance"));
        dialog.mapping[1].right = Some(1);
        dialog.mapping[0].key = true;
        dialog.mapping[1].key = true;
        dialog.refresh_spec();
        assert!(dialog.reason.is_none());
        assert_eq!(dialog.spec.key_pairs, [0, 1]);
        dialog.mapping[1].right = Some(0);
        dialog.refresh_spec();
        assert!(
            dialog
                .reason
                .as_ref()
                .unwrap()
                .contains("only one mapped pair")
        );
    }

    #[test]
    fn blocked_right_snapshot_remains_selectable_but_cannot_start() {
        let mut state = opened(2);
        let dialog = state.dialog.as_mut().unwrap();
        dialog.sources[1].blocked =
            Some("Submit or revert pending changes before comparing data".into());
        dialog.refresh_spec();
        assert!(dialog.reason.as_ref().unwrap().contains("pending changes"));
        assert!(dialog.start().is_none());
        assert_eq!(dialog.right, Some(1));
    }

    #[test]
    fn cell_text_and_headers_align_left_while_numbers_align_right() {
        for width in [1100.0, 1440.0] {
            let mut h = egui_kittest::Harness::builder()
                .with_size([width, 500.0])
                .build_ui_state(
                    |ui, positions: &mut Vec<(egui::Rect, egui::Rect, bool)>| {
                        positions.clear();
                        for side in [40.0, width * 0.5] {
                            for (row, text, right) in
                                [(0, "name", false), (1, "Ada", false), (2, "123", true)]
                            {
                                let rect = egui::Rect::from_min_size(
                                    egui::pos2(side, 40.0 + row as f32 * ROW_HEIGHT),
                                    egui::vec2(COLUMN_WIDTH - 8.0, ROW_HEIGHT),
                                );
                                let response = aligned_label(
                                    ui,
                                    rect,
                                    egui::Label::new(text).truncate(),
                                    right,
                                );
                                positions.push((rect, response.rect, right));
                            }
                        }
                    },
                    Vec::new(),
                );
            h.run_steps(3);
            for (cell, text, right) in h.state() {
                let offset = if *right {
                    cell.right() - text.right()
                } else {
                    text.left() - cell.left()
                };
                assert!(offset.abs() < 1.0, "cell {cell:?}, text {text:?}");
                assert!((cell.center().y - text.center().y).abs() < 1.0);
            }
        }
    }

    #[test]
    fn diff_virtualizes_rows_and_reuses_visible_previews_on_idle() {
        let mut h = egui_kittest::Harness::builder()
            .with_size([1100.0, 500.0])
            .build_ui_state(
                |ui, state: &mut CompareState| {
                    state.show(ui.ctx());
                },
                completed(10000),
            );
        h.run_steps(8);
        let diff = h.state().dialog.as_ref().unwrap().diff.as_ref().unwrap();
        assert!(diff.painted_rows < 20);
        assert!(diff.previews.len() < 100);
        let key = *diff.previews.keys().next().unwrap();
        let pointer = diff.previews[&key].text.as_ptr();
        let label_pointer = diff.previews[&key].label.as_ptr();
        h.run_steps(5);
        let diff = h.state().dialog.as_ref().unwrap().diff.as_ref().unwrap();
        assert_eq!(diff.rebuilds, 1);
        assert_eq!(diff.previews[&key].text.as_ptr(), pointer);
        assert_eq!(diff.previews[&key].label.as_ptr(), label_pointer);
        assert!(h.query_by_label("Left row 1, value: before 0").is_some());
        assert!(h.query_by_label("Right row 1, value: after 0").is_some());
        let diff = h
            .state_mut()
            .dialog
            .as_mut()
            .unwrap()
            .diff
            .as_mut()
            .unwrap();
        diff.selected = Some(9999);
        diff.reveal = true;
        h.run_steps(5);
        assert!(
            h.query_by_label("Left row 10000, value: before 9999")
                .is_some()
        );
        assert!(
            h.query_by_label("Right row 10000, value: after 9999")
                .is_some()
        );
        for label in ["Show identical", "Change Comparison", "Close"] {
            let rect = h.get_by_label(label).rect();
            assert!(
                rect.right() <= 1100.0 && rect.bottom() <= 500.0,
                "{label}: {rect:?}"
            );
        }
    }

    #[test]
    fn horizontal_scroll_reveals_the_same_last_column_on_both_sides() {
        let mut left = source(1, 2, false);
        let mut right = source(2, 2, true);
        for source in [&mut left, &mut right] {
            let changed = source.id.0 == 2;
            let data = Arc::make_mut(&mut source.data);
            data.columns = (0..20)
                .map(|index| Column {
                    name: format!("column{index}"),
                    type_name: "text".into(),
                })
                .collect();
            data.rows = (0..2)
                .map(|row| {
                    (0..20)
                        .map(|column| {
                            if column == 0 {
                                Value::Int(row)
                            } else {
                                Value::Text(format!(
                                    "{}-{row}-{column}",
                                    if changed { "after" } else { "before" }
                                ))
                            }
                        })
                        .collect()
                })
                .collect();
        }
        let mut state = CompareState::default();
        state.open(vec![left, right], (1, 1));
        let request = state.dialog.as_mut().unwrap().start().unwrap();
        let id = request.request_id;
        state.done(id, crate::compare::compare(request));
        let mut h = egui_kittest::Harness::builder()
            .with_size([1100.0, 500.0])
            .build_ui_state(
                |ui, state: &mut CompareState| {
                    state.show(ui.ctx());
                },
                state,
            );
        h.run_steps(6);
        let position = h
            .get_by_label("Left row 1, column1: before-0-1")
            .rect()
            .center();
        h.event(egui::Event::PointerMoved(position));
        h.run_steps(1);
        h.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(-5000.0, 0.0),
            modifiers: egui::Modifiers::NONE,
            phase: egui::TouchPhase::Move,
        });
        h.run_steps(25);
        let left = h.get_by_label("Left row 1, column19: before-0-19").rect();
        let right = h.get_by_label("Right row 1, column19: after-0-19").rect();
        assert!((left.center().y - right.center().y).abs() < 1.0);
        assert!(right.right() <= 1100.0 && left.left() >= 0.0);
        assert!(h.query_by_label("Left row 1, column0: 0").is_none());
        assert_eq!(
            h.state()
                .dialog
                .as_ref()
                .unwrap()
                .diff
                .as_ref()
                .unwrap()
                .rebuilds,
            1
        );
    }

    #[test]
    fn show_identical_changes_only_the_cached_projection_and_next_wraps() {
        let mut left = source(1, 3, false);
        let right = source(2, 3, true);
        Arc::make_mut(&mut left.data).rows[1][1] = Value::Text("after 1".into());
        let mut state = CompareState::default();
        state.open(vec![left, right], (1, 1));
        let request = state.dialog.as_mut().unwrap().start().unwrap();
        let id = request.request_id;
        state.done(id, crate::compare::compare(request));
        let diff = state.dialog.as_mut().unwrap().diff.as_mut().unwrap();
        assert_eq!(diff.visible, [0, 2]);
        assert_eq!(diff.data.counts.equal, 1);
        diff.show_identical = true;
        diff.refresh_visible();
        assert_eq!(diff.visible, [0, 1, 2]);
        diff.next(false);
        assert_eq!(diff.selected, Some(2));
        diff.next(false);
        assert_eq!(diff.selected, Some(0));
        diff.next(true);
        assert_eq!(diff.selected, Some(2));
        assert_eq!(diff.data.counts.changed, 2);
    }

    #[test]
    fn previews_are_bounded_but_copy_keeps_all_text_and_binary_bytes() {
        let text = Value::Text(format!("{}\nend", "Ł".repeat(10000)));
        assert!(preview_value(&text, CELL_BYTES).len() < 300);
        assert_eq!(full_value(&text).len(), 20004);
        let bytes = Value::Bytes((0..=255).collect());
        assert!(preview_value(&bytes, CELL_BYTES).len() < 100);
        assert_eq!(full_value(&bytes).len(), 514);
        assert!(full_value(&bytes).ends_with("FEFF"));
    }

    #[test]
    #[ignore = "release CPU diff view benchmark; no GPU timing"]
    fn benchmark_compare_view_cache() {
        #[allow(clippy::assertions_on_constants)]
        {
            assert!(!cfg!(debug_assertions), "run with --release");
        }
        fn median(values: &mut [std::time::Duration]) -> std::time::Duration {
            values.sort_unstable();
            values[values.len() / 2]
        }
        for count in [500, 5000, 100000] {
            let mut state = completed(count);
            let diff = state.dialog.as_mut().unwrap().diff.take().unwrap();
            let labels = diff.labels.clone();
            let mut data = diff.data;
            let mut build = Vec::new();
            for sample in 0..8 {
                let labels = labels.clone();
                let start = std::time::Instant::now();
                let view = DiffView::new(data, labels, 1);
                let elapsed = start.elapsed();
                if sample > 0 {
                    build.push(elapsed);
                }
                data = view.data;
            }
            state.dialog.as_mut().unwrap().diff = Some(DiffView::new(data, labels, 1));
            let mut h = egui_kittest::Harness::builder()
                .with_size([1100.0, 500.0])
                .build_ui_state(
                    |ui, state: &mut CompareState| {
                        state.show(ui.ctx());
                    },
                    state,
                );
            h.run_steps(10);
            let mut idle = Vec::new();
            let mut projection = Vec::new();
            for _ in 0..7 {
                let start = std::time::Instant::now();
                h.run_steps(100);
                idle.push(start.elapsed() / 100);
                let diff = h
                    .state_mut()
                    .dialog
                    .as_mut()
                    .unwrap()
                    .diff
                    .as_mut()
                    .unwrap();
                let start = std::time::Instant::now();
                diff.show_identical = true;
                diff.refresh_visible();
                diff.show_identical = false;
                diff.refresh_visible();
                projection.push(start.elapsed());
            }
            eprintln!(
                "compare_view rows={count} columns=2 samples=7 median_new={:?} median_projection_toggle_pair={:?} median_idle_frame={:?}",
                median(&mut build),
                median(&mut projection),
                median(&mut idle)
            );
        }
    }
}
