//! Run tool window: exports, imports and PostgreSQL client tools running in
//! the background, each with its progress, log and outcome, as an IDE shows
//! them in Run. The status bar keeps a compact indicator of running tasks.

use super::{App, ServicesNotice, theme};
use crate::backend::{LogLine, Progress, TaskEvent, TaskId, bytes_text, elapsed_text, rows_text};
use egui::{Align, Layout, RichText, Ui};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TransferKind {
    Export,
    Import,
    Dump,
    Restore,
}

impl TransferKind {
    fn verb(self) -> &'static str {
        match self {
            TransferKind::Export => "Exporting",
            TransferKind::Import => "Importing",
            TransferKind::Dump => "Dumping",
            TransferKind::Restore => "Restoring",
        }
    }
}

/// Lines kept in a transfer's log; older ones are dropped and counted.
const MAX_LOG_LINES: usize = 10_000;
/// Characters of a log line that are kept; a dump of wide rows can print
/// megabytes on one line.
const MAX_LINE_CHARS: usize = 2_000;

pub(super) struct Transfer {
    pub id: TaskId,
    pub kind: TransferKind,
    pub title: String,
    /// File or directory to reveal when the transfer is done.
    pub target: Option<PathBuf>,
    /// Console whose Output also gets the outcome.
    pub console_id: Option<u64>,
    started: Instant,
    finished: Option<Instant>,
    progress: Progress,
    log: VecDeque<LogLine>,
    /// Lines dropped from the front of `log`.
    dropped: u64,
    outcome: Option<Result<String, String>>,
    /// Whether `target` existed when the transfer finished.
    target_exists: bool,
}

impl Transfer {
    pub fn running(&self) -> bool {
        self.outcome.is_none()
    }

    fn push_log(&mut self, error: bool, text: &str) {
        for line in text.lines() {
            let text = match line.char_indices().nth(MAX_LINE_CHARS) {
                Some((end, _)) => format!("{}…", &line[..end]),
                None => line.to_string(),
            };
            if self.log.len() == MAX_LOG_LINES {
                self.log.pop_front();
                self.dropped += 1;
            }
            self.log.push_back(LogLine { error, text });
        }
    }

    fn elapsed(&self) -> std::time::Duration {
        self.finished.unwrap_or_else(Instant::now) - self.started
    }

    /// "120000 rows · 12.3 MB · 45000 rows/s", the counters that apply.
    fn counters(&self) -> String {
        let p = &self.progress;
        let mut parts = Vec::new();
        if p.items > 1 {
            parts.push(format!("{} of {}", (p.item + 1).min(p.items), p.items));
        }
        // Client tools report output lines, not rows.
        if matches!(self.kind, TransferKind::Dump | TransferKind::Restore) {
            if p.rows > 0 {
                parts.push(if p.rows == 1 { "1 line".into() } else { format!("{} lines", p.rows) });
            }
        } else {
            parts.push(rows_text(p.rows));
        }
        if p.total_bytes > 0 {
            parts.push(format!("{} of {}", bytes_text(p.bytes), bytes_text(p.total_bytes)));
        } else if p.bytes > 0 {
            parts.push(bytes_text(p.bytes));
        }
        if p.errors > 0 {
            parts.push(format!("{} skipped", rows_text(p.errors)));
        }
        let seconds = self.elapsed().as_secs_f64();
        if self.running() && p.rows > 0 && seconds > 0.5 {
            parts.push(format!("{:.0} rows/s", p.rows as f64 / seconds));
        }
        parts.push(elapsed_text(self.elapsed()));
        parts.join(" · ")
    }
}

#[derive(Default)]
pub(super) struct Transfers {
    list: Vec<Transfer>,
    selected: Option<TaskId>,
    next_id: TaskId,
}

/// What the Run window or the status bar indicator asked for.
pub(super) enum RunIntent {
    Stop(TaskId),
    Select(TaskId),
    Close(TaskId),
    Reveal(PathBuf),
    Show,
    Hide,
}

impl Transfers {
    pub fn running(&self) -> impl Iterator<Item = &Transfer> {
        self.list.iter().filter(|t| t.running())
    }

    fn get_mut(&mut self, id: TaskId) -> Option<&mut Transfer> {
        self.list.iter_mut().find(|t| t.id == id)
    }
}

impl App {
    /// Adds a transfer to the Run window, shows it and returns its task id.
    pub(super) fn begin_transfer(
        &mut self,
        kind: TransferKind,
        title: String,
        target: Option<PathBuf>,
        console_id: Option<u64>,
    ) -> TaskId {
        self.transfers.next_id += 1;
        let id = self.transfers.next_id;
        self.transfers.list.push(Transfer {
            id,
            kind,
            title,
            target,
            console_id,
            started: Instant::now(),
            finished: None,
            progress: Progress::default(),
            log: VecDeque::new(),
            dropped: 0,
            outcome: None,
            target_exists: false,
        });
        self.transfers.selected = Some(id);
        self.show_run = true;
        id
    }

