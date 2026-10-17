//! Activity Monitor: a live dashboard of PostgreSQL sessions. Cards with the
//! numbers that matter, a timeline of session states, breakdowns that filter,
//! a blocking tree and a session table with details and Cancel/Terminate.
//! A live view of pg_stat_activity rather than a one-off query result.

use super::{App, theme};
use crate::activity::{
    self, BlockNode, Facet, History, Session, Snapshot, State, Summary, blocking_trees, breakdown, duration_text,
};
use crate::model::DataSource;
use egui::{Align, Color32, Context, Id, Layout, Rect, RichText, Sense, Stroke, Ui, pos2, vec2};
use egui_extras::{Column, TableBuilder};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Longest wait between samples while the server keeps failing.
const MAX_BACKOFF: u64 = 30;

/// What the window shows from one sample, derived once when it arrives so
/// frames only draw.
#[derive(Default)]
struct Derived {
    trees: Vec<BlockNode>,
    /// Snapshot index of each pid.
    index: HashMap<i32, usize>,
    /// Pids each session blocks.
    blocking: HashMap<i32, Vec<i32>>,
    /// Client connections plus this monitor, against max_connections.
    clients: usize,
    hidden_queries: bool,
    tps: Vec<f64>,
}

impl Derived {
    fn of(snapshot: &Snapshot, history: &History) -> Self {
        let mut blocking: HashMap<i32, Vec<i32>> = HashMap::new();
        for s in &snapshot.sessions {
            for &blocker in &s.blocked_by {
                blocking.entry(blocker).or_default().push(s.pid);
            }
        }
        Derived {
            trees: blocking_trees(&snapshot.sessions),
            index: snapshot.sessions.iter().enumerate().map(|(i, s)| (s.pid, i)).collect(),
            blocking,
            clients: snapshot.sessions.iter().filter(|s| s.is_client()).count() + 1,
            hidden_queries: !snapshot.server.sees_all && snapshot.sessions.iter().any(Session::query_hidden),
            tps: history.samples.iter().filter_map(|s| s.tps).collect(),
        }
    }
}

/// Sample, facet, background and this-database switches of a breakdown.
type BreakdownKey = (u64, Facet, bool, bool);

/// Seconds before the next sample: the interval, doubled per failure in a row.
fn backoff(interval: u64, failures: u32) -> u64 {
    interval.saturating_mul(1 << failures.min(5)).min(MAX_BACKOFF.max(interval))
}

/// Rows, summary and the longest visible duration for one [`ViewKey`].
struct View {
    key: ViewKey,
    rows: Arc<[usize]>,
    summary: Summary,
    max_duration: f64,
}

const INTERVALS: [(u64, &str); 4] = [(1, "1 s"), (2, "2 s"), (5, "5 s"), (10, "10 s")];

