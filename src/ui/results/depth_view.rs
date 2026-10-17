//! Depth View: the fetched result stays a table, while one to three date
//! columns place each row in depth on a shared time axis. Projections of the
//! same row are connected and labelled with exact, signed deltas.
//!
//! Work proportional to the result happens only when the result, the series,
//! the window or the table order changes. A frame lays out the visible rows only.

use crate::ui::scene3d::{Camera, Preset, Vec3, View, quad_contains};
use super::{GridRow, ResultSet, Shown};
use crate::depth::{self, DepthModel, MAX_SERIES, Step, TimeColumn};
use crate::model::Value;
use crate::sqlgen::CellEdit;
use crate::ui::theme;
use egui::{Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};
use std::sync::Arc;

/// Room for a row card and, above it, the tabs of further time columns.
const ROW_PITCH: f32 = 58.0;
const TAB_HEIGHT: f32 = 20.0;
const CARD_HEIGHT: f32 = 30.0;
/// Default and smallest width of the details panel; the canvas keeps at least `MIN_CANVAS`.
const DETAILS_WIDTH: f32 = 340.0;
const MIN_DETAILS: f32 = 220.0;
/// Characters of one value shown in the details panel.
const DETAILS_CHARS: usize = 4_000;
const MIN_CANVAS: f32 = 320.0;
const GHOST_WIDTH: f32 = 120.0;
const HISTOGRAM_BINS: usize = 120;
/// World length of the whole time window along Z at depth scale 1.
const DEPTH_WORLD: f32 = 1_000.0;
const MAX_CARD_WIDTH: f32 = 1_100.0;
const COLUMN_GAP: f32 = 16.0;
/// Rows placed in the 3D scene at once; the slice is moved explicitly.
const ROWS_IN_SCENE: usize = 16;
/// Text size in world units; grid column widths (measured for 12 px text) are
/// scaled to match, so cards keep the table's proportions.
const TEXT_WORLD: f32 = 17.0;
const WIDTH_SCALE: f32 = TEXT_WORLD / 12.0;
const SERIES_NAMES: [&str; MAX_SERIES] = ["A", "B", "C"];
const SERIES_COLORS: [Color32; MAX_SERIES] = [
    Color32::from_rgb(0x4D, 0x8D, 0xF7),
    Color32::from_rgb(0xE5, 0x9B, 0x3C),
    Color32::from_rgb(0x5F, 0xB8, 0x6B),
];

/// Per-result view state. Derived data is keyed by the result allocation, so a
/// reload (a new `Arc`) never reuses times or row lists of an older version.
#[derive(Default)]
pub(in crate::ui) struct DepthState {
    pub open: bool,
    candidates: Option<(usize, Arc<Vec<TimeColumn>>)>,
    /// Cheap answer to "does the button apply", by data version.
    may_apply: Option<(usize, bool)>,
    /// Time analysis of a large result running off the UI thread.
    pending: Option<Pending>,
    pub series: Vec<usize>,
    /// Model keyed by the data version and the chosen series.
    model: Option<(usize, Vec<usize>, ModelResult)>,
    histogram: Vec<u32>,
    /// `[start, end)` in model microseconds; `None` until the first model.
    window: Option<(i64, i64)>,
    /// The user has moved or resized the window; otherwise it follows the
    /// full range when series or data change.
    window_moved: bool,
    step: Option<Step>,
    pub scale: f32,
    rows: Vec<usize>,
    /// Projections inside the window, counted with `rows`.
    projections: usize,
    rows_key: Option<RowsKey>,
    /// First row (in table order) of the slice shown in the scene.
    first: usize,
    camera: Option<Camera>,
    /// What the automatic camera was framed for; framing runs again only when it changes.
    framed: Option<FrameKey>,
    /// Time under the pointer on the histogram, shown as a wall in the scene.
    cursor: Option<i64>,
    /// Width of the details panel chosen by dragging its edge; 0 is the default.
    details_width: f32,
    /// The user has moved the camera; otherwise it keeps framing the scene
    /// as rows, series and the window change.
    camera_moved: bool,
    preset: Option<Preset>,
}

type ModelResult = Result<Arc<DepthModel>, String>;

/// Results with more rows are analysed on a worker thread.
const SYNC_ROWS: usize = 20_000;

struct Pending {
    key: usize,
    series: Vec<usize>,
    receiver: std::sync::mpsc::Receiver<Analysis>,
}

/// Candidates, resolved series and model of one data version: one pass of
/// parsing for the candidates (skipped when cached) and one for the model.
struct Analysis {
    key: usize,
    candidates: Arc<Vec<TimeColumn>>,
    series: Vec<usize>,
    model: ModelResult,
    histogram: Vec<u32>,
}

fn analyse(data: &crate::model::QueryResult, key: usize, mut series: Vec<usize>, cached: Option<Arc<Vec<TimeColumn>>>) -> Analysis {
    let candidates = cached.unwrap_or_else(|| Arc::new(depth::time_columns(data)));
    series.retain(|column| candidates.iter().any(|candidate| candidate.index == *column));
    if series.is_empty()
        && let Some(first) = candidates.first()
    {
        series.push(first.index);
    }
    let model = DepthModel::build(data, &series, &candidates).map(Arc::new);
    let histogram = model.as_ref().map(|m| histogram(m)).unwrap_or_default();
    Analysis {
        key,
        candidates,
        series,
        model,
        histogram,
    }
}

#[derive(Clone, Copy, PartialEq)]
struct FrameKey {
    first: usize,
    last: usize,
    series: usize,
    window: (i64, i64),
    scale: f32,
    card_width: f32,
    rect: Rect,
    preset: Preset,
}

#[derive(Clone, PartialEq)]
struct RowsKey {
    data: usize,
    series: Vec<usize>,
    window: (i64, i64),
    order: (usize, usize, Option<(usize, bool)>),
}

impl DepthState {
    pub fn carry_from(&mut self, old: &mut DepthState) {
        self.open = old.open;
        self.series = std::mem::take(&mut old.series);
        self.window = old.window;
        self.window_moved = old.window_moved;
        self.step = old.step;
        self.scale = old.scale;
        self.camera = old.camera;
        self.camera_moved = old.camera_moved;
        self.details_width = old.details_width;
        self.preset = old.preset;
    }
}

impl ResultSet {
    fn data_key(&self) -> usize {
        Arc::as_ptr(&self.data) as usize
    }

    /// Whether Depth View applies, from column types or a small sample; the
    /// full analysis waits until the view is opened.
    pub(in crate::ui) fn depth_may_apply(&mut self) -> bool {
        let key = self.data_key();
        match self.depth.may_apply {
            Some((cached, answer)) if cached == key => answer,
            _ => {
                let answer = depth::may_have_time_columns(&self.data);
                self.depth.may_apply = Some((key, answer));
                answer
            }
        }
    }