    pub(super) fn transfer_event(&mut self, id: TaskId, event: TaskEvent) {
        let Some(transfer) = self.transfers.get_mut(id) else {
            return;
        };
        match event {
            TaskEvent::Progress(progress) => transfer.progress = progress,
            TaskEvent::Log { error, text } => transfer.push_log(error, &text),
            TaskEvent::Lines(lines) => {
                for line in lines {
                    transfer.push_log(line.error, &line.text);
                }
            }
            TaskEvent::Finished(outcome) => {
                transfer.finished = Some(Instant::now());
                transfer.target_exists = transfer.target.as_ref().is_some_and(|t| t.exists());
                let (text, error) = match &outcome {
                    Ok(summary) => (format!("{}: {summary}", transfer.title), false),
                    Err(reason) => (format!("{} failed: {reason}", transfer.title), true),
                };
                transfer.push_log(
                    error,
                    match &outcome {
                        Ok(summary) => summary,
                        Err(reason) => reason,
                    },
                );
                transfer.outcome = Some(outcome);
                let console = transfer.console_id;
                self.notice = Some(if error {
                    ServicesNotice::error(text.clone())
                } else {
                    ServicesNotice::info(text.clone())
                });
                if let Some(tab) = console.and_then(|id| self.tabs.iter_mut().find(|t| t.id == id)) {
                    tab.log(
                        if error {
                            super::results::LogKind::Error
                        } else {
                            super::results::LogKind::Info
                        },
                        text,
                    );
                }
            }
        }
    }

    /// Run tool window at the bottom; it takes the place of Services.
    pub(super) fn run_window(&mut self, ui: &mut Ui) {
        if !self.show_run {
            return;
        }
        let mut intents = Vec::new();
        let transfers = &self.transfers;
        egui::Panel::bottom("run_window")
            .resizable(true)
            .default_size(260.0)
            .min_size(120.0)
            .frame(
                egui::Frame::new()
                    .fill(theme::BG_PANEL)
                    .inner_margin(egui::Margin::symmetric(6, 0)),
            )
            .show(ui, |ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), theme::HEADER_HEIGHT),
                    Layout::right_to_left(Align::Center),
                    |ui| {
                        if theme::tool_button(ui, "−", "Hide").clicked() {
                            intents.push(RunIntent::Hide);
                        }
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            ui.label(RichText::new("Run").font(theme::semibold(13.0)));
                            ui.add_space(10.0);
                            egui::ScrollArea::horizontal()
                                .id_salt("run_tabs")
                                .scroll_bar_visibility(
                                    egui::scroll_area::ScrollBarVisibility::AlwaysHidden,
                                )
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        for t in &transfers.list {
                                            let mark = match &t.outcome {
                                                None => "◌ ",
                                                Some(Ok(_)) => "✓ ",
                                                Some(Err(_)) => "✗ ",
                                            };
                                            let color = match &t.outcome {
                                                None => theme::TEXT,
                                                Some(Ok(_)) => theme::TEXT,
                                                Some(Err(_)) => theme::RED,
                                            };
                                            let (tab, close) = theme::flat_tab(
                                                ui,
                                                RichText::new(format!("{mark}{}", t.title)).color(color),
                                                transfers.selected == Some(t.id),
                                                !t.running(),
                                            );
                                            if close {
                                                intents.push(RunIntent::Close(t.id));
                                            } else if tab.clicked() {
                                                intents.push(RunIntent::Select(t.id));
                                            }
                                        }
                                    });
                                });
                        });
                    },
                );
                theme::hairline(ui);
                let Some(t) = transfers
                    .selected
                    .and_then(|id| transfers.list.iter().find(|t| t.id == id))
                else {
                    theme::empty_text(ui, "Nothing to show");
                    return;
                };
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    theme::flat_widgets(ui);
                    if theme::tool_button_enabled(ui, t.running(), "■", "Stop").clicked() {
                        intents.push(RunIntent::Stop(t.id));
                    }
                    if let Some(target) = &t.target
                        && theme::tool_button_enabled(
                            ui,
                            t.target_exists,
                            "🗀",
                            if cfg!(target_os = "macos") { "Reveal in Finder" } else { "Open Containing Folder" },
                        )
                        .clicked()
                    {
                        intents.push(RunIntent::Reveal(target.clone()));
                    }
                    ui.add_space(6.0);
                    let (status, color) = match &t.outcome {
                        None => (t.kind.verb(), theme::TEXT),
                        Some(Ok(_)) => ("Done", theme::GREEN),
                        Some(Err(_)) => ("Failed", theme::RED),
                    };
                    ui.label(RichText::new(status).color(color));
                    ui.label(RichText::new(t.counters()).color(theme::TEXT_MUTED));
                    if t.running() {
                        ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
                    }
                });
                progress_bar(ui, t);
                ui.add_space(4.0);
                egui::Frame::new()
                    .fill(theme::BG_EDITOR)
                    .inner_margin(egui::Margin::symmetric(8, 6))
                    .show(ui, |ui| {
                        let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
                        let skipped = usize::from(t.dropped > 0);
                        egui::ScrollArea::both()
                            .id_salt(("run_log", t.id))
                            .auto_shrink([false, false])
                            .stick_to_bottom(true)
                            .show_rows(ui, row_height, t.log.len() + skipped, |ui, rows| {
                                for row in rows {
                                    if row < skipped {
                                        let text = format!("… {} earlier lines not shown", t.dropped);
                                        ui.label(RichText::new(text).monospace().color(theme::TEXT_MUTED));
                                        continue;
                                    }
                                    let line = &t.log[row - skipped];
                                    let text = RichText::new(&line.text).monospace().color(if line.error {
                                        theme::RED
                                    } else {
                                        theme::TEXT
                                    });
                                    // show_rows assumes one text line per row; long lines scroll sideways.
                                    ui.add(egui::Label::new(text).selectable(true).wrap_mode(egui::TextWrapMode::Extend));
                                }
                            });
                    });
            });
        self.apply_run_intents(intents);
    }

    pub(super) fn apply_run_intents(&mut self, intents: Vec<RunIntent>) {
        for intent in intents {
            match intent {
                RunIntent::Stop(id) => self.backend.stop_task(id),
                RunIntent::Select(id) => self.transfers.selected = Some(id),
                RunIntent::Close(id) => {
                    self.transfers.list.retain(|t| t.id != id);
                    if self.transfers.selected == Some(id) {
                        self.transfers.selected = self.transfers.list.last().map(|t| t.id);
                    }
                }
                RunIntent::Reveal(path) => reveal(&path),
                RunIntent::Show => self.show_run = true,
                RunIntent::Hide => self.show_run = false,
            }
        }
    }
}