pub(super) fn state_color(state: State) -> Color32 {
    match state {
        State::Active | State::FastPath => theme::GREEN,
        State::Waiting => theme::RED,
        State::IdleInTransaction => theme::SEARCH_MARKER,
        State::IdleInTransactionAborted => Color32::from_rgb(0xE0, 0x7A, 0x5F),
        State::Idle => theme::LINE_NUMBER,
        State::Disabled => theme::TEXT_MUTED,
        State::Background => theme::SQL_FUNCTION,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sort {
    Pid,
    State,
    User,
    Database,
    Application,
    Client,
    Wait,
    Duration,
    Query,
}

/// What decides the visible rows; they are recomputed only when it changes.
#[derive(Clone, PartialEq)]
struct ViewKey {
    sample: u64,
    filter: String,
    hide_idle: bool,
    this_database: bool,
    background: bool,
    facet: Option<(Facet, String)>,
    sort: (Sort, bool),
}

pub(super) struct ActivityMonitor {
    id: u64,
    ds_id: String,
    ds: DataSource,
    open: bool,
    snapshot: Option<Snapshot>,
    history: History,
    /// Increments with every sample, for caches.
    sample: u64,
    derived: Derived,
    error: Option<String>,
    /// Failed samples in a row; each doubles the wait up to [`MAX_BACKOFF`].
    failures: u32,
    paused: bool,
    interval: u64,
    next_due: Instant,
    in_flight: Option<u64>,
    request: u64,
    filter: String,
    hide_idle: bool,
    this_database: bool,
    background: bool,
    facet: Facet,
    facet_filter: Option<(Facet, String)>,
    sort: (Sort, bool),
    selected: Option<i32>,
    confirm: Option<(i32, bool)>,
    message: Option<(String, bool)>,
    view: Option<View>,
    breakdown: Option<(BreakdownKey, Vec<(String, usize)>)>,
    /// Highlighted query of the selected session, per sample.
    query_job: Option<((u64, i32), egui::text::LayoutJob)>,
}

/// What the window asked for; applied after drawing.
enum Intent {
    Sample,
    Signal(i32, bool),
    OpenConsole(String),
}

impl ActivityMonitor {
    fn in_scope(&self, s: &Session, database: &str) -> bool {
        (self.background || s.is_client()) && (!self.this_database || s.database == database)
    }

    /// Visible rows (indexes into the snapshot) and the cards' summary.
    fn view(&mut self) -> &View {
        let key = ViewKey {
            sample: self.sample,
            filter: self.filter.trim().to_lowercase(),
            hide_idle: self.hide_idle,
            this_database: self.this_database,
            background: self.background,
            facet: self.facet_filter.clone(),
            sort: self.sort,
        };
        if self.view.as_ref().is_none_or(|view| view.key != key) {
            let empty = Snapshot::default();
            let snapshot = self.snapshot.as_ref().unwrap_or(&empty);
            let database = &snapshot.server.database;
            let scoped: Vec<usize> = (0..snapshot.sessions.len())
                .filter(|&i| self.in_scope(&snapshot.sessions[i], database))
                .collect();
            let summary = Summary::of(scoped.iter().map(|&i| &snapshot.sessions[i]));
            let mut rows: Vec<usize> = scoped
                .into_iter()
                .filter(|&i| {
                    let s = &snapshot.sessions[i];
                    !(self.hide_idle && s.state == State::Idle)
                        && s.matches(&key.filter)
                        && key.facet.as_ref().is_none_or(|(facet, value)| facet.value(s) == *value)
                })
                .collect();
            let (sort, descending) = self.sort;
            rows.sort_by(|&a, &b| {
                let (a, b) = (&snapshot.sessions[a], &snapshot.sessions[b]);
                let order = match sort {
                    Sort::Pid => a.pid.cmp(&b.pid),
                    Sort::State => a.state.cmp(&b.state),
                    Sort::User => a.user.cmp(&b.user),
                    Sort::Database => a.database.cmp(&b.database),
                    Sort::Application => a.application.cmp(&b.application),
                    Sort::Client => a.client.cmp(&b.client),
                    Sort::Wait => a.wait().cmp(b.wait()),
                    Sort::Duration => a.duration().unwrap_or(-1.0).total_cmp(&b.duration().unwrap_or(-1.0)),
                    Sort::Query => a.query.cmp(&b.query),
                }
                .then(a.pid.cmp(&b.pid));
                if descending { order.reverse() } else { order }
            });
            let max_duration = rows
                .iter()
                .filter_map(|&i| snapshot.sessions[i].duration())
                .fold(0.0, f64::max)
                .max(1.0);
            self.view = Some(View { key, rows: rows.into(), summary, max_duration });
        }
        self.view.as_ref().expect("computed above")
    }

    fn session(&self, pid: i32) -> Option<&Session> {
        self.snapshot.as_ref()?.sessions.get(*self.derived.index.get(&pid)?)
    }

    /// Facet values with counts for the sessions in scope, per sample.
    fn breakdown(&mut self) -> &[(String, usize)] {
        let key = (self.sample, self.facet, self.background, self.this_database);
        if self.breakdown.as_ref().is_none_or(|(k, _)| *k != key) {
            let list = match &self.snapshot {
                Some(snapshot) => {
                    let database = &snapshot.server.database;
                    breakdown(self.facet, snapshot.sessions.iter().filter(|s| self.in_scope(s, database)))
                }
                None => Vec::new(),
            };
            self.breakdown = Some((key, list));
        }
        &self.breakdown.as_ref().expect("computed above").1
    }

    fn delay(&self) -> Duration {
        Duration::from_secs(backoff(self.interval, self.failures))
    }
}

impl App {
    pub(super) fn open_activity_monitor(&mut self, ds_id: &str, database: Option<String>) {
        let Some(source) = self.source(ds_id).cloned() else {
            return;
        };
        let ds = match &database {
            Some(db) => source.with_database(db),
            None => source,
        };
        if let Some(monitor) = self.activity.iter_mut().find(|m| m.ds_id == ds_id && m.ds.database == ds.database) {
            monitor.open = true;
            return;
        }
        self.activity_next_id += 1;
        self.activity.push(ActivityMonitor {
            id: self.activity_next_id,
            ds_id: ds_id.to_string(),
            ds,
            open: true,
            snapshot: None,
            history: History::default(),
            sample: 0,
            derived: Derived::default(),
            error: None,
            failures: 0,
            paused: false,
            interval: 2,
            next_due: Instant::now(),
            in_flight: None,
            request: 0,
            filter: String::new(),
            hide_idle: false,
            this_database: false,
            background: false,
            facet: Facet::State,
            facet_filter: None,
            sort: (Sort::Duration, true),
            selected: None,
            confirm: None,
            message: None,
            view: None,
            breakdown: None,
            query_job: None,
        });
    }

    pub(super) fn activity_sample_loaded(&mut self, monitor: u64, request: u64, result: Result<Box<Snapshot>, String>) {
        let Some(m) = self.activity.iter_mut().find(|m| m.id == monitor) else {
            return;
        };
        if m.in_flight != Some(request) {
            return;
        }
        m.in_flight = None;
        match result {
            Ok(snapshot) => {
                m.history.push(&snapshot);
                m.derived = Derived::of(&snapshot, &m.history);
                m.snapshot = Some(*snapshot);
                m.sample += 1;
                m.error = None;
                m.failures = 0;
            }
            // The last good sample stays on screen, marked as stale.
            Err(error) => {
                m.error = Some(error);
                m.failures = m.failures.saturating_add(1);
            }
        }
        m.next_due = Instant::now() + m.delay();
    }

    pub(super) fn activity_signal_done(&mut self, monitor: u64, pid: i32, terminate: bool, result: Result<bool, String>) {
        let Some(m) = self.activity.iter_mut().find(|m| m.id == monitor) else {
            return;
        };
        let action = if terminate { "Terminate" } else { "Cancel" };
        m.message = Some(match result {
            Ok(true) => (format!("{action} sent to PID {pid}"), false),
            Ok(false) => (format!("PID {pid} no longer exists"), true),
            Err(error) => (format!("{action} failed: {error}"), true),
        });
        // Show the effect at once instead of at the next tick.
        m.next_due = Instant::now();
    }

    pub(super) fn activity_windows(&mut self, ctx: &Context) {
        let mut closed = Vec::new();
        let mut intents = Vec::new();
        // A minimized window shows nothing, so it does not poll the server.
        let minimized = ctx.input(|i| i.viewport().minimized == Some(true));
        for (index, monitor) in self.activity.iter_mut().enumerate() {
            if !monitor.open {
                continue;
            }
            let now = Instant::now();
            if !monitor.paused && !minimized && monitor.in_flight.is_none() {
                if now >= monitor.next_due {
                    intents.push((index, Intent::Sample));
                } else {
                    ctx.request_repaint_after(monitor.next_due - now);
                }
            }
            let mut open = true;
            let title = format!("Activity · {}", monitor.ds.display_name());
            let id = Id::new(("activity_monitor", monitor.id));
            theme::show_window(
                ctx,
                RichText::new(title).font(theme::semibold(14.0)),
                &mut open,
                |window| {
                    window
                        .id(id)
                        .default_size([1180.0, 760.0])
                        .min_size([720.0, 480.0])
                        // Content never widens the window past the application.
                        .max_size(ctx.content_rect().size() - vec2(24.0, 24.0))
                        .constrain(true)
                        .resizable(true)
                        .collapsible(false)
                        .frame(egui::Frame::window(&ctx.global_style()).fill(theme::BG_PANEL).inner_margin(12))
                },
                |ui| dashboard(ui, monitor, index, &mut intents),
            );
            if !open {
                closed.push(index);
            }
        }
        for (index, intent) in intents {
            let monitor = &mut self.activity[index];
            match intent {
                Intent::Sample => {
                    self.transfer_request += 1;
                    monitor.request = self.transfer_request;
                    monitor.in_flight = Some(monitor.request);
                    self.backend.activity_sample(monitor.id, monitor.request, monitor.ds.clone());
                }
                Intent::Signal(pid, terminate) => self.backend.activity_signal(monitor.id, monitor.ds.clone(), pid, terminate),
                Intent::OpenConsole(sql) => {
                    let ds_id = monitor.ds_id.clone();
                    let database = monitor.ds.database.clone();
                    self.open_console(ds_id, None, sql);
                    if let Some(tab) = self.tabs.last_mut() {
                        tab.database = (!database.is_empty()).then_some(database);
                    }
                }
            }
        }
        for index in closed.into_iter().rev() {
            let monitor = self.activity.remove(index);
            self.backend.activity_close(monitor.id);
        }
    }
}

fn dashboard(ui: &mut Ui, m: &mut ActivityMonitor, index: usize, intents: &mut Vec<(usize, Intent)>) {
    toolbar(ui, m, index, intents);
    ui.add_space(8.0);
    if m.snapshot.is_none() {
        match &m.error {
            Some(error) => {
                ui.label(RichText::new(error.as_str()).color(theme::RED));
            }
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Reading pg_stat_activity…").color(theme::TEXT_MUTED));
                });
            }
        }
        return;
    }
    if let Some(error) = &m.error {
        banner(ui, &format!("Refresh failed, showing the last sample: {error}"), theme::RED);
    }
    if m.derived.hidden_queries {
        banner(
            ui,
            "Queries of other users are hidden by the server. Grant pg_read_all_stats to see them.",
            theme::SEARCH_MARKER,
        );
    }
    stats(ui, m);
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        let width = ui.available_width();
        let chart_width = (width * 0.6).max(320.0);
        ui.allocate_ui_with_layout(vec2(chart_width, 130.0), Layout::top_down(Align::Min), |ui| timeline(ui, &m.history));
        ui.add_space(12.0);
        ui.allocate_ui_with_layout(vec2(ui.available_width(), 130.0), Layout::top_down(Align::Min), |ui| breakdown_panel(ui, m));
    });
    if !m.derived.trees.is_empty() {
        ui.add_space(8.0);
        let trees = std::mem::take(&mut m.derived.trees);
        blocking(ui, m, &trees);
        m.derived.trees = trees;
    }
    ui.add_space(8.0);
    // Table and details side by side; a panel inside a floating window would
    // not reserve its space.
    let height = ui.available_height().max(160.0);
    if m.selected.is_some() {
        let details_width = (ui.available_width() * 0.32).clamp(320.0, 520.0);
        ui.horizontal_top(|ui| {
            let table_width = ui.available_width() - details_width - 12.0;
            ui.allocate_ui_with_layout(vec2(table_width, height), Layout::top_down(Align::Min), |ui| {
                ui.set_min_size(vec2(table_width, height));
                ui.set_max_width(table_width);
                sessions_table(ui, m);
            });
            ui.add_space(4.0);
            ui.allocate_ui_with_layout(vec2(details_width, height), Layout::top_down(Align::Min), |ui| {
                ui.set_min_size(vec2(details_width, height));
                ui.set_max_width(details_width);
                details(ui, m, index, intents);
            });
        });
    } else {
        sessions_table(ui, m);
    }
}