    pub(in crate::ui) fn toggle_depth_view(&mut self) {
        self.commit_find_buffer();
        self.depth.open = !self.depth.open;
        if self.depth.open {
            self.record_view = false;
            self.aggregate_view = false;
            if self.depth.scale == 0.0 {
                self.depth.scale = 1.0;
            }
        }
        self.pending_grid_focus = true;
    }

    fn apply_analysis(&mut self, done: Analysis) {
        self.depth.candidates = Some((done.key, done.candidates));
        self.depth.series = done.series.clone();
        if let Ok(model) = &done.model {
            self.depth.histogram = done.histogram;
            // Keep a user's window when it still overlaps the new data.
            let valid = self
                .depth
                .window
                .is_some_and(|(start, end)| end > model.min && start <= model.max && end > start);
            if !valid || !self.depth.window_moved {
                self.depth.window = Some(full_window(model));
                self.depth.window_moved = false;
            }
            let (start, end) = self.depth.window.unwrap();
            self.depth.step.get_or_insert(Step::for_span(end - start));
        }
        self.depth.model = Some((done.key, done.series, done.model));
        self.depth.rows_key = None;
        self.depth.framed = None;
    }

    /// Candidates and model for the current data and series, or `None` while
    /// a worker parses a large result.
    fn depth_analysis(&mut self, ctx: &egui::Context) -> Option<(Arc<Vec<TimeColumn>>, ModelResult)> {
        let key = self.data_key();
        if let Some(pending) = &self.depth.pending {
            match pending.receiver.try_recv() {
                Ok(done) => {
                    self.depth.pending = None;
                    if done.key == key {
                        self.apply_analysis(done);
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.depth.pending = None,
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        let cached = self
            .depth
            .candidates
            .as_ref()
            .filter(|(cached, _)| *cached == key)
            .map(|(_, candidates)| Arc::clone(candidates));
        if let (Some(candidates), Some((cached, series, model))) = (&cached, &self.depth.model)
            && *cached == key
            && *series == self.depth.series
        {
            return Some((Arc::clone(candidates), model.clone()));
        }
        if self
            .depth
            .pending
            .as_ref()
            .is_some_and(|p| p.key == key && p.series == self.depth.series)
        {
            return None;
        }
        let series = self.depth.series.clone();
        if self.data.rows.len() <= SYNC_ROWS {
            let done = analyse(&self.data, key, series, cached);
            self.apply_analysis(done);
            let candidates = self.depth.candidates.as_ref().map(|(_, c)| Arc::clone(c))?;
            return self.depth.model.as_ref().map(|(_, _, model)| (candidates, model.clone()));
        }
        let (sender, receiver) = std::sync::mpsc::channel();
        let data = Arc::clone(&self.data);
        let ctx = ctx.clone();
        let requested = series.clone();
        std::thread::Builder::new()
            .name("depth-view".into())
            .spawn(move || {
                let _ = sender.send(analyse(&data, key, requested, cached));
                ctx.request_repaint();
            })
            .ok()?;
        self.depth.pending = Some(Pending { key, series, receiver });
        None
    }

    /// Data rows in table order (sort and filters) with a projection in the window.
    fn depth_rows(&mut self, model: &DepthModel) -> &[usize] {
        let (start, end) = self.depth.window.unwrap_or((model.min, model.max + 1));
        // Find and local filters build a new row list on every change, so its
        // address identifies the filtered order; sorting is keyed by its spec.
        let filtered = self.filtered_rows().map_or((0, usize::MAX), |rows| (rows.as_ptr() as usize, rows.len()));
        let order = (filtered.0, filtered.1, self.sort);
        let key = RowsKey {
            data: self.data_key(),
            series: self.depth.series.clone(),
            window: (start, end),
            order,
        };
        if self.depth.rows_key.as_ref() != Some(&key) {
            // One pass: a row is kept when any projection is inside, and counted per projection.
            let mut rows = std::mem::take(&mut self.depth.rows);
            rows.clear();
            let mut projections = 0;
            for index in 0..self.row_count() {
                if let Some(GridRow::Data(row)) = self.display_row(index) {
                    let inside = (0..model.series.len())
                        .filter(|&s| model.time(row, s).is_some_and(|t| t >= start && t < end))
                        .count();
                    if inside > 0 {
                        rows.push(row);
                        projections += inside;
                    }
                }
            }
            self.depth.projections = projections;
            self.depth.rows = rows;
            self.depth.rows_key = Some(key);
        }
        &self.depth.rows
    }
}

fn full_window(model: &DepthModel) -> (i64, i64) {
    // `end` is exclusive; include the latest value.
    (model.min, model.max.saturating_add(1).max(model.min + 1))
}

/// Primary series over the full fetched range, computed once per model.
fn histogram(model: &DepthModel) -> Vec<u32> {
    let mut bins = vec![0u32; HISTOGRAM_BINS];
    let span = (model.max - model.min).max(1) as f64;
    for row in 0..model.rows() {
        if let Some(time) = model.time(row, 0) {
            let bin = ((time - model.min) as f64 / span * (HISTOGRAM_BINS - 1) as f64) as usize;
            bins[bin.min(HISTOGRAM_BINS - 1)] += 1;
        }
    }
    bins
}

/// Depth of `time` in `[0, 1]`: 0 at the window end (near), 1 at its start (far).
fn depth_of(time: i64, (start, end): (i64, i64)) -> f32 {
    ((end - time) as f64 / (end - start).max(1) as f64).clamp(0.0, 1.0) as f32
}

/// Cell text cut to `limit` characters before anything is laid out, so a
/// large value is never copied or measured whole. Line breaks show as ↵.
fn shown_text(rs: &ResultSet, row: usize, column: usize, limit: usize) -> String {
    let bounded = |text: &str| {
        let mut out: String = text.chars().take(limit + 1).map(|c| if c == '\n' { '↵' } else { c }).collect();
        if out.chars().count() > limit {
            let cut = out.char_indices().nth(limit.saturating_sub(1)).map_or(out.len(), |(i, _)| i);
            out.truncate(cut);
            out.push('…');
        }
        out
    };
    match rs.cell(GridRow::Data(row), column) {
        Shown::Value(Value::Null) | Shown::Edit(CellEdit::Null) => "<null>".into(),
        Shown::Edit(CellEdit::Default) => "<default>".into(),
        Shown::Value(Value::Text(text)) | Shown::Edit(CellEdit::Text(text)) => bounded(text),
        Shown::Value(value) => bounded(&value.to_string()),
    }
}

/// A card or a tab of one row in the scene: a slab whose front face is `front`.
struct Drawable {
    row: usize,
    series: usize,
    /// Distance along the view direction, for painting far to near.
    depth: f32,
    /// Projected front face, for picking.
    quad: [egui::Pos2; 4],
    /// Front face in world space: top left, top right, bottom right, bottom left.
    front: [Vec3; 4],
}

pub(super) fn show(ui: &mut Ui, rs: &mut ResultSet) {
    let Some((columns, model)) = rs.depth_analysis(ui.ctx()) else {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(format!("Reading times of {} rows…", rs.data.rows.len()));
        });
        return;
    };
    if columns.is_empty() {
        ui.label(
            RichText::new("This result has no date or time columns.").color(theme::TEXT_MUTED),
        );
        return;
    }
    controls(ui, rs, &columns);
    let model = match model {
        Ok(model) => model,
        Err(reason) => {
            ui.label(RichText::new(reason).color(theme::RED));
            return;
        }
    };
    window_bar(ui, rs, &model);
    let rows_total = rs.data.rows.len();
    let visible = rs.depth_rows(&model).len();
    let projections = rs.depth.projections;
    status_line(ui, rs, &model, visible, rows_total, projections);
    camera_bar(ui, rs, visible);
    let selected = match rs.valid_selection() {
        Some((GridRow::Data(row), _)) => Some(row),
        _ => None,
    };
    let available = ui.available_rect_before_wrap();
    let details = selected.is_some() && available.width() > MIN_DETAILS + MIN_CANVAS;
    let width = if rs.depth.details_width > 0.0 { rs.depth.details_width } else { DETAILS_WIDTH }
        .clamp(MIN_DETAILS, (available.width() - MIN_CANVAS).max(MIN_DETAILS));
    let canvas_rect = if details {
        Rect::from_min_max(available.min, egui::pos2(available.max.x - width - 6.0, available.max.y))
    } else {
        available
    };
    let clicked = canvas(ui, rs, &model, canvas_rect, selected);
    if let Some(row) = clicked {
        let column = rs.valid_selection().map_or(0, |(_, column)| column);
        rs.select_cell((GridRow::Data(row), column), false);
    }
    if details && let Some(row) = selected {
        // The edge between the scene and the panel resizes the panel.
        let edge = Rect::from_min_max(
            egui::pos2(canvas_rect.max.x, available.min.y),
            egui::pos2(canvas_rect.max.x + 6.0, available.max.y),
        );
        let handle = ui.interact(edge, ui.id().with("depth_details_edge"), Sense::drag());
        let handle = handle.on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
        if handle.dragged() {
            rs.depth.details_width = (width - handle.drag_delta().x)
                .clamp(MIN_DETAILS, (available.width() - MIN_CANVAS).max(MIN_DETAILS));
        }
        let line = if handle.hovered() || handle.dragged() { theme::ACCENT } else { theme::GRID_LINE };
        ui.painter().line_segment([edge.center_top(), edge.center_bottom()], Stroke::new(1.0, line));
        handle.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Other, true, "Resize details"));
        // The panel has the panel background, apart from the scene.
        let panel = Rect::from_min_max(egui::pos2(edge.max.x, available.min.y), available.max);
        ui.painter().rect_filled(panel, 0.0, theme::BG_PANEL);
        let rect = panel.shrink2(egui::vec2(10.0, 8.0));
        ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
            ui.set_max_width(rect.width());
            ui.visuals_mut().faint_bg_color = theme::STRIPE_ON_PANEL;
            details_panel(ui, rs, &model, row);
        });
    }
    ui.allocate_rect(available, Sense::hover());
}

