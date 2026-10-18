//! Files tool window: browse a folder and search it for data files that can be
//! opened ad hoc (CSV/TSV, Data Package, SQLite). Other files are not shown.
//!
//! Folders are listed and file sizes read on background threads; drawing uses
//! the cached listings and lays out only the rows in view.

use super::explorer::{ROW_HEIGHT, TreeIcon, arrow_hit, tree_row};
use super::{App, theme};
use crate::files::{self, FileKind};
use crate::model::ObjectKind;
use egui::{RichText, Ui};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

/// Search results shown at most, to keep the list and the scan short.
const SEARCH_LIMIT: usize = 300;
/// Folders listed at the same time.
const MAX_LISTINGS: usize = 2;
/// Searches running at the same time, including cancelled ones still stopping.
const MAX_SEARCHES: usize = 2;
/// Pause after the last keystroke before a search starts.
const SEARCH_DELAY: Duration = Duration::from_millis(250);

#[derive(Default)]
pub struct FilesPanel {
    expanded: HashSet<PathBuf>,
    listings: HashMap<PathBuf, Listing>,
    loader: ListingLoader,
    /// Rows of the tree, rebuilt when a listing arrives or a folder opens.
    rows: Option<Vec<Row>>,
    query: String,
    search: Option<Search>,
    searches: Arc<AtomicUsize>,
    selected: Option<PathBuf>,
}

/// One folder: subfolders, and supported files with their size label.
#[derive(Default)]
struct DirListing {
    dirs: Vec<PathBuf>,
    files: Vec<(PathBuf, String)>,
}

enum Listing {
    Loading,
    Ready(Arc<DirListing>),
}

/// Lists folders on at most [`MAX_LISTINGS`] threads. A refresh starts a new
/// generation, and listings of the old one are dropped when they arrive.
struct ListingLoader {
    generation: u64,
    queue: VecDeque<PathBuf>,
    running: usize,
    tx: Sender<(u64, PathBuf, DirListing)>,
    rx: Receiver<(u64, PathBuf, DirListing)>,
}

impl Default for ListingLoader {
    fn default() -> Self {
        let (tx, rx) = channel();
        Self {
            generation: 0,
            queue: VecDeque::new(),
            running: 0,
            tx,
            rx,
        }
    }
}

enum Row {
    Dir { path: PathBuf, depth: usize, open: bool },
    File { listing: Arc<DirListing>, index: usize, depth: usize },
    Loading { depth: usize },
}

/// Background search; results replace the tree while the query is not empty.
struct Search {
    query: String,
    root: PathBuf,
    cancel: Arc<AtomicBool>,
    rx: Receiver<Vec<PathBuf>>,
    results: Option<Vec<PathBuf>>,
}

impl Drop for Search {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

enum FileAction {
    Open(PathBuf),
    Toggle(PathBuf),
    Select(PathBuf),
    SetRoot(PathBuf),
    Focus(String),
    Close(String),
    Refresh,
    Hide,
}

/// Native file dialog limited to supported formats.
pub fn pick_file() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .add_filter(
            "Data files (CSV, TSV, Data Package, SQLite)",
            files::EXTENSIONS,
        )
        .pick_file()
}

fn kind_icon(kind: FileKind) -> ObjectKind {
    match kind {
        FileKind::Csv => ObjectKind::Table,
        FileKind::DataPackage => ObjectKind::Schema,
        FileKind::Sqlite => ObjectKind::Database,
    }
}