fn banner(ui: &mut Ui, text: &str, color: Color32) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.15))
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.5)))
        .corner_radius(4)
        .inner_margin(egui::Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(text).color(theme::TEXT));
        });
    ui.add_space(6.0);
}

fn toolbar(ui: &mut Ui, m: &mut ActivityMonitor, index: usize, intents: &mut Vec<(usize, Intent)>) {
    ui.horizontal(|ui| {
        theme::flat_widgets(ui);
        let (dot, status) = match (&m.error, m.paused) {
            (Some(_), _) => (theme::RED, "Error"),
            (None, true) => (theme::TEXT_MUTED, "Paused"),
            (None, false) => (theme::GREEN, "Live"),
        };
        let (rect, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
        ui.painter().circle_filled(rect.center(), 4.0, dot);
        ui.label(RichText::new(status).color(theme::TEXT));
        if let Some(server) = m.snapshot.as_ref().map(|s| &s.server) {
            ui.label(RichText::new(format!("PostgreSQL {} · {}", server.version, server.database)).small().color(theme::TEXT_MUTED));
        }
        ui.separator();
        if theme::tool_button(ui, if m.paused { "▶" } else { "⏸" }, if m.paused { "Resume" } else { "Pause" }).clicked() {
            m.paused = !m.paused;
            m.next_due = Instant::now();
        }
        if theme::tool_button_enabled(ui, m.in_flight.is_none(), "⟳", "Refresh Now").clicked() {
            intents.push((index, Intent::Sample));
        }
        egui::ComboBox::from_id_salt(("activity_interval", m.id))
            .selected_text(format!("Every {}", INTERVALS.iter().find(|(s, _)| *s == m.interval).map_or("2 s", |(_, l)| l)))
            .width(96.0)
            .show_ui(ui, |ui| {
                for (seconds, label) in INTERVALS {
                    if ui.selectable_value(&mut m.interval, seconds, label).clicked() {
                        m.next_due = Instant::now() + Duration::from_secs(seconds);
                    }
                }
            });
        ui.separator();
        ui.add(
            egui::TextEdit::singleline(&mut m.filter)
                .hint_text("Filter by PID, user, application, query…")
                .desired_width(260.0),
        );
        ui.toggle_value(&mut m.hide_idle, "Hide idle").on_hover_text("Hide sessions waiting for a new command");
        ui.toggle_value(&mut m.this_database, "This database").on_hover_text("Only sessions of the database this monitor is connected to");
        ui.toggle_value(&mut m.background, "Background").on_hover_text("Include autovacuum, WAL writer and other background processes");
        if let Some((facet, value)) = m.facet_filter.clone() {
            ui.separator();
            if ui
                .button(RichText::new(format!("{}: {value}  ×", facet.label())).color(theme::ACCENT))
                .on_hover_text("Clear this filter")
                .clicked()
            {
                m.facet_filter = None;
            }
        }
    });
}

/// One "label value" pair of the stats strip; colour only flags a problem.
fn stat(ui: &mut Ui, label: &str, value: &str, alert: Option<Color32>, tooltip: &str) {
    let response = ui
        .horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 5.0;
            ui.label(RichText::new(label).color(theme::TEXT_MUTED));
            ui.label(RichText::new(value).font(theme::semibold(13.0)).color(alert.unwrap_or(theme::TEXT)));
        })
        .response;
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, format!("{label}: {value}")));
    if !tooltip.is_empty() {
        response.on_hover_text(tooltip);
    }
}