fn controls(ui: &mut Ui, rs: &mut ResultSet, columns: &[TimeColumn]) {
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("Depth View").strong());
        let mut remove = None;
        for slot in 0..rs.depth.series.len() {
            ui.label(RichText::new(SERIES_NAMES[slot]).color(SERIES_COLORS[slot]).strong());
            let current = rs.depth.series[slot];
            let mut chosen = current;
            egui::ComboBox::from_id_salt(("depth_series", slot))
                .selected_text(rs.data.columns[current].name.as_str())
                .width(150.0)
                .show_ui(ui, |ui| {
                    for column in columns {
                        let taken = rs.depth.series.contains(&column.index) && column.index != current;
                        let label = format!(
                            "{}{}",
                            rs.data.columns[column.index].name,
                            if column.date_only { "  (date)" } else { "" }
                        );
                        if ui
                            .add_enabled(!taken, egui::Button::selectable(column.index == current, label))
                            .clicked()
                        {
                            chosen = column.index;
                        }
                    }
                });
            if chosen != current {
                rs.depth.series[slot] = chosen;
            }
            if slot > 0 && theme::tool_button(ui, "×", "Remove this time column").clicked() {
                remove = Some(slot);
            }
        }
        if let Some(slot) = remove {
            rs.depth.series.remove(slot);
        }
        let free = columns
            .iter()
            .find(|column| !rs.depth.series.contains(&column.index));
        if rs.depth.series.len() < MAX_SERIES
            && let Some(free) = free
            && ui
                .button("+ Time Column")
                .on_hover_text("Add another projection of each row, connected on the same time axis")
                .clicked()
        {
            rs.depth.series.push(free.index);
        }
        ui.separator();
        ui.label("Depth");
        ui.spacing_mut().slider_width = 110.0;
        ui.add(egui::Slider::new(&mut rs.depth.scale, 0.0..=1.0).show_value(false))
            .on_hover_text("Stretch of the scene; 0 shows the flat table. Does not filter data.");
    });
}

