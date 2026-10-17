//! Files tool window: browse a folder and search it for data files that can be
//! opened ad hoc (CSV/TSV, Data Package, SQLite). Other files are not shown.

use super::explorer::{TreeIcon, arrow_hit, tree_row};
use super::{App, theme};
use crate::files::{self, FileKind};
use crate::model::ObjectKind;
use egui::{RichText, Ui};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};

/// Search results shown at most, to keep the list and the scan short.
const SEARCH_LIMIT: usize = 300;

#[derive(Default)]
pub struct FilesPanel {
    expanded: HashSet<PathBuf>,
    listings: HashMap<PathBuf, (Vec<PathBuf>, Vec<PathBuf>)>,
    query: String,
    search: Option<Search>,
    selected: Option<PathBuf>,
}

/// Background search; results replace the tree while the query is not empty.
struct Search {
    query: String,
    root: PathBuf,
    rx: Receiver<Vec<PathBuf>>,
    results: Option<Vec<PathBuf>>,
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

fn human_size(path: &Path) -> String {
    let Ok(meta) = std::fs::metadata(path) else {
        return String::new();
    };
    let b = meta.len() as f64;
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

                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
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
                            dir_rows(ui, panel, &root, 0, &mut actions);
                        } else {
                            search_rows(ui, panel, &root, &mut actions);
                        }
                    });
            });

        for a in actions {
            match a {
                FileAction::Open(path) => self.open_file(path),
                FileAction::Toggle(dir) => {
                    if !self.files_panel.expanded.remove(&dir) {
                        self.files_panel.expanded.insert(dir);
                    }
                }
                FileAction::Select(path) => self.files_panel.selected = Some(path),
                FileAction::SetRoot(dir) => {
                    self.config.files_root = Some(dir);
                    self.files_panel.listings.clear();
                    self.files_panel.search = None;
                    self.save_config();
                }
                FileAction::Focus(id) => {
                    if let Some(i) = self.tabs.iter().position(|t| t.ds_id == id) {
                        self.select_tab_by_id(self.tabs[i].id);
                    }
                }
                FileAction::Close(id) => self.close_file(&id),
                FileAction::Refresh => {
                    self.files_panel.listings.clear();
                    self.files_panel.search = None;
                }
                FileAction::Hide => {
                    self.apply_tool_window_action(super::actions::ActionId::Files, ui.ctx())
                }
            }
        }
    }
}

/// Starts a search when the query changed and picks up finished results.
fn poll_search(panel: &mut FilesPanel, root: &Path, ctx: &egui::Context) {
    let query = panel.query.trim().to_string();
    if query.is_empty() {
        panel.search = None;
        return;
    }
    let stale = panel
        .search
        .as_ref()
        .is_none_or(|s| s.query != query || s.root != root);
    if stale {
        let (tx, rx) = channel();
        let (q, r, ctx) = (query.clone(), root.to_path_buf(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(files::search(&r, &q, SEARCH_LIMIT));
            ctx.request_repaint();
        });
        panel.search = Some(Search {
            query,
            root: root.to_path_buf(),
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
    panel: &FilesPanel,
    path: &Path,
    depth: usize,
    detail: &str,
    actions: &mut Vec<FileAction>,
) {
    let Some(kind) = files::kind_of(path) else {
        return;
    };
    let selected = panel.selected.as_deref() == Some(path);
    let r = tree_row(
        ui,
        depth,
        None,
        TreeIcon::Kind(kind_icon(kind)),
        &file_name(path),
        detail,
        theme::TEXT_MUTED,
        selected,
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

/// Lazily loaded folder tree with only supported files.
fn dir_rows(
    ui: &mut Ui,
    panel: &mut FilesPanel,
    dir: &Path,
    depth: usize,
    actions: &mut Vec<FileAction>,
) {
    let (dirs, files) = panel
        .listings
        .entry(dir.to_path_buf())
        .or_insert_with(|| files::list_dir(dir).unwrap_or_default())
        .clone();
    if depth == 0 && dirs.is_empty() && files.is_empty() {
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
    for sub in &dirs {
        let open = panel.expanded.contains(sub);
        let r = tree_row(
            ui,
            depth,
            Some(open),
            TreeIcon::Kind(ObjectKind::Folder),
            &file_name(sub),
            "",
            theme::TEXT_MUTED,
            false,
        );
        if r.double_clicked() || (r.clicked() && arrow_hit(&r)) {
            actions.push(FileAction::Toggle(sub.clone()));
        }
        r.context_menu(|ui| {
            if ui.button("Use as Root Folder").clicked() {
                actions.push(FileAction::SetRoot(sub.clone()));
                ui.close();
            }
        });
        if open {
            dir_rows(ui, panel, sub, depth + 1, actions);
        }
    }
    for f in &files {
        file_row(ui, panel, f, depth, &human_size(f), actions);
    }
}

fn search_rows(ui: &mut Ui, panel: &FilesPanel, root: &Path, actions: &mut Vec<FileAction>) {
    let Some(search) = &panel.search else { return };
    let Some(results) = &search.results else {
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
    for path in results {
        let folder = path
            .parent()
            .and_then(|p| p.strip_prefix(root).ok())
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        file_row(ui, panel, path, 0, &folder, actions);
    }
    if results.len() >= SEARCH_LIMIT {
        ui.label(
            RichText::new(format!("First {SEARCH_LIMIT} matches shown"))
                .small()
                .color(theme::TEXT_MUTED),
        );
    }
}