/// Server numbers in one compact line under the toolbar.
fn stats(ui: &mut Ui, m: &mut ActivityMonitor) {
    m.view();
    let (Some(snapshot), Some(view)) = (&m.snapshot, &m.view) else {
        return;
    };
    let summary = &view.summary;
    let server = &snapshot.server;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 14.0;
        // Every client connection counts against max_connections, whatever the filters.
        let clients = m.derived.clients;
        let share = clients as f32 / server.max_connections.max(1) as f32;
        let usage = if share > 0.9 { Some(theme::RED) } else if share > 0.7 { Some(theme::SEARCH_MARKER) } else { None };
        stat(
            ui,
            "Connections",
            &format!("{clients} / {}", server.max_connections),
            usage,
            &format!("{:.0}% of max_connections, including this monitor", share * 100.0),
        );
        let active = summary.count(State::Active) + summary.count(State::FastPath);
        stat(
            ui,
            "Active",
            &active.to_string(),
            None,
            &summary.longest_query.map_or("No running queries".into(), |(pid, age)| format!("Longest query {} (PID {pid})", duration_text(age))),
        );
        let waiting = summary.count(State::Waiting);
        stat(
            ui,
            "Waiting on locks",
            &waiting.to_string(),
            (waiting > 0).then_some(theme::RED),
            &match server.waiting_locks {
                1 => "1 lock request not granted".to_string(),
                n => format!("{n} lock requests not granted"),
            },
        );
        let in_tx = summary.count(State::IdleInTransaction) + summary.count(State::IdleInTransactionAborted);
        stat(
            ui,
            "Idle in transaction",
            &in_tx.to_string(),
            (summary.stale_transactions > 0).then_some(theme::SEARCH_MARKER),
            &match (summary.stale_transactions, summary.oldest_transaction) {
                (0, None) => "No open transactions".to_string(),
                (0, Some((pid, age))) => format!("Oldest transaction {} (PID {pid})", duration_text(age)),
                (n, _) => format!("{n} open for over a minute: they hold locks and delay vacuum"),
            },
        );
        stat(
            ui,
            "Blocked",
            &summary.blocked.to_string(),
            (summary.blocked > 0).then_some(theme::RED),
            "Sessions waiting for another session; see Blocking below",
        );
        let tps = m.history.tps();
        stat(ui, "Transactions/s", &tps.map_or("–".into(), |t| format!("{t:.0}")), None, "Commits and rollbacks of all databases since the previous sample");
        let (rect, _) = ui.allocate_exact_size(vec2(64.0, 14.0), Sense::hover());
        sparkline(ui, rect, &m.derived.tps);
    });
}