fn window_bar(ui: &mut Ui, rs: &mut ResultSet, model: &DepthModel) {
    let (mut start, mut end) = rs.depth.window.unwrap_or_else(|| full_window(model));
    let mut step = rs.depth.step.unwrap_or(Step::Minute);
    let date_only = false;
    ui.horizontal_wrapped(|ui| {
        let span = (end - start).max(1);
        theme::flat_widgets(ui);
        if theme::tool_button(ui, "◀", "Move the window back by one step").clicked() {
            start -= step.micros();
            end -= step.micros();
        }
        ui.label(RichText::new(depth::format_time(start, model.domain, date_only)).monospace());
        ui.label("→");
        ui.label(RichText::new(depth::format_time(end, model.domain, date_only)).monospace());
        ui.label(RichText::new("(end excluded)").small().color(theme::TEXT_MUTED));
        if theme::tool_button(ui, "▶", "Move the window forward by one step").clicked() {
            start += step.micros();
            end += step.micros();
        }
        egui::ComboBox::from_id_salt("depth_step")
            .selected_text(format!("Step {}", step.label()))
            .width(90.0)
            .show_ui(ui, |ui| {
                for option in Step::ALL {
                    ui.selectable_value(&mut step, option, option.label());
                }
            });
        if ui.button("Narrow").on_hover_text("Halve the window around its center").clicked() {
            let center = start + span / 2;
            start = center - span / 4;
            end = (center + span / 4).max(start + 1);
        }
        if ui.button("Widen").on_hover_text("Double the window around its center").clicked() {
            start -= span / 2;
            end += span / 2;
        }
        if ui.button("Fit").on_hover_text("Show the whole range of the fetched rows").clicked() {
            (start, end) = full_window(model);
        }
    });
    // Histogram of the fetched rows with the window drawn over it; drag to move it.
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 28.0), Sense::click_and_drag());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Slider, true, "Time histogram"));
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 3.0, theme::BG_PANEL);
    let peak = rs.depth.histogram.iter().copied().max().unwrap_or(1).max(1) as f32;
    let bin_width = rect.width() / HISTOGRAM_BINS as f32;
    for (bin, count) in rs.depth.histogram.iter().enumerate() {
        if *count == 0 {
            continue;
        }
        let height = (*count as f32 / peak) * (rect.height() - 4.0);
        let x = rect.left() + bin as f32 * bin_width;
        painter.rect_filled(
            Rect::from_min_max(egui::pos2(x, rect.bottom() - height), egui::pos2(x + bin_width.max(1.0), rect.bottom())),
            0.0,
            theme::TEXT_MUTED.gamma_multiply(0.5),
        );
    }
    let full = (model.max - model.min).max(1) as f32;
    let to_x = |t: i64| rect.left() + ((t - model.min) as f32 / full) * rect.width();
    let window_rect = Rect::from_min_max(
        egui::pos2(to_x(start).max(rect.left()), rect.top()),
        egui::pos2(to_x(end).min(rect.right()).max(to_x(start) + 2.0), rect.bottom()),
    );
    painter.rect_filled(window_rect, 2.0, theme::ACCENT.gamma_multiply(0.25));
    painter.rect_stroke(window_rect, 2.0, Stroke::new(1.0, theme::ACCENT), egui::StrokeKind::Inside);
    // The time under the pointer moves a wall through the 3D scene.
    rs.depth.cursor = response.hover_pos().filter(|_| !response.dragged()).map(|pointer| {
        model.min + ((pointer.x - rect.left()) / rect.width() * full) as i64
    });
    if let Some(time) = rs.depth.cursor {
        let x = to_x(time);
        painter.line_segment([egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())], Stroke::new(1.5, theme::SEARCH_MARKER));
    }
    if response.dragged() {
        let shift = (response.drag_delta().x / rect.width() * full) as i64;
        start += shift;
        end += shift;
    } else if response.clicked()
        && let Some(pointer) = response.interact_pointer_pos()
    {
        let center = model.min + ((pointer.x - rect.left()) / rect.width() * full) as i64;
        let half = (end - start) / 2;
        start = center - half;
        end = start + (end - start);
    }
    let hint = match rs.depth.cursor {
        Some(time) if time >= start && time < end => format!(
            "{} · drag or click to move the window",
            depth::format_time(time, model.domain, date_only)
        ),
        Some(time) => format!(
            "{} · outside the window, not in the scene",
            depth::format_time(time, model.domain, date_only)
        ),
        None => "Fetched rows over time. Drag or click to move the window.".into(),
    };
    response.on_hover_text_at_pointer(hint);
    if end <= start {
        end = start + 1;
    }
    let fitted = (start, end) == full_window(model);
    if rs.depth.window != Some((start, end)) {
        rs.depth.window_moved = !fitted;
    }
    rs.depth.window = Some((start, end));
    rs.depth.step = Some(step);
    // Arrow keys move the window when the view has focus.
    if ui.memory(|m| m.focused().is_none()) {
        let shift = ui.input(|i| {
            if i.key_pressed(egui::Key::ArrowLeft) {
                -step.micros()
            } else if i.key_pressed(egui::Key::ArrowRight) {
                step.micros()
            } else {
                0
            }
        });
        if shift != 0 {
            rs.depth.window = Some((start + shift, end + shift));
            rs.depth.window_moved = true;
        }
    }
}

fn status_line(
    ui: &mut Ui,
    rs: &ResultSet,
    model: &DepthModel,
    visible: usize,
    total: usize,
    projections: usize,
) {
    // Facts about the window first, then the legend: one sample per series.
    ui.horizontal_wrapped(|ui| {
        let scope = if rs.data.truncated {
            "fetched page; more rows exist in the database"
        } else {
            "all rows of the result"
        };
        ui.label(
            RichText::new(format!(
                "{visible} of {} in the window · {projections} projections · {scope} · {}",
                rows(total),
                model.domain.label()
            ))
            .small()
            .color(theme::TEXT_MUTED),
        );
        if model.mixed_precision {
            ui.label(
                RichText::new("Dates without a time of day are placed at midnight.")
                    .small()
                    .color(theme::RED),
            );
        }
    });
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for (slot, column) in model.series.iter().enumerate() {
            series_sample(ui, slot);
            ui.label(
                RichText::new(format!("{} {}", SERIES_NAMES[slot], rs.data.columns[*column].name))
                    .small()
                    .color(SERIES_COLORS[slot]),
            );
            let missing = model.missing.get(slot).copied().unwrap_or(0);
            if missing > 0 {
                ui.label(
                    RichText::new(format!("{} without a value", rows(missing)))
                        .small()
                        .color(theme::TEXT_MUTED),
                );
            }
            ui.add_space(10.0);
        }
        ui.label(
            RichText::new("right and smaller: earlier · left: later")
                .small()
                .color(theme::TEXT_MUTED),
        );
    });
}

fn rows(count: usize) -> String {
    if count == 1 {
        "1 row".into()
    } else {
        format!("{count} rows")
    }
}

/// A short line drawn the way the series is drawn in the scene.
fn series_sample(ui: &mut Ui, slot: usize) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(22.0, 12.0), Sense::hover());
    let color = SERIES_COLORS[slot];
    let stroke = egui::Stroke::new(2.0, color);
    let (y, left, right) = (rect.center().y, rect.left() + 2.0, rect.right() - 2.0);
    let points = [egui::pos2(left, y), egui::pos2(right, y)];
    let painter = ui.painter();
    match slot {
        0 => {
            painter.line_segment(points, stroke);
        }
        1 => {
            painter.extend(egui::Shape::dashed_line(&points, stroke, 5.0, 3.0));
        }
        _ => {
            painter.extend(egui::Shape::dotted_line(&points, color, 4.0, 1.2));
        }
    }
}