/// Compact indicator of running transfers for the status bar.
pub(super) fn indicator(ui: &mut Ui, transfers: &Transfers, intents: &mut Vec<RunIntent>) {
    let running: Vec<&Transfer> = transfers.running().collect();
    let Some(first) = running.first() else {
        return;
    };
    ui.spinner();
    let text = if running.len() > 1 {
        format!("{} tasks running", running.len())
    } else {
        format!("{} · {}", first.title, first.counters())
    };
    let label = ui.add(
        egui::Label::new(RichText::new(text).small().color(theme::TEXT_MUTED))
            .sense(egui::Sense::click()),
    );
    if label.on_hover_text("Show in Run").clicked() {
        intents.push(RunIntent::Show);
        intents.push(RunIntent::Select(first.id));
    }
    if theme::tool_button(ui, RichText::new("■").small(), "Stop").clicked() {
        intents.push(RunIntent::Stop(first.id));
    }
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
}

/// Thin bar under the status line: the share of files done when there are
/// several, a moving segment while a single item runs.
fn progress_bar(ui: &mut Ui, t: &Transfer) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 3.0), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 1.5, theme::GRID_LINE);
    let color = match &t.outcome {
        Some(Err(_)) => theme::RED,
        Some(Ok(_)) => theme::GREEN,
        None => theme::ACCENT,
    };
    let fraction = match &t.outcome {
        Some(_) => Some(1.0),
        None if t.progress.total_bytes > 0 => Some(t.progress.bytes as f32 / t.progress.total_bytes as f32),
        None if t.progress.items > 1 => Some(t.progress.item as f32 / t.progress.items as f32),
        None => None,
    };
    match fraction {
        Some(f) => {
            let mut done = rect;
            done.set_width(rect.width() * f.clamp(0.0, 1.0));
            painter.rect_filled(done, 1.5, color);
        }
        None => {
            let phase = (ui.input(|i| i.time) * 0.6).fract() as f32;
            let width = rect.width() * 0.25;
            let x = rect.left() + (rect.width() + width) * phase - width;
            let segment = egui::Rect::from_min_max(
                egui::pos2(x.max(rect.left()), rect.top()),
                egui::pos2((x + width).min(rect.right()), rect.bottom()),
            );
            painter.rect_filled(segment, 1.5, color);
        }
    }
}

/// Shows a file or directory in the system file manager.
fn reveal(path: &std::path::Path) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg("-R").arg(path).spawn();
    #[cfg(not(target_os = "macos"))]
    {
        let folder = if path.is_dir() { path } else { path.parent().unwrap_or(path) };
        let _ = std::process::Command::new("xdg-open").arg(folder).spawn();
    }
}