fn sparkline(ui: &Ui, rect: Rect, values: &[f64]) {
    if values.len() < 2 {
        return;
    }
    let max = values.iter().copied().fold(0.0, f64::max).max(1.0);
    let step = rect.width() / (values.len() - 1) as f32;
    let points: Vec<egui::Pos2> = values
        .iter()
        .enumerate()
        .map(|(i, v)| pos2(rect.left() + i as f32 * step, rect.bottom() - (v / max) as f32 * rect.height()))
        .collect();
    ui.painter().add(egui::Shape::line(points, Stroke::new(1.5, theme::ACCENT)));
}

/// Stacked columns of session states over the last samples.
fn timeline(ui: &mut Ui, history: &History) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Sessions over time").font(theme::semibold(13.0)));
        for (label, color) in [
            ("active", state_color(State::Active)),
            ("waiting", state_color(State::Waiting)),
            ("idle in transaction", state_color(State::IdleInTransaction)),
            ("idle", state_color(State::Idle)),
        ] {
            let (rect, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
            ui.painter().rect_filled(rect.shrink(1.0), 2.0, color);
            ui.label(RichText::new(label).small().color(theme::TEXT_MUTED));
        }
    });
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), ui.available_height().max(100.0)), Sense::hover());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Other, true, "Sessions over time"));
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 6.0, theme::BG_EDITOR);
    let plot = rect.shrink2(vec2(36.0, 10.0)).translate(vec2(14.0, 0.0));
    let samples = &history.samples;
    let total = |s: &activity::Sample| s.active + s.waiting + s.idle_in_transaction + s.idle;
    let max = samples.iter().map(total).max().unwrap_or(0).max(4);
    // A round axis maximum with two gridlines.
    let step = ((max as f32 / 2.0).ceil()).max(1.0);
    let top = step * 2.0;
    for i in 0..=2 {
        let value = step * i as f32;
        let y = plot.bottom() - value / top * plot.height();
        painter.line_segment([pos2(plot.left(), y), pos2(plot.right(), y)], Stroke::new(1.0, theme::GRID_LINE));
        painter.text(pos2(plot.left() - 6.0, y), egui::Align2::RIGHT_CENTER, format!("{value:.0}"), egui::FontId::proportional(10.5), theme::TEXT_MUTED);
    }
    if samples.is_empty() {
        return;
    }
    let slot = plot.width() / activity::HISTORY as f32;
    let width = (slot - 1.0).max(1.0);
    // The newest sample is on the right edge, as in a moving chart.
    let first_x = plot.right() - samples.len() as f32 * slot;
    let hover = response.hover_pos();
    let mut hovered = None;
    for (i, sample) in samples.iter().enumerate() {
        let x = first_x + i as f32 * slot;
        let mut y = plot.bottom();
        for (count, state) in [
            (sample.active, State::Active),
            (sample.waiting, State::Waiting),
            (sample.idle_in_transaction, State::IdleInTransaction),
            (sample.idle, State::Idle),
        ] {
            if count == 0 {
                continue;
            }
            let h = count as f32 / top * plot.height();
            painter.rect_filled(Rect::from_min_max(pos2(x, y - h), pos2(x + width, y)), 0.0, state_color(state));
            y -= h;
        }
        if hover.is_some_and(|p| p.x >= x && p.x < x + slot && plot.contains(p)) {
            hovered = Some((x, *sample, samples.len() - 1 - i));
        }
    }
    if let Some((x, sample, age)) = hovered {
        painter.line_segment([pos2(x + width / 2.0, plot.top()), pos2(x + width / 2.0, plot.bottom())], Stroke::new(1.0, theme::TEXT_MUTED));
        response.on_hover_ui_at_pointer(|ui| {
            ui.label(if age == 0 { "Latest sample".to_string() } else { format!("{age} samples ago") });
            ui.label(format!("active {} · waiting {} · idle in transaction {} · idle {}", sample.active, sample.waiting, sample.idle_in_transaction, sample.idle));
            if let Some(tps) = sample.tps {
                ui.label(format!("{tps:.0} transactions/s"));
            }
        });
    }
}