fn camera_bar(ui: &mut Ui, rs: &mut ResultSet, visible: usize) {
    ui.horizontal_wrapped(|ui| {
        theme::flat_widgets(ui);
        for (preset, label) in [(Preset::Front, "Front"), (Preset::Oblique, "Oblique"), (Preset::Top, "Top")] {
            if ui.selectable_label(rs.depth.preset == Some(preset), label).clicked() {
                rs.depth.preset = Some(preset);
                rs.depth.camera = None;
                rs.depth.camera_moved = false;
            }
        }
        if ui.button("Fit View").on_hover_text("Frame the rows of the scene").clicked() {
            rs.depth.camera = None;
            rs.depth.camera_moved = false;
        }
        ui.separator();
        let last = (rs.depth.first + ROWS_IN_SCENE).min(visible);
        if theme::tool_button(ui, "▲", "Previous rows").clicked() {
            rs.depth.first = rs.depth.first.saturating_sub(ROWS_IN_SCENE / 2);
        }
        ui.label(
            RichText::new(format!(
                "Rows {}–{last} of {visible} in the scene",
                (rs.depth.first + 1).min(last)
            ))
            .small(),
        );
        if theme::tool_button(ui, "▼", "Next rows").clicked() && last < visible {
            rs.depth.first += ROWS_IN_SCENE / 2;
        }
        ui.label(
            RichText::new("Drag: rotate · Shift or right drag: pan · Wheel: zoom")
                .small()
                .color(theme::TEXT_MUTED),
        );
    });
    rs.depth.first = rs.depth.first.min(visible.saturating_sub(1));
}

fn canvas(
    ui: &mut Ui,
    rs: &mut ResultSet,
    model: &DepthModel,
    rect: Rect,
    selected: Option<usize>,
) -> Option<usize> {
    let response = ui.interact(rect, ui.id().with("depth_canvas"), Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, theme::BG_EDITOR);
    let window = rs.depth.window.unwrap_or_else(|| full_window(model));
    let first = rs.depth.first.min(rs.depth.rows.len());
    let last = (first + ROWS_IN_SCENE).min(rs.depth.rows.len());
    let widths: Vec<f32> = rs.normal_widths().iter().map(|w| w * WIDTH_SCALE).collect();
    // Wider cards would only push the text out of reach; the details panel
    // shows every column.
    let card_width = (widths.iter().sum::<f32>() + 40.0).clamp(320.0, MAX_CARD_WIDTH);
    let depth_world = DEPTH_WORLD * rs.depth.scale;
    // World geometry first: it does not depend on the camera, and framing
    // measures exactly what is drawn.
    let mut cards: Vec<(usize, usize, [Vec3; 4])> = Vec::with_capacity((last - first) * model.series.len());
    for position in first..last {
        let row = rs.depth.rows[position];
        let y = (position - first) as f32 * ROW_PITCH + TAB_HEIGHT;
        for series in 0..model.series.len() {
            let Some(time) = model.time(row, series).filter(|t| *t >= window.0 && *t < window.1) else {
                continue;
            };
            let z = depth_of(time, window) * depth_world;
            // The primary series carries the whole row; further columns are
            // tabs above it, side by side, at their own depth.
            let (x0, x1, y0, y1) = if series == 0 {
                (0.0, card_width, y, y + CARD_HEIGHT)
            } else {
                let x0 = (series - 1) as f32 * (GHOST_WIDTH + 6.0);
                (x0, x0 + GHOST_WIDTH, y - TAB_HEIGHT - 1.0, y - 1.0)
            };
            cards.push((
                row,
                series,
                [Vec3::new(x0, y0, z), Vec3::new(x1, y0, z), Vec3::new(x1, y1, z), Vec3::new(x0, y1, z)],
            ));
        }
    }
    let preset = rs.depth.preset.unwrap_or(Preset::Oblique);
    let frame = FrameKey {
        first,
        last,
        series: model.series.len(),
        window,
        scale: rs.depth.scale,
        card_width,
        rect,
        preset,
    };
    if rs.depth.camera.is_none() || (!rs.depth.camera_moved && rs.depth.framed != Some(frame)) {
        let points: Vec<Vec3> = cards.iter().flat_map(|(_, _, corners)| *corners).collect();
        rs.depth.camera = Some(Camera::framing(preset, &points, rect));
        rs.depth.framed = Some(frame);
    }
    let camera = rs.depth.camera.as_mut().expect("framed above");
    // Camera interaction: never runs SQL, only changes the view.
    let shift = ui.input(|i| i.modifiers.shift);
    if response.dragged_by(egui::PointerButton::Secondary)
        || response.dragged_by(egui::PointerButton::Middle)
        || (response.dragged_by(egui::PointerButton::Primary) && shift)
    {
        let delta = response.drag_delta();
        camera.pan(delta.x, delta.y, rect);
        rs.depth.camera_moved = true;
    } else if response.dragged_by(egui::PointerButton::Primary) {
        let delta = response.drag_delta();
        camera.orbit(delta.x, delta.y);
        rs.depth.camera_moved = true;
    }
    if response.hovered() {
        let wheel = ui.input(|i| i.smooth_scroll_delta.y + i.zoom_delta().ln() * 400.0);
        if wheel != 0.0 {
            camera.zoom(wheel);
            rs.depth.camera_moved = true;
        }
    }
    let view = camera.view();
    let mut drawables = Vec::with_capacity(cards.len());
    let mut anchors: Vec<(usize, usize, Vec3)> = Vec::with_capacity(cards.len());
    for (row, series, corners) in &cards {
        let (row, series) = (*row, *series);
        let [top_left, _, _, bottom_left] = *corners;
        let anchor = if series == 0 { top_left } else { bottom_left };
        anchors.push((row, series, Vec3::new(anchor.x + 4.0, anchor.y, anchor.z)));
        let projected: Option<Vec<_>> = corners.iter().map(|c| view.project(*c, rect)).collect();
        let Some(projected) = projected else {
            continue;
        };
        drawables.push(Drawable {
            row,
            series,
            depth: projected.iter().map(|p| p.depth).sum::<f32>() / 4.0,
            quad: [projected[0].pos, projected[1].pos, projected[2].pos, projected[3].pos],
            front: *corners,
        });
    }
    drawables.sort_by(|a, b| b.depth.total_cmp(&a.depth));
    let pointer = response.hover_pos();
    let hovered = pointer.and_then(|p| {
        drawables
            .iter()
            .rev()
            .find(|d| quad_contains(&d.quad, p))
            .map(|d| d.row)
    });
    // Connectors: same row, consecutive series in the user's order, in 3D.
    let mut labels = Vec::new();
    for position in first..last {
        let row = rs.depth.rows[position];
        let focus = Some(row) == selected || Some(row) == hovered;
        for series in 0..model.series.len().saturating_sub(1) {
            let a = anchors.iter().find(|(r, s, _)| *r == row && *s == series).map(|a| a.2);
            let b = anchors.iter().find(|(r, s, _)| *r == row && *s == series + 1).map(|a| a.2);
            let stroke = Stroke::new(
                if focus { 2.0 } else { 1.0 },
                SERIES_COLORS[series + 1].gamma_multiply(if focus { 1.0 } else { 0.55 }),
            );
            match (a, b) {
                (Some(a), Some(b)) => {
                    if let (Some(pa), Some(pb)) = (view.project(a, rect), view.project(b, rect)) {
                        painter.line_segment([pa.pos, pb.pos], stroke);
                        if focus && let Some(delta) = model.delta(row, series) {
                            labels.push((pa.pos, series, delta));
                        }
                    }
                }
                // The other end is outside the window: a continuation marker,
                // never a clipped end drawn as if it were an event.
                (Some(end), None) | (None, Some(end)) if model.delta(row, series).is_some() => {
                    if let Some(p) = view.project(end, rect) {
                        let tip = p.pos + Vec2::new(0.0, -10.0);
                        painter.add(egui::Shape::dashed_line(&[p.pos, tip], stroke, 3.0, 2.0));
                        painter.text(tip, Align2::CENTER_BOTTOM, "…", FontId::monospace(11.0), stroke.color);
                    }
                }
                _ => {}
            }
        }
    }
    // Time wall: the histogram cursor as a translucent plane at its depth.
    // Cards behind it are painted first, so the wall tints them and not the nearer ones.
    let wall = rs.depth.cursor.filter(|t| *t >= window.0 && *t < window.1).map(|time| {
        let z = depth_of(time, window) * depth_world;
        let height = (last - first).max(1) as f32 * ROW_PITCH + TAB_HEIGHT;
        let corners = [
            Vec3::new(-40.0, -14.0, z),
            Vec3::new(card_width + 40.0, -14.0, z),
            Vec3::new(card_width + 40.0, height + 10.0, z),
            Vec3::new(-40.0, height + 10.0, z),
        ];
        (time, z, corners)
    });
    let half_step = rs.depth.step.unwrap_or(Step::Minute).micros() / 2;
    let near_wall = |drawable: &Drawable| {
        wall.is_some_and(|(time, ..)| {
            model
                .time(drawable.row, drawable.series)
                .is_some_and(|t| (t - time).abs() <= half_step)
        })
    };
    let wall_faces = wall.is_some_and(|(_, _, corners)| view.faces_camera(corners[0], Vec3::new(0.0, 0.0, -1.0)));
    let behind = |drawable: &Drawable| match wall {
        Some((_, z, _)) => (drawable.front[0].z > z) == wall_faces,
        None => true,
    };
    let draw = |drawable: &Drawable| {
        draw_card(&painter, &view, rect, rs, model, drawable, &widths, card_width, Some(drawable.row) == selected, Some(drawable.row) == hovered);
        if near_wall(drawable) {
            let mut outline = drawable.quad.to_vec();
            outline.push(drawable.quad[0]);
            painter.add(egui::Shape::line(outline, Stroke::new(2.0, theme::SEARCH_MARKER)));
        }
    };
    for drawable in drawables.iter().filter(|d| behind(d)) {
        draw(drawable);
    }
    if let Some((time, _, corners)) = wall
        && let Some(points) = corners.iter().map(|c| view.project(*c, rect).map(|p| p.pos)).collect::<Option<Vec<_>>>()
    {
        painter.add(egui::Shape::convex_polygon(
            points.clone(),
            theme::SEARCH_MARKER.gamma_multiply(0.12),
            Stroke::new(1.5, theme::SEARCH_MARKER.gamma_multiply(0.8)),
        ));
        let close = drawables.iter().filter(|d| near_wall(d)).count();
        let label = format!(
            "{} · {close} within ±{}",
            depth::format_time(time, model.domain, false),
            depth::format_delta(half_step).trim_start_matches('+')
        );
        let galley = painter.layout_no_wrap(label, FontId::monospace(12.0), theme::TEXT);
        let top = points.iter().copied().fold(points[0], |a, b| if b.y < a.y { b } else { a });
        let at = egui::pos2(
            (top.x - galley.size().x * 0.5).clamp(rect.left() + 4.0, rect.right() - galley.size().x - 4.0),
            (top.y - galley.size().y - 6.0).clamp(rect.top() + 4.0, rect.bottom() - galley.size().y - 4.0),
        );
        painter.rect_filled(Rect::from_min_size(at, galley.size()).expand(3.0), 3.0, theme::BG_PANEL);
        painter.rect_stroke(Rect::from_min_size(at, galley.size()).expand(3.0), 3.0, Stroke::new(1.0, theme::SEARCH_MARKER), egui::StrokeKind::Outside);
        painter.galley(at, galley, theme::TEXT);
    }
    if wall.is_some() {
        for drawable in drawables.iter().filter(|d| !behind(d)) {
            draw(drawable);
        }
    }
    // Delta labels stay on top, one line per pair, next to the primary card.
    for (at, series, delta) in labels {
        let text = format!("{}−{} {}", SERIES_NAMES[series + 1], SERIES_NAMES[series], depth::format_delta(delta));
        let galley = painter.layout_no_wrap(text, FontId::monospace(11.0), theme::TEXT);
        let pos = at + Vec2::new(-galley.size().x - 10.0, -TAB_HEIGHT + series as f32 * 13.0);
        painter.rect_filled(Rect::from_min_size(pos, galley.size()).expand(2.0), 2.0, theme::BG_EDITOR.gamma_multiply(0.9));
        painter.galley(pos, galley, theme::TEXT);
    }
    if drawables.is_empty() {
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            "No projections in view. Move or widen the window, or press Fit.",
            FontId::proportional(13.0),
            theme::TEXT_MUTED,
        );
    }
    if response.clicked() { hovered } else { None }
}