fn human_size(bytes: u64) -> String {
    let b = bytes as f64;
    match b {
        _ if b < 1024.0 => format!("{b} B"),
        _ if b < 1024.0 * 1024.0 => format!("{:.0} KB", b / 1024.0),
        _ => format!("{:.1} MB", b / 1024.0 / 1024.0),
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// A folder with the size of each listed file, read once.
fn read_listing(dir: &Path) -> DirListing {
    let Ok((dirs, files)) = files::list_dir(dir) else {
        return DirListing::default();
    };
    let files = files
        .into_iter()
        .map(|path| {
            let size = std::fs::metadata(&path).map_or_else(|_| String::new(), |m| human_size(m.len()));
            (path, size)
        })
        .collect();
    DirListing { dirs, files }
}

impl FilesPanel {
    /// Forgets listings and search results; the next frames read them again.
    fn refresh(&mut self) {
        self.loader.generation += 1;
        self.loader.queue.clear();
        self.listings.clear();
        self.rows = None;
        self.search = None;
    }

    /// Picks up finished listings and starts queued ones.
    fn poll_listings(&mut self, ctx: &egui::Context) {
        while let Ok((generation, dir, listing)) = self.loader.rx.try_recv() {
            self.loader.running -= 1;
            if generation == self.loader.generation {
                self.listings.insert(dir, Listing::Ready(Arc::new(listing)));
                self.rows = None;
            }
        }
        while self.loader.running < MAX_LISTINGS
            && let Some(dir) = self.loader.queue.pop_front()
        {
            self.loader.running += 1;
            let (tx, generation, ctx) = (self.loader.tx.clone(), self.loader.generation, ctx.clone());
            std::thread::spawn(move || {
                let listing = read_listing(&dir);
                let _ = tx.send((generation, dir, listing));
                ctx.request_repaint();
            });
        }
    }

    /// The listing of `dir`, queued for reading when it is not known yet.
    fn listing(&mut self, dir: &Path) -> Option<Arc<DirListing>> {
        match self.listings.get(dir) {
            Some(Listing::Ready(listing)) => Some(Arc::clone(listing)),
            Some(Listing::Loading) => None,
            None => {
                self.listings.insert(dir.to_path_buf(), Listing::Loading);
                self.loader.queue.push_back(dir.to_path_buf());
                None
            }
        }
    }

    /// Rows of the tree under `dir`, in display order: each folder's open
    /// subfolders follow their own row.
    fn tree_rows_at(&mut self, dir: &Path, depth: usize) -> Vec<Row> {
        let mut rows = Vec::new();
        let Some(listing) = self.listing(dir) else {
            rows.push(Row::Loading { depth });
            return rows;
        };
        for sub in &listing.dirs {
            let open = self.expanded.contains(sub);
            rows.push(Row::Dir { path: sub.clone(), depth, open });
            if open {
                rows.extend(self.tree_rows_at(sub, depth + 1));
            }
        }
        for index in 0..listing.files.len() {
            rows.push(Row::File { listing: Arc::clone(&listing), index, depth });
        }
        rows
    }
}

impl App {
    fn files_root(&self) -> PathBuf {
        self.config
            .files_root
            .clone()
            .or_else(|| directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."))
    }

    pub(super) fn files_panel(&mut self, ui: &mut Ui) {
        if !self.show_files {
            return;
        }
        let root = self.files_root();
        let mut actions = Vec::new();
        let opened: Vec<(String, PathBuf, FileKind)> = self
            .files
            .iter()
            .map(|f| (f.source.id.clone(), f.origin.clone(), f.kind))
            .collect();
        let panel = &mut self.files_panel;
        poll_search(panel, &root, ui.ctx());
        panel.poll_listings(ui.ctx());

        egui::Panel::left("files")
            .resizable(true)
            .default_size(320.0)
            .min_size(240.0)
            .frame(
                egui::Frame::new()
                    .fill(theme::BG_PANEL)
                    .inner_margin(egui::Margin::symmetric(6, 0)),
            )
            .show(ui, |ui| {
                theme::tool_window_header(ui, "Files", |ui| {
                    if theme::tool_button(ui, "−", "Hide").clicked() {
                        actions.push(FileAction::Hide);
                    }
                    if theme::tool_button(ui, "⟳", "Refresh").clicked() {
                        actions.push(FileAction::Refresh);
                    }
                    if theme::tool_button(ui, "🗁", "Open Folder…").clicked()
                        && let Some(dir) = rfd::FileDialog::new().set_directory(&root).pick_folder()
                    {
                        actions.push(FileAction::SetRoot(dir));
                    }
                    if theme::tool_button(ui, "🗋", "Open File…").clicked()
                        && let Some(path) = pick_file()
                    {
                        actions.push(FileAction::Open(path));
                    }
                });
                ui.horizontal(|ui| {
                    let parent = root.parent().map(Path::to_path_buf);
                    if theme::tool_button_enabled(ui, parent.is_some(), "↑", "Parent folder")
                        .clicked()
                        && let Some(p) = parent
                    {
                        actions.push(FileAction::SetRoot(p));
                    }
                    ui.add(
                        egui::Label::new(
                            RichText::new(file_name(&root)).font(theme::semibold(12.5)),
                        )
                        .truncate(),
                    )
                    .on_hover_text(root.display().to_string());
                });
                ui.add(
                    egui::TextEdit::singleline(&mut panel.query)
                        .id(super::tool_windows::files_search_id())
                        .hint_text("🔍 Search CSV, TSV, Data Package, SQLite")
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(4.0);
                theme::hairline(ui);

                if !opened.is_empty() {
                    ui.label(RichText::new("Opened").small().color(theme::TEXT_MUTED));
                    for (id, path, kind) in &opened {
                        let r = tree_row(
                            ui,
                            0,
                            None,
                            TreeIcon::Kind(kind_icon(*kind)),
                            &file_name(path),
                            kind.label(),
                            theme::TEXT_MUTED,
                            false,
                        )
                        .on_hover_text(path.display().to_string());
                        if r.clicked() {
                            actions.push(FileAction::Focus(id.clone()));
                        }
                        r.context_menu(|ui| {
                            if ui.button("Close File").clicked() {
                                actions.push(FileAction::Close(id.clone()));
                                ui.close();
                            }
                        });
                    }
                    ui.add_space(6.0);
                }
                if panel.query.trim().is_empty() {
                    dir_rows(ui, panel, &root, &mut actions);
                } else {
                    search_rows(ui, panel, &root, &mut actions);
                }
            });
        // Folders asked for while drawing start listing in the next frame.
        self.files_panel.poll_listings(ui.ctx());

        for a in actions {
            match a {
                FileAction::Open(path) => self.open_file(path),
                FileAction::Toggle(dir) => {
                    if !self.files_panel.expanded.remove(&dir) {
                        self.files_panel.expanded.insert(dir);
                    }
                    self.files_panel.rows = None;
                }
                FileAction::Select(path) => self.files_panel.selected = Some(path),
                FileAction::SetRoot(dir) => {
                    self.config.files_root = Some(dir);
                    self.files_panel.refresh();
                    self.save_config();
                }
                FileAction::Focus(id) => {
                    if let Some(i) = self.tabs.iter().position(|t| t.ds_id == id) {
                        self.select_tab_by_id(self.tabs[i].id);
                    }
                }
                FileAction::Close(id) => self.close_file(&id),
                FileAction::Refresh => self.files_panel.refresh(),
                FileAction::Hide => {
                    self.apply_tool_window_action(super::actions::ActionId::Files, ui.ctx())
                }
            }
        }
    }
}

/// Starts a search a moment after the query stops changing and picks up
/// finished results. A changed query cancels the running scan; at most
/// [`MAX_SEARCHES`] scans run at once.
fn poll_search(panel: &mut FilesPanel, root: &Path, ctx: &egui::Context) {
    let query = panel.query.trim().to_string();
    if query.is_empty() {
        panel.search = None;
        return;
    }
    let current = panel
        .search
        .as_ref()
        .is_some_and(|s| s.query == query && s.root == root);
    if !current {
        // Dropping the old search cancels its scan.
        panel.search = None;
        let changed = ctx.data_mut(|d| {
            let at = d.get_temp_mut_or_insert_with(egui::Id::new("files_search_typed"), || {
                (String::new(), Instant::now())
            });
            if at.0 != query {
                *at = (query.clone(), Instant::now());
            }
            at.1
        });
        let waited = changed.elapsed();
        if waited < SEARCH_DELAY || panel.searches.load(Ordering::Relaxed) >= MAX_SEARCHES {
            ctx.request_repaint_after(SEARCH_DELAY.saturating_sub(waited).max(Duration::from_millis(20)));
            return;
        }
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let running = Arc::clone(&panel.searches);
        running.fetch_add(1, Ordering::Relaxed);
        let (q, r, ctx, stop) = (query.clone(), root.to_path_buf(), ctx.clone(), Arc::clone(&cancel));
        std::thread::spawn(move || {
            let found = files::search(&r, &q, SEARCH_LIMIT, &stop);
            running.fetch_sub(1, Ordering::Relaxed);
            if !stop.load(Ordering::Relaxed) {
                let _ = tx.send(found);
            }
            ctx.request_repaint();
        });
        panel.search = Some(Search {
            query,
            root: root.to_path_buf(),
            cancel,
            rx,
            results: None,
        });
    }
    if let Some(s) = &mut panel.search
        && s.results.is_none()
        && let Ok(found) = s.rx.try_recv()
    {
        s.results = Some(found);
    }
}

fn file_row(
    ui: &mut Ui,
    selected: Option<&Path>,
    path: &Path,
    depth: usize,
    detail: &str,
    actions: &mut Vec<FileAction>,
) {
    let Some(kind) = files::kind_of(path) else {
        return;
    };
    let r = tree_row(
        ui,
        depth,
        None,
        TreeIcon::Kind(kind_icon(kind)),
        &file_name(path),
        detail,
        theme::TEXT_MUTED,
        selected == Some(path),
    )
    .on_hover_text(format!(
        "{}\n{} · double click to open",
        path.display(),
        kind.label()
    ));
    if r.clicked() {
        actions.push(FileAction::Select(path.to_path_buf()));
    }
    if r.double_clicked() {
        actions.push(FileAction::Open(path.to_path_buf()));
    }
    r.context_menu(|ui| {
        if ui.button("Open").clicked() {
            actions.push(FileAction::Open(path.to_path_buf()));
            ui.close();
        }
        if ui.button("Copy Path").clicked() {
            ui.ctx().copy_text(path.display().to_string());
            ui.close();
        }
    });
}

/// The folder tree with only supported files; only rows in view are drawn.
fn dir_rows(ui: &mut Ui, panel: &mut FilesPanel, root: &Path, actions: &mut Vec<FileAction>) {
    if panel.rows.is_none() {
        let rows = panel.tree_rows_at(root, 0);
        panel.rows = Some(rows);
    }
    let rows = panel.rows.as_ref().expect("built above");
    if rows.is_empty() {
        ui.add_space(12.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("No supported files in this folder").color(theme::TEXT_MUTED));
            ui.label(
                RichText::new("CSV, TSV, datapackage.json, SQLite")
                    .small()
                    .color(theme::TEXT_MUTED),
            );
        });
        return;
    }
    let selected = panel.selected.as_deref();
    ui.spacing_mut().item_spacing.y = 0.0;
    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .show_rows(ui, ROW_HEIGHT, rows.len(), |ui, range| {
            for row in &rows[range] {
                match row {
                    Row::Loading { depth } => {
                        ui.allocate_ui(egui::vec2(ui.available_width(), ROW_HEIGHT), |ui| {
                            ui.horizontal(|ui| {
                                ui.add_space(16.0 * *depth as f32 + 12.0);
                                ui.spinner();
                            });
                        });
                    }
                    Row::Dir { path, depth, open } => {
                        let r = tree_row(
                            ui,
                            *depth,
                            Some(*open),
                            TreeIcon::Kind(ObjectKind::Folder),
                            &file_name(path),
                            "",
                            theme::TEXT_MUTED,
                            false,
                        );
                        if r.double_clicked() || (r.clicked() && arrow_hit(&r)) {
                            actions.push(FileAction::Toggle(path.clone()));
                        }
                        r.context_menu(|ui| {
                            if ui.button("Use as Root Folder").clicked() {
                                actions.push(FileAction::SetRoot(path.clone()));
                                ui.close();
                            }
                        });
                    }
                    Row::File { listing, index, depth } => {
                        let (path, size) = &listing.files[*index];
                        file_row(ui, selected, path, *depth, size, actions);
                    }
                }
            }
        });
}

fn search_rows(ui: &mut Ui, panel: &FilesPanel, root: &Path, actions: &mut Vec<FileAction>) {
    let Some(results) = panel.search.as_ref().and_then(|s| s.results.as_ref()) else {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(RichText::new("Searching…").color(theme::TEXT_MUTED));
        });
        return;
    };
    if results.is_empty() {
        ui.label(RichText::new("No matching files").color(theme::TEXT_MUTED));
        return;
    }
    let selected = panel.selected.as_deref();
    ui.spacing_mut().item_spacing.y = 0.0;
    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .show_rows(ui, ROW_HEIGHT, results.len() + 1, |ui, range| {
            for index in range {
                let Some(path) = results.get(index) else {
                    if results.len() >= SEARCH_LIMIT {
                        ui.label(
                            RichText::new(format!("First {SEARCH_LIMIT} matches shown"))
                                .small()
                                .color(theme::TEXT_MUTED),
                        );
                    }
                    continue;
                };
                let folder = path
                    .parent()
                    .and_then(|p| p.strip_prefix(root).ok())
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                file_row(ui, selected, path, 0, &folder, actions);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_tree(folders: usize) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dbeast_files_panel_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..folders {
            let sub = dir.join(format!("folder{i:02}"));
            std::fs::create_dir_all(&sub).unwrap();
            std::fs::write(sub.join(format!("data{i}.csv")), "a,b\n1,2\n").unwrap();
        }
        std::fs::write(dir.join("top.csv"), "x\n".repeat(2048)).unwrap();
        dir
    }

    fn settle(panel: &mut FilesPanel, ctx: &egui::Context) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while panel.loader.running > 0 || !panel.loader.queue.is_empty() {
            panel.poll_listings(ctx);
            assert!(panel.loader.running <= MAX_LISTINGS);
            assert!(Instant::now() < deadline, "listings did not finish");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn folders_are_listed_in_the_background_with_sizes_read_once() {
        let root = temp_tree(12);
        let ctx = egui::Context::default();
        let mut panel = FilesPanel::default();
        // Drawing asks for the root without reading it.
        let rows = panel.tree_rows_at(&root, 0);
        assert!(matches!(rows[..], [Row::Loading { depth: 0 }]));
        settle(&mut panel, &ctx);
        let rows = panel.tree_rows_at(&root, 0);
        assert_eq!(rows.len(), 13, "12 folders and one file");
        let Row::File { listing, index, .. } = &rows[12] else {
            panic!("the file follows the folders")
        };
        assert_eq!(listing.files[*index].1, "4 KB", "size read with the listing");
        // Opening every folder queues them, and at most two are listed at once.
        for i in 0..12 {
            panel.expanded.insert(root.join(format!("folder{i:02}")));
        }
        let rows = panel.tree_rows_at(&root, 0);
        assert_eq!(rows.iter().filter(|r| matches!(r, Row::Loading { .. })).count(), 12);
        settle(&mut panel, &ctx);
        let rows = panel.tree_rows_at(&root, 0);
        assert_eq!(rows.len(), 12 * 2 + 1);
        assert!(matches!(&rows[1], Row::File { depth: 1, .. }), "a folder's file follows it");
        assert!(panel.loader.queue.is_empty(), "nothing is read again");
        std::fs::remove_dir_all(root).ok();
    }

    /// Run with `cargo test --release --lib files_panel_benchmark -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn files_panel_benchmark() {
        let dir = std::env::temp_dir().join(format!("dbeast_files_bench_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..5000 {
            std::fs::write(dir.join(format!("f{i:04}.csv")), "a\n").unwrap();
        }
        let (dirs, files) = files::list_dir(&dir).unwrap();
        // Work each frame did before: clone the cached listing and read every size.
        let mut old = Vec::new();
        for _ in 0..20 {
            let t = Instant::now();
            let (d, f) = (dirs.clone(), files.clone());
            let sizes: usize = f.iter().map(|p| std::fs::metadata(p).map_or(0, |m| m.len() as usize)).sum();
            std::hint::black_box((d, sizes));
            old.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        // Now: rows are built once; a frame only reads the rows in view.
        let ctx = egui::Context::default();
        let mut panel = FilesPanel::default();
        panel.tree_rows_at(&dir, 0);
        settle(&mut panel, &ctx);
        let t = Instant::now();
        let rows = panel.tree_rows_at(&dir, 0);
        let build = t.elapsed();
        old.sort_by(f64::total_cmp);
        eprintln!(
            "5000 files: before, per frame p50={:.3} ms p95={:.3} ms; after, rows built once in {build:?} ({} rows), no file system access per frame",
            old[10],
            old[19],
            rows.len()
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn listings_of_an_earlier_root_are_dropped() {
        let root = temp_tree(1);
        let ctx = egui::Context::default();
        let mut panel = FilesPanel::default();
        panel.tree_rows_at(&root, 0);
        panel.poll_listings(&ctx);
        assert_eq!(panel.loader.running, 1);
        panel.refresh();
        settle(&mut panel, &ctx);
        assert!(panel.listings.is_empty(), "the listing of the old generation is ignored");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn typing_starts_one_search_after_a_pause_and_cancels_the_previous_one() {
        let root = temp_tree(3);
        let ctx = egui::Context::default();
        let mut panel = FilesPanel::default();
        for query in ["d", "da", "dat"] {
            panel.query = query.into();
            poll_search(&mut panel, &root, &ctx);
            assert!(panel.search.is_none(), "no scan while typing");
        }
        std::thread::sleep(SEARCH_DELAY + Duration::from_millis(20));
        poll_search(&mut panel, &root, &ctx);
        let first = Arc::clone(&panel.search.as_ref().expect("scan after the pause").cancel);
        panel.query = "data1".into();
        poll_search(&mut panel, &root, &ctx);
        assert!(first.load(Ordering::Relaxed), "a changed query cancels the scan");
        std::thread::sleep(SEARCH_DELAY + Duration::from_millis(20));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            poll_search(&mut panel, &root, &ctx);
            assert!(panel.searches.load(Ordering::Relaxed) <= MAX_SEARCHES);
            if let Some(results) = panel.search.as_ref().and_then(|s| s.results.as_ref()) {
                assert_eq!(results.len(), 1);
                assert!(results[0].ends_with("data1.csv"));
                break;
            }
            assert!(Instant::now() < deadline, "search did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
        std::fs::remove_dir_all(root).ok();
    }
}