fn breakdown_panel(ui: &mut Ui, m: &mut ActivityMonitor) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Breakdown").font(theme::semibold(13.0)));
        ui.add_space(6.0);
        theme::flat_widgets(ui);
        for facet in Facet::ALL {
            if ui.selectable_label(m.facet == facet, RichText::new(facet.label()).small()).clicked() {
                m.facet = facet;
            }
        }
    });
    if m.snapshot.is_none() {
        return;
    }
    m.breakdown();
    // Taken out while drawing, which also needs the monitor mutably.
    let Some((key, list)) = m.breakdown.take() else {
        return;
    };
    let total: usize = list.iter().map(|(_, n)| n).sum::<usize>().max(1);
    let mut clicked = None;
    egui::Frame::new().fill(theme::BG_EDITOR).corner_radius(6).inner_margin(8).show(ui, |ui| {
        ui.set_min_size(ui.available_size());
        egui::ScrollArea::vertical().id_salt(("activity_breakdown", m.id)).auto_shrink([false, false]).show(ui, |ui| {
            for (value, count) in list.iter().take(12) {
                let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 20.0), Sense::click());
                response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, format!("{value}: {count}")));
                let active = m.facet_filter.as_ref().is_some_and(|(f, v)| *f == m.facet && v == value);
                let painter = ui.painter();
                if response.hovered() || active {
                    painter.rect_filled(rect, 3.0, theme::BG_HOVER);
                }
                let label_width = rect.width() * 0.42;
                let galley = painter.layout_no_wrap(value.clone(), egui::FontId::proportional(12.0), theme::TEXT);
                let clip = Rect::from_min_size(rect.min, vec2(label_width - 6.0, rect.height()));
                painter.with_clip_rect(clip).galley(pos2(rect.left() + 4.0, rect.center().y - galley.size().y / 2.0), galley, theme::TEXT);
                let bar_max = rect.width() - label_width - 44.0;
                let color = match m.facet {
                    Facet::State => State::ALL.into_iter().find(|s| s.label() == value).map_or(theme::ACCENT, state_color),
                    _ => theme::ACCENT,
                };
                let bar = Rect::from_min_size(pos2(rect.left() + label_width, rect.center().y - 5.0), vec2((*count as f32 / total as f32 * bar_max).max(2.0), 10.0));
                painter.rect_filled(bar, 2.0, color.gamma_multiply(0.85));
                painter.text(pos2(rect.right() - 4.0, rect.center().y), egui::Align2::RIGHT_CENTER, count.to_string(), egui::FontId::proportional(12.0), theme::TEXT_MUTED);
                if response.on_hover_text("Filter the sessions by this value").clicked() {
                    clicked = Some((value.clone(), active));
                }
            }
            if list.is_empty() {
                theme::empty_text(ui, "No sessions");
            }
        });
    });
    m.breakdown = Some((key, list));
    if let Some((value, active)) = clicked {
        m.facet_filter = if active { None } else { Some((m.facet, value)) };
    }
}

fn blocking(ui: &mut Ui, m: &mut ActivityMonitor, trees: &[BlockNode]) {
    let blocked: usize = trees.iter().map(BlockNode::count).sum();
    let title = format!(
        "Blocking · {} waiting on {}",
        if blocked == 1 { "1 session".to_string() } else { format!("{blocked} sessions") },
        if trees.len() == 1 { "1 blocker".to_string() } else { format!("{} blockers", trees.len()) }
    );
    egui::CollapsingHeader::new(RichText::new(title).font(theme::semibold(13.0)).color(theme::RED))
        .id_salt(("activity_blocking", m.id))
        .default_open(true)
        .show(ui, |ui| {
            egui::Frame::new().fill(theme::BG_EDITOR).corner_radius(6).inner_margin(8).show(ui, |ui| {
                ui.set_width(ui.available_width());
                egui::ScrollArea::vertical().id_salt(("activity_blocking_list", m.id)).max_height(140.0).show(ui, |ui| {
                    for tree in trees {
                        block_row(ui, m, tree, 0);
                    }
                });
            });
        });
}

fn block_row(ui: &mut Ui, m: &mut ActivityMonitor, node: &BlockNode, depth: usize) {
    let (state, user, duration, query) = match m.session(node.pid) {
        Some(s) => (s.state, s.user.clone(), s.duration(), s.query_line.clone()),
        None => (State::Background, String::new(), None, String::new()),
    };
    ui.horizontal(|ui| {
        ui.add_space(depth as f32 * 18.0);
        ui.label(RichText::new(if depth == 0 { "■" } else { "└" }).color(if depth == 0 { theme::RED } else { theme::TEXT_MUTED }));
        let selected = m.selected == Some(node.pid);
        if ui.selectable_label(selected, RichText::new(format!("PID {}", node.pid)).monospace()).clicked() {
            m.selected = Some(node.pid);
        }
        state_chip(ui, state);
        ui.label(RichText::new(user).color(theme::TEXT_MUTED));
        if let Some(d) = duration {
            ui.label(RichText::new(duration_text(d)).color(theme::TEXT_MUTED));
        }
        if depth == 0 {
            ui.label(RichText::new(format!("blocks {}", node.count())).color(theme::RED));
        }
        ui.add(egui::Label::new(RichText::new(query).monospace().color(theme::TEXT_MUTED)).truncate());
    });
    for child in &node.children {
        block_row(ui, m, child, depth + 1);
    }
}

fn state_chip(ui: &mut Ui, state: State) {
    let color = state_color(state);
    let text = state.label();
    let galley = ui.painter().layout_no_wrap(text.to_string(), egui::FontId::proportional(11.5), theme::TEXT);
    let size = vec2(galley.size().x + 22.0, 18.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, text));
    let painter = ui.painter();
    painter.rect_filled(rect, 9.0, color.gamma_multiply(0.18));
    painter.circle_filled(pos2(rect.left() + 9.0, rect.center().y), 3.5, color);
    painter.galley(pos2(rect.left() + 16.0, rect.center().y - galley.size().y / 2.0), galley, theme::TEXT);
}