/// Slab thickness in world units, towards the viewer.
const SLAB: f32 = 12.0;

#[allow(clippy::too_many_arguments)]
fn draw_card(
    painter: &egui::Painter,
    view: &View,
    rect: Rect,
    rs: &ResultSet,
    model: &DepthModel,
    drawable: &Drawable,
    widths: &[f32],
    card_width: f32,
    selected: bool,
    hovered: bool,
) {
    let primary = drawable.series == 0;
    let color = SERIES_COLORS[drawable.series];
    let fill = if selected {
        theme::BG_SELECTED
    } else if primary {
        theme::BG_PANEL
    } else {
        theme::BG_PANEL.gamma_multiply(0.85)
    };
    let [tl, tr, br, bl] = drawable.front;
    // The slab extends from the front face away from the viewer.
    let back = |p: Vec3| Vec3::new(p.x, p.y, p.z + SLAB);
    let faces: [([Vec3; 4], Vec3, f32); 5] = [
        ([back(tl), back(tr), tr, tl], Vec3::new(0.0, -1.0, 0.0), 0.75),
        ([bl, br, back(br), back(bl)], Vec3::new(0.0, 1.0, 0.0), 0.45),
        ([back(tl), tl, bl, back(bl)], Vec3::new(-1.0, 0.0, 0.0), 0.6),
        ([tr, back(tr), back(br), br], Vec3::new(1.0, 0.0, 0.0), 0.6),
        ([back(tr), back(tl), back(bl), back(br)], Vec3::new(0.0, 0.0, 1.0), 0.4),
    ];
    for (corners, normal, shade) in faces {
        if !view.faces_camera(corners[0], normal) {
            continue;
        }
        let points: Option<Vec<Pos2>> = corners.iter().map(|c| view.project(*c, rect).map(|p| p.pos)).collect();
        if let Some(points) = points {
            let side = theme::BG_PANEL.lerp_to_gamma(color, 0.35 * shade + 0.1);
            painter.add(egui::Shape::convex_polygon(points, side, Stroke::new(0.5, color.gamma_multiply(0.5))));
        }
    }
    let front_visible = view.faces_camera(tl, Vec3::new(0.0, 0.0, -1.0));
    let stroke = Stroke::new(if selected || hovered { 2.0 } else { 1.0 }, color);
    painter.add(egui::Shape::convex_polygon(drawable.quad.to_vec(), fill, Stroke::NONE));
    // Distinct outlines, so the series are not told apart by color alone.
    let mut outline = drawable.quad.to_vec();
    outline.push(drawable.quad[0]);
    match drawable.series {
        0 => {
            painter.add(egui::Shape::line(outline, stroke));
        }
        series => {
            let (dash, gap) = if series == 1 { (6.0, 3.0) } else { (2.0, 2.0) };
            painter.extend(egui::Shape::dashed_line(&outline, stroke, dash, gap));
        }
    }
    // Series tag on the left edge of the card, in world space.
    let tag = [tl, Vec3::new(tl.x + 22.0, tl.y, tl.z), Vec3::new(bl.x + 22.0, bl.y, bl.z), bl];
    if let Some(points) = tag.iter().map(|c| view.project(*c, rect).map(|p| p.pos)).collect::<Option<Vec<_>>>() {
        painter.add(egui::Shape::convex_polygon(points, color.gamma_multiply(0.85), Stroke::NONE));
    }
    if !front_visible {
        return;
    }
    let middle = (tl.y + bl.y) * 0.5;
    let size = if primary { TEXT_WORLD } else { TEXT_WORLD * 0.85 };
    // All text of a card goes into one mesh, painted right after the card.
    let mut text = TextMesh::new(painter, view, rect, size);
    if !primary {
        let label = model
            .time(drawable.row, drawable.series)
            .map(|t| depth::format_time(t, depth::Domain::Wall, false))
            .and_then(|t| t.rsplit(' ').next().map(str::to_owned))
            .unwrap_or_default();
        text.add(Vec3::new(tl.x + 28.0, middle, tl.z), GHOST_WIDTH - 32.0, label);
    } else {
        let mut x = 28.0;
        for (column, width) in widths.iter().enumerate() {
            let room = (width - COLUMN_GAP * 0.5).min(card_width - x - 8.0);
            if room < size {
                break;
            }
            let limit = text.fitting_chars(room);
            text.add(Vec3::new(tl.x + x, middle, tl.z), room, shown_text(rs, drawable.row, column, limit));
            x += width + COLUMN_GAP;
        }
    }
    text.paint(painter);
}