fn sessions_table(ui: &mut Ui, m: &mut ActivityMonitor) {
    let id = m.id;
    let view = m.view();
    let (rows, max_duration) = (view.rows.clone(), view.max_duration);
    let Some(snapshot) = m.snapshot.as_ref() else {
        return;
    };
    ui.horizontal(|ui| {
        ui.label(RichText::new("Sessions").font(theme::semibold(13.0)));
        ui.label(RichText::new(format!("{} of {}", rows.len(), snapshot.sessions.len())).small().color(theme::TEXT_MUTED));
    });
    let mut clicked = None;
    let mut sort = m.sort;
    let selected = m.selected;
    egui::Frame::new().fill(theme::BG_EDITOR).corner_radius(6).inner_margin(4).show(ui, |ui| {
        ui.set_min_size(ui.available_size());
        let font = theme::grid_font(ui);
        let mut header = |ui: &mut Ui, label: &str, column: Sort| {
            let arrow = if sort.0 == column { if sort.1 { " ▼" } else { " ▲" } } else { "" };
            let response = ui.add(egui::Label::new(RichText::new(format!("{label}{arrow}")).strong()).sense(Sense::click()));
            if response.clicked() {
                sort = if sort.0 == column { (column, !sort.1) } else { (column, matches!(column, Sort::Duration)) };
            }
        };
        TableBuilder::new(ui)
            .id_salt(("activity_sessions", id))
            .striped(true)
            .resizable(true)
            .sense(Sense::click())
            .auto_shrink([false, false])
            .min_scrolled_height(0.0)
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::initial(64.0).at_least(48.0))
            .column(Column::initial(150.0).at_least(90.0).clip(true))
            .column(Column::initial(100.0).at_least(60.0).clip(true))
            .column(Column::initial(100.0).at_least(60.0).clip(true))
            .column(Column::initial(130.0).at_least(60.0).clip(true))
            .column(Column::initial(110.0).at_least(60.0).clip(true))
            .column(Column::initial(140.0).at_least(60.0).clip(true))
            .column(Column::initial(130.0).at_least(90.0))
            .column(Column::remainder().at_least(120.0).clip(true))
            .header(24.0, |mut row| {
                for (label, column) in [
                    ("PID", Sort::Pid),
                    ("State", Sort::State),
                    ("User", Sort::User),
                    ("Database", Sort::Database),
                    ("Application", Sort::Application),
                    ("Client", Sort::Client),
                    ("Wait", Sort::Wait),
                    ("Duration", Sort::Duration),
                    ("Query", Sort::Query),
                ] {
                    row.col(|ui| header(ui, label, column));
                }
            })
            .body(|body| {
                body.rows(24.0, rows.len(), |mut row| {
                    let s = &snapshot.sessions[rows[row.index()]];
                    row.set_selected(selected == Some(s.pid));
                    let text = |t: &str| RichText::new(t.to_string()).font(font.clone());
                    row.col(|ui| {
                        ui.label(text(&s.pid.to_string()).color(theme::SQL_NUMBER));
                    });
                    row.col(|ui| state_chip(ui, s.state));
                    row.col(|ui| {
                        ui.label(text(&s.user));
                    });
                    row.col(|ui| {
                        ui.label(text(&s.database));
                    });
                    row.col(|ui| {
                        ui.label(text(if s.application.is_empty() { &s.backend_type } else { &s.application }).color(if s.application.is_empty() { theme::TEXT_MUTED } else { theme::TEXT }));
                    });
                    row.col(|ui| {
                        ui.label(text(&s.client).color(theme::TEXT_MUTED));
                    });
                    row.col(|ui| {
                        ui.label(text(s.wait()).color(if s.state == State::Waiting { theme::RED } else { theme::TEXT_MUTED }));
                    });
                    row.col(|ui| duration_cell(ui, s, max_duration));
                    row.col(|ui| {
                        let line = &s.query_line;
                        let rich = if s.query_hidden() { text(line).italics().color(theme::TEXT_MUTED) } else { text(line).color(theme::TEXT) };
                        ui.add(egui::Label::new(rich).truncate().selectable(false));
                    });
                    if row.response().clicked() {
                        clicked = Some(s.pid);
                    }
                });
            });
    });
    m.sort = sort;
    if let Some(pid) = clicked {
        m.selected = if m.selected == Some(pid) { None } else { Some(pid) };
        m.confirm = None;
    }
}

/// Duration as text over a bar scaled to the longest visible one.
fn duration_cell(ui: &mut Ui, s: &Session, max: f64) {
    let Some(duration) = s.duration() else {
        return;
    };
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 18.0), Sense::hover());
    let painter = ui.painter();
    // Square root keeps short durations visible next to a long one.
    let share = (duration / max).sqrt().clamp(0.02, 1.0) as f32;
    let bar = Rect::from_min_size(pos2(rect.left(), rect.bottom() - 3.0), vec2(rect.width() * share, 3.0));
    painter.rect_filled(bar, 1.5, state_color(s.state).gamma_multiply(0.8));
    painter.text(pos2(rect.left(), rect.top() + 7.0), egui::Align2::LEFT_CENTER, duration_text(duration), egui::FontId::proportional(12.0), theme::TEXT);
}