/// Font sizes text is rasterized at. Discrete sizes keep the glyph atlas and
/// the layout cache warm while the camera moves; the perspective scale stays
/// continuous.
const FONT_SIZES: [f32; 9] = [8.0, 10.0, 12.0, 14.0, 17.0, 20.0, 24.0, 28.0, 32.0];
/// Advance of a monospace character relative to the font size.
const MONO_ADVANCE: f32 = 0.6;

/// Text lying on card faces: glyph quads are placed in world space and
/// projected one vertex at a time, so the text has true perspective and never
/// leaves its cell.
struct TextMesh<'a> {
    view: &'a View,
    rect: Rect,
    size: f32,
    uv_scale: Vec2,
    mesh: egui::Mesh,
    painter: &'a egui::Painter,
}

impl<'a> TextMesh<'a> {
    fn new(painter: &'a egui::Painter, view: &'a View, rect: Rect, size: f32) -> Self {
        let atlas = painter.ctx().fonts(|fonts| fonts.font_image_size());
        Self {
            view,
            rect,
            size,
            uv_scale: Vec2::new(1.0 / atlas[0] as f32, 1.0 / atlas[1] as f32),
            mesh: egui::Mesh::with_texture(egui::TextureId::default()),
            painter,
        }
    }

    /// Characters that fit a cell of `width` world units.
    fn fitting_chars(&self, width: f32) -> usize {
        ((width / (self.size * MONO_ADVANCE)).floor() as usize).max(1)
    }

    /// `origin` is the left end of the text's middle line.
    fn add(&mut self, origin: Vec3, width: f32, text: String) {
        let Some(start) = self.view.project(origin, self.rect) else {
            return;
        };
        // Rasterize near the size the text has on screen; skip it when unreadable.
        let on_screen = self.size * self.view.pixels_per_unit(start.depth, self.rect);
        if on_screen < 4.0 || text.is_empty() {
            return;
        }
        let font_size = FONT_SIZES.iter().copied().find(|s| *s >= on_screen).unwrap_or(32.0);
        let font = FontId::monospace(font_size);
        let scale = self.size / font_size;
        let mut galley = self.painter.layout_no_wrap(text, font.clone(), theme::TEXT);
        if galley.size().x * scale > width {
            // Font metrics differ slightly from the estimate; cut once more.
            let fit = self.fitting_chars(width * 0.9);
            let shortened: String = galley.text().chars().take(fit.saturating_sub(1)).collect::<String>() + "…";
            galley = self.painter.layout_no_wrap(shortened, font, theme::TEXT);
        }
        let half = galley.size().y * 0.5;
        let first = self.mesh.vertices.len();
        for placed in &galley.rows {
            let source = &placed.row.visuals.mesh;
            let offset = self.mesh.vertices.len() as u32;
            for vertex in &source.vertices {
                let local = placed.pos + vertex.pos.to_vec2();
                // Slightly in front of the face so the text is not hidden by it.
                let world = Vec3::new(origin.x + local.x * scale, origin.y + (local.y - half) * scale, origin.z - 0.5);
                let Some(point) = self.view.project(world, self.rect) else {
                    // Partly behind the camera: drop this text only.
                    self.mesh.vertices.truncate(first);
                    self.mesh.indices.retain(|i| (*i as usize) < first);
                    return;
                };
                let color = if vertex.color == Color32::PLACEHOLDER { theme::TEXT } else { vertex.color };
                self.mesh.vertices.push(egui::epaint::Vertex {
                    pos: point.pos,
                    uv: (vertex.uv.to_vec2() * self.uv_scale).to_pos2(),
                    color,
                });
            }
            self.mesh.indices.extend(source.indices.iter().map(|i| i + offset));
        }
    }