fn details(ui: &mut Ui, m: &mut ActivityMonitor, index: usize, intents: &mut Vec<(usize, Intent)>) {
    let Some(pid) = m.selected else {
        return;
    };
    theme::panel_header(ui, |ui| {
        ui.label(RichText::new(format!("PID {pid}")).font(theme::semibold(14.0)));
        if let Some(s) = m.session(pid) {
            state_chip(ui, s.state);
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if theme::tool_button(ui, "×", "Close Details").clicked() {
                m.selected = None;
            }
        });
    });
    let Some(session) = m.session(pid).cloned() else {
        theme::empty_text(ui, "The session has ended");
        return;
    };
    if let Some((text, error)) = &m.message {
        ui.label(RichText::new(text.as_str()).color(if *error { theme::RED } else { theme::GREEN }));
        ui.add_space(4.0);
    }
    ui.horizontal_wrapped(|ui| {
        theme::flat_widgets(ui);
        let running = matches!(session.state, State::Active | State::Waiting | State::FastPath);
        if ui
            .add_enabled(running, egui::Button::new("Cancel Query"))
            .on_hover_text("pg_cancel_backend: stops the running query, keeps the session")
            .on_disabled_hover_text("The session is not running a query")
            .clicked()
        {
            m.confirm = Some((pid, false));
        }
        if ui.button(RichText::new("Terminate Session").color(theme::RED)).on_hover_text("pg_terminate_backend: closes the connection").clicked() {
            m.confirm = Some((pid, true));
        }
        if ui.add_enabled(!session.query.is_empty() && !session.query_hidden(), egui::Button::new("Open in Console")).clicked() {
            intents.push((index, Intent::OpenConsole(session.query.clone())));
        }
        if ui.add_enabled(!session.query.is_empty(), egui::Button::new("Copy Query")).clicked() {
            ui.ctx().copy_text(session.query.clone());
        }
    });
    if let Some((target, terminate)) = m.confirm.filter(|(target, _)| *target == pid) {
        egui::Frame::new()
            .fill(theme::RED.gamma_multiply(0.12))
            .stroke(Stroke::new(1.0, theme::RED.gamma_multiply(0.5)))
            .corner_radius(4)
            .inner_margin(8)
            .show(ui, |ui| {
                ui.label(if terminate {
                    format!("Terminate session {target} of {}? Its open transaction is rolled back.", session.user)
                } else {
                    format!("Cancel the query of session {target}?")
                });
                ui.horizontal(|ui| {
                    if theme::primary_button(ui, if terminate { "Terminate" } else { "Cancel Query" }).clicked() {
                        intents.push((index, Intent::Signal(target, terminate)));
                        m.confirm = None;
                    }
                    if theme::dialog_button(ui, "Keep").clicked() {
                        m.confirm = None;
                    }
                });
            });
    }
    ui.add_space(6.0);
    egui::ScrollArea::vertical().id_salt(("activity_details_scroll", m.id)).show(ui, |ui| {
        egui::Grid::new(("activity_fields", m.id)).num_columns(2).spacing([12.0, 5.0]).striped(true).show(ui, |ui| {
            let field = |ui: &mut Ui, label: &str, value: String| {
                ui.label(RichText::new(label).color(theme::TEXT_MUTED));
                ui.add(egui::Label::new(if value.is_empty() { RichText::new("–").color(theme::TEXT_MUTED) } else { RichText::new(value) }).selectable(true));
                ui.end_row();
            };
            field(ui, "User", session.user.clone());
            field(ui, "Database", session.database.clone());
            field(ui, "Application", session.application.clone());
            field(ui, "Client", session.client.clone());
            field(ui, "Backend type", session.backend_type.clone());
            field(ui, "Connected", format!("{} ({} ago)", session.backend_start, session.backend_age.map(duration_text).unwrap_or_default()));
            field(ui, "Query started", format!("{} ({} ago)", session.query_start, session.query_age.map(duration_text).unwrap_or_default()));
            field(ui, "Transaction age", session.xact_age.map(duration_text).unwrap_or_default());
            field(ui, "In this state", session.state_age.map(duration_text).unwrap_or_default());
            field(ui, "Wait", session.wait.clone());
            field(ui, "Transaction ID", session.xid.clone());
            field(ui, "xmin", session.xmin.clone());
        });
        let blocking = m.derived.blocking.get(&pid).cloned().unwrap_or_default();
        for (label, pids, color) in [("Blocked by", session.blocked_by.clone(), theme::RED), ("Blocking", blocking, theme::SEARCH_MARKER)] {
            if pids.is_empty() {
                continue;
            }
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(label).color(color));
                for other in pids {
                    if ui.link(format!("PID {other}")).clicked() {
                        m.selected = Some(other);
                    }
                }
            });
        }
        ui.add_space(8.0);
        ui.label(RichText::new("Query").font(theme::semibold(13.0)));
        egui::Frame::new().fill(theme::BG_EDITOR).corner_radius(4).inner_margin(8).show(ui, |ui| {
            ui.set_width(ui.available_width());
            if session.query_hidden() {
                ui.label(RichText::new("Hidden by the server: the monitor's role cannot read other users' queries.").italics().color(theme::TEXT_MUTED));
            } else if session.query.is_empty() {
                ui.label(RichText::new("No query").color(theme::TEXT_MUTED));
            } else {
                // Highlighting runs once per sample and session, not per frame.
                let key = (m.sample, pid);
                if m.query_job.as_ref().is_none_or(|(k, _)| *k != key) {
                    m.query_job = Some((key, super::highlight::layout_with_folds(&session.query, 12.5, &|_| None)));
                }
                let mut job = m.query_job.as_ref().expect("computed above").1.clone();
                job.wrap.max_width = ui.available_width();
                ui.add(egui::Label::new(job).selectable(true));
            }
        });
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn failed_samples_back_off_up_to_thirty_seconds() {
        assert_eq!(super::backoff(2, 0), 2);
        assert_eq!(super::backoff(2, 1), 4);
        assert_eq!(super::backoff(2, 3), 16);
        assert_eq!(super::backoff(2, 9), 30);
        assert_eq!(super::backoff(10, 0), 10);
    }
}