    fn paint(self, painter: &egui::Painter) {
        if !self.mesh.is_empty() {
            painter.add(egui::Shape::mesh(self.mesh));
        }
    }
}

fn details_panel(ui: &mut Ui, rs: &ResultSet, model: &DepthModel, row: usize) {
    egui::ScrollArea::vertical().id_salt("depth_details").show(ui, |ui| {
        ui.label(RichText::new(format!("Row {}", row + 1)).strong());
        ui.add_space(4.0);
        // Times and deltas of the row in their own card, apart from the columns.
        egui::Frame::new()
            .fill(theme::BG_EDITOR)
            .corner_radius(4)
            .inner_margin(8)
            .show(ui, |ui| {
            ui.set_width(ui.available_width());
        for (slot, column) in model.series.iter().enumerate() {
            let value = model
                .time(row, slot)
                .map_or_else(|| "no value".to_string(), |t| depth::format_time(t, model.domain, false));
            ui.add(
                egui::Label::new(
                    RichText::new(format!("{} {}: {value}", SERIES_NAMES[slot], rs.data.columns[*column].name))
                        .color(SERIES_COLORS[slot]),
                )
                .wrap(),
            );
            if slot + 1 < model.series.len() {
                let delta = model
                    .delta(row, slot)
                    .map_or_else(|| "not computable".to_string(), depth::format_delta);
                ui.label(
                    RichText::new(format!(
                        "    {} − {}: {delta}",
                        rs.data.columns[model.series[slot + 1]].name,
                        rs.data.columns[*column].name
                    ))
                    .monospace()
                    .small(),
                );
            }
        }
            });
        ui.add_space(8.0);
        ui.label(RichText::new("Columns").small().color(theme::TEXT_MUTED));
        // Long values wrap inside the panel; dragging its edge makes it wider.
        let names = rs
            .data
            .columns
            .iter()
            .map(|c| c.name.chars().count())
            .max()
            .unwrap_or(0)
            .min(18) as f32
            * 7.5;
        let values = (ui.available_width() - names - 20.0).max(80.0);
        egui::Grid::new("depth_record")
            .num_columns(2)
            .striped(true)
            .spacing([12.0, 6.0])
            .min_col_width(names.min(values))
            .max_col_width(values)
            .show(ui, |ui| {
                for (column, meta) in rs.data.columns.iter().enumerate() {
                    ui.add(egui::Label::new(RichText::new(&meta.name).color(theme::TEXT_MUTED)).truncate());
                    // Very long values are cut; Value Editor in the grid shows them whole.
                    // The value column fills the panel so stripes reach its edge.
                    ui.horizontal_wrapped(|ui| {
                        ui.set_min_width(values);
                        ui.add(egui::Label::new(RichText::new(shown_text(rs, row, column, DETAILS_CHARS)).monospace()).wrap());
                    });
                    ui.end_row();
                }
            });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Column, Page, QueryResult};

    fn logs() -> ResultSet {
        let text = |value: &str| Value::Text(value.into());
        let data = QueryResult {
            columns: [("user", "text"), ("occurred", "timestamp"), ("received", "timestamp")]
                .iter()
                .map(|(name, ty)| Column {
                    name: (*name).into(),
                    type_name: (*ty).into(),
                })
                .collect(),
            rows: vec![
                vec![text("Ola"), text("2026-10-15 10:00:00"), text("2026-10-15 10:00:18")],
                vec![text("Adam"), text("2026-10-15 10:05:00"), text("2026-10-15 10:04:50")],
                vec![text("Ewa"), text("2026-10-15 10:10:00"), Value::Null],
            ],
            ..Default::default()
        };
        ResultSet::new("logs".into(), "select".into(), data, Page::first(500))
    }

    fn model(rs: &mut ResultSet) -> Arc<DepthModel> {
        rs.depth_analysis(&egui::Context::default()).expect("small results are analysed at once").1.unwrap()
    }

    #[test]
    fn rows_follow_table_order_and_any_projection_window_without_copying_data() {
        let mut rs = logs();
        rs.toggle_depth_view();
        model(&mut rs);
        assert_eq!(rs.depth.series, [1], "the first time column is the default depth");
        rs.depth.series.push(2);
        let model = model(&mut rs);
        // Sorting by user, not by time, decides the vertical order.
        rs.set_sort(Some((0, true)));
        assert_eq!(rs.depth_rows(&model), &[1, 2, 0]);
        assert_eq!(rs.depth.projections, 5, "a missing value is not a projection");
        // [10:00:10, 10:05:00): row 0 only through `received`, row 1 only through
        // `received` (reversed before `occurred`), row 2 not at all.
        let ten = model.time(0, 0).unwrap();
        rs.depth.window = Some((ten + 10_000_000, ten + 300_000_000));
        assert_eq!(rs.depth_rows(&model), &[1, 0]);
        assert_eq!(rs.depth.projections, 2);
        // The cache is reused until something it depends on changes.
        let rows = rs.depth.rows.as_ptr();
        rs.depth_rows(&model);
        assert_eq!(rs.depth.rows.as_ptr(), rows);
    }

    #[test]
    fn large_results_are_analysed_off_the_ui_thread() {
        let rows = (0..SYNC_ROWS + 1)
            .map(|i| vec![Value::Text(format!("2026-10-15 10:{:02}:{:02}", (i / 60) % 60, i % 60))])
            .collect();
        let data = QueryResult {
            columns: vec![Column { name: "at".into(), type_name: "text".into() }],
            rows,
            ..Default::default()
        };
        let mut rs = ResultSet::new("big".into(), "select".into(), data, Page::first(500));
        assert!(rs.depth_may_apply(), "a sample of undeclared text times is enough for the button");
        rs.toggle_depth_view();
        let ctx = egui::Context::default();
        assert!(rs.depth_analysis(&ctx).is_none(), "the first frame does not parse the whole result");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let model = loop {
            if let Some((_, model)) = rs.depth_analysis(&ctx) {
                break model.unwrap();
            }
            assert!(std::time::Instant::now() < deadline, "the worker did not finish");
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert_eq!(model.rows(), SYNC_ROWS + 1);
        assert_eq!(rs.depth.series, [0]);
    }

    #[test]
    fn reload_keeps_configuration_but_rebuilds_times() {
        let mut old = logs();
        old.toggle_depth_view();
        old.depth.series = vec![1, 2];
        let first = model(&mut old);
        let mut fresh = logs();
        fresh.depth.carry_from(&mut old.depth);
        assert!(fresh.depth.open);
        assert_eq!(fresh.depth.series, [1, 2]);
        let rebuilt = model(&mut fresh);
        assert!(!Arc::ptr_eq(&first, &rebuilt), "a new result version gets new times");
        assert_eq!(depth_of(first.min, full_window(&first)), 1.0);
        assert!(depth_of(first.max, full_window(&first)) < 1e-6, "the latest time is nearest");
    }
}
