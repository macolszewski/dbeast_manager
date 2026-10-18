mod actions;
mod activity;
mod about;
mod appearance;
pub mod branding;
mod compare;
mod compare_actions;
mod console;
mod ddl_diff;
mod ddl_diff_actions;
mod dialog;
mod execution_context;
mod explain;
mod dump_dialog;
mod export_dialog;
mod import_dialog;
mod explorer;
mod files_panel;
mod highlight;
mod json_view;
mod map;
mod mcp;
mod query_plan;
mod related;
mod related_execution;
mod result_editing;

pub use map::OSM_TILES;
mod results;
mod scene3d;
mod schema_graph;
mod schema_graph_actions;
mod scope_chooser;
mod session;
mod tabs;
mod theme;
mod tool_windows;
mod transfers;
mod zoom;

use crate::backend::{Backend, Event, RunRequest};
use crate::completion::Item;
use crate::config::{Config, SavedConsole};
use crate::drivers::select_all;
use crate::files::OpenedFile;
use crate::model::{
    CompletionMetadata, DataSource, DbObject, DriverKind, ExecutionContext, MetadataKey,
    ObjectKind, Page, QueryResult, ResolvedContext, SchemaSelection,
};
use crate::splitter;
use actions::ActionId;
use dialog::{DataSourceDialog, DialogAction};
use egui::{Align, Key, KeyboardShortcut, Layout, Modifiers, RichText, Ui};
use results::{GridAction, LogKind, LogLine, ResultSet};
use std::collections::{HashMap, HashSet};
use std::ops::Range;

const RUN_STATEMENT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Enter);
const RUN_SCRIPT: KeyboardShortcut =
    KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::Enter);
const STOP: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::F2);
const COMPLETE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::CTRL, Key::Space);

#[derive(Default)]
enum ConnStatus {
    #[default]
    Disconnected,
    Connecting,
    Connected(String),
    Failed(String),
}

#[derive(Default)]
struct SourceState {
    status: ConnStatus,
    roots: Option<Vec<DbObject>>,
    loading: HashSet<Vec<String>>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
enum TabKind {
    Console {
        sql: String,
    },
    Table {
        schema: Option<String>,
        table: String,
        filter: String,
        order: String,
        /// SQLite table without a primary key: its rowid is selected so rows
        /// have a stable identity for editing.
        #[serde(default)]
        select_rowid: bool,
    },
}

#[derive(Clone, Copy, PartialEq)]
enum ServicesView {
    Output,
    Result(usize),
    Plan,
}

/// What the running request is for, which decides where its results go.
#[derive(Clone, Copy, PartialEq)]
enum RunKind {
    Query,
    /// Another page of the result set with this index.
    Page(usize),
    /// Grid edits use stable result and request identities, not tab positions.
    Edits {
        result_id: u64,
        request_id: u64,
    },
    Explain {
        mode: crate::query_plan::ExplainMode,
        request_id: u64,
        context_revision: u64,
    },
    Transaction,
}

/// One-line message in the status bar, e.g. the outcome of an export.
struct ServicesNotice {
    text: String,
    error: bool,
}

impl ServicesNotice {
    fn info(text: String) -> Self {
        Self { text, error: false }
    }

    fn error(text: String) -> Self {
        Self { text, error: true }
    }
}

/// Text field of a tab that offers code completion.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Field {
    Console,
    Where,
    OrderBy,
}

/// Open list of recent filters under a table editor field.
struct HistoryPopup {
    field: Field,
    selected: usize,
}

/// Open completion popup; only one field of a tab can show it at a time.
struct CompletionPopup {
    field: Field,
    items: Vec<Item>,
    /// Byte range in the field's text that the chosen item replaces.
    replace: Range<usize>,
    selected: usize,
}

struct Tab {
    id: u64,
    title: String,
    ds_id: String,
    kind: TabKind,
    bound_query: Option<std::sync::Arc<crate::model::CapturedRelatedQuery>>,
    results: Vec<ResultSet>,
    plan: Option<query_plan::PlanView>,
    log: results::OutputLog,
    view: ServicesView,
    running: Option<RunKind>,
    running_origin: Option<results::ResultOrigin>,
    /// A table tab restored from the session shows its saved rows without
    /// connecting. The first attempt to edit them reloads the page so edits
    /// use fresh data and metadata.
    refresh_on_activate: bool,
    /// Last known caret/selection as char offsets.
    cursor: Option<Range<usize>>,
    manual_tx: bool,
    in_tx: bool,
    completion: Option<CompletionPopup>,
    filter_history: Option<HistoryPopup>,
    /// Transposed grid view, kept for the tab's next results.
    transposed: bool,
    record_view: bool,
    aggregate_view: bool,
    show_value_editor: bool,
    /// A statement changed data since the opened file was last saved.
    unsaved_writes: bool,
    /// A file write is in progress. Its owner must remain until FileSaved.
    saving_file: bool,
    /// Committed changes await a serialized write for this opened file.
    file_save_queued: bool,
    /// Folded byte ranges of the console text, sorted and not overlapping.
    folds: Vec<Range<usize>>,
    search: console::SearchState,
    editor_history: console::EditorHistory,
    editor_cache: console::EditorCache,
    /// Server database the tab runs in (PostgreSQL), when not the data source's default.
    database: Option<String>,
    schema: SchemaSelection,
    context_revision: u64,
    context_pending: Option<ExecutionContext>,
    resolved_context: Option<ResolvedContext>,
    metadata_key: MetadataKey,
    context_ui: console::ContextUiState,
}

impl Tab {
    fn log(&mut self, kind: LogKind, text: impl Into<String>) {
        self.log.push(LogLine {
            time: chrono::Local::now(),
            kind,
            text: text.into(),
        });
    }

    fn is_running(&self) -> bool {
        self.running.is_some()
    }
}

enum TreeAction {
    Toggle(String, DbObject),
    Select(String, Vec<String>),
    OpenTable(String, DbObject),
    /// New console on a data source, optionally in one of its databases.
    NewConsole(String, Option<String>),
    /// Opens the database and schema chooser of a data source.
    ChooseScope(String),
    Refresh(String),
    RefreshNode(String, DbObject),
    Disconnect(String),
    Properties(String),
    CopyName(String),
    Ddl(String, DbObject),
    CompareStructure(String, DbObject),
    CloseFile(String),
    SelectGroup(Vec<String>),
    ToggleGroup(Vec<String>),
    /// Moves a data source into a group ("" is the top level).
    MoveToGroup(String, String),
    DropSource(String, explorer::groups::SourceDrop),
    GroupDialog(explorer::groups::GroupDialogMode),
    RemoveGroup(String),
    /// Sets standing MCP access for a database or table.
    /// `None` audience is every agent connection.
    ShareWithAgents(mcp::ShareTarget, Option<String>, mcp::ShareLevel),
    McpSharingSettings,
    /// Database Graph of a data source, database or schema.
    DatabaseGraph(String, Option<DbObject>),
    /// An Import/Export entry on a data source, database, schema or table.
    Transfer(String, Option<DbObject>, export_dialog::TransferEntry),
    /// Activity Monitor of a PostgreSQL data source, in one of its databases.
    ActivityMonitor(String, Option<String>),
}

pub struct App {
    branding: branding::Branding,
    startup_splash: branding::StartupSplash,
    /// The window has no native title bar; the main toolbar moves the window
    /// and leaves room for the macOS traffic lights.
    integrated_titlebar: bool,
    backend: Backend,
    config: Config,
    sources: HashMap<String, SourceState>,
    /// Tables and columns per data source, for code completion.
    meta: HashMap<MetadataKey, CompletionMetadata>,
    metadata_loading: HashMap<MetadataKey, u64>,
    metadata_request_id: u64,
    metadata_errors: HashMap<MetadataKey, String>,
    expanded: HashSet<(String, Vec<String>)>,
    selected: Option<(String, Vec<String>)>,
    explorer_ui: explorer::ExplorerState,
    tabs: Vec<Tab>,
    active: usize,
    next_id: u64,
    dialog: Option<DataSourceDialog>,
    group_dialog: Option<explorer::groups::GroupDialog>,
    config_loaded: bool,
    config_loading_error: Option<String>,
    config_apply_pending: Option<(bool, Option<String>)>,
    config_dirty: bool,
    config_saves_pending: usize,
    closing: bool,
    close_ready: bool,
    filter: String,
    show_explorer: bool,
    show_services: bool,
    show_files: bool,
    /// Run tool window with background transfers; it replaces Services.
    show_run: bool,
    transfers: transfers::Transfers,
    export_dialog: Option<export_dialog::ExportDialog>,
    import_dialog: Option<import_dialog::ImportDialog>,
    dump_dialog: Option<dump_dialog::DumpDialog>,
    activity: Vec<activity::ActivityMonitor>,
    activity_next_id: u64,
    /// Request ids of transfer dialog previews and table lists.
    transfer_request: u64,
    files_panel: files_panel::FilesPanel,
    /// Files opened in this session; their data sources are not saved.
    files: Vec<OpenedFile>,
    notice: Option<ServicesNotice>,
    /// Base map tiles of the Geo Viewer.
    tiles: map::TileCache,
    /// Data source whose database and schema chooser is open.
    scope_chooser: Option<String>,
    session_writer: session::SessionWriter,
    find_action: actions::FindAction,
    appearance_dialog: Option<appearance::AppearanceDialog>,
    about: about::About,
    related_rows: related::RelatedRowsState,
    compare: compare::CompareState,
    structure_compare: ddl_diff::StructureCompareState,
    schema_graph: schema_graph::GraphWindow,
    /// Saved Database Graph layouts by data source and database.
    graph_layouts: std::collections::BTreeMap<String, schema_graph::SavedGraphLayout>,
    pending_close: Option<tabs::CloseRequest>,
    reveal_active_tab: bool,
    pending_editor_focus: bool,
    pending_action_focus: Option<egui::Id>,
    mcp: mcp::McpUi,
}

impl App {
    /// Keep the native launch artwork readable, with immediate user dismissal.
    pub fn with_startup_splash(mut self) -> Self {
        self.startup_splash = branding::StartupSplash::new(0.8);
        self
    }

    pub fn with_integrated_titlebar(mut self, integrated: bool) -> Self {
        self.integrated_titlebar = integrated;
        self
    }

    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        Self::new_with_store(cc, Box::<crate::credentials::SystemStore>::default(), false)
    }

    /// Test fixtures explicitly inject an isolated store; config overrides never select one.
    pub fn new_for_test(cc: &eframe::CreationContext<'_>) -> Self {
        assert!(
            std::env::var_os("DBEAST_CONFIG").is_some(),
            "test config path required"
        );
        Self::new_with_store(cc, Box::new(crate::credentials::fixture_store()), true)
    }

    fn new_with_store(
        cc: &eframe::CreationContext<'_>,
        store: Box<dyn crate::credentials::CredentialStore>,
        synchronous: bool,
    ) -> Self {
        theme::apply(&cc.egui_ctx);
        let mut backend = Backend::new(cc.egui_ctx.clone());
        let initial = backend.start_persistence(store, synchronous);
        let config = Config::default();
        // Temporary databases of files opened in an earlier run that did not exit cleanly.
        let _ = std::fs::remove_dir_all(Config::data_dir().join("open-files"));
        let tiles = map::TileCache::new(config.tile_url.clone(), Config::data_dir().join("tiles"));
        let mut app = Self {
            branding: Default::default(),
            startup_splash: branding::StartupSplash::new(0.0),
            integrated_titlebar: false,
            backend,
            config,
            sources: HashMap::new(),
            meta: HashMap::new(),
            metadata_loading: HashMap::new(),
            metadata_request_id: 0,
            metadata_errors: HashMap::new(),
            expanded: HashSet::new(),
            selected: None,
            explorer_ui: Default::default(),
            tabs: Vec::new(),
            active: 0,
            next_id: 1,
            dialog: None,
            group_dialog: None,
            config_loaded: false,
            config_loading_error: None,
            config_apply_pending: None,
            config_dirty: false,
            config_saves_pending: 0,
            closing: false,
            close_ready: false,
            filter: String::new(),
            show_explorer: true,
            show_services: true,
            show_files: false,
            show_run: false,
            transfers: Default::default(),
            export_dialog: None,
            import_dialog: None,
            dump_dialog: None,
            activity: Vec::new(),
            activity_next_id: 0,
            transfer_request: 0,
            files_panel: files_panel::FilesPanel::default(),
            files: Vec::new(),
            notice: None,
            tiles,
            scope_chooser: None,
            session_writer: Default::default(),
            find_action: Default::default(),
            appearance_dialog: None,
            about: about::About::new(!synchronous),
            related_rows: Default::default(),
            compare: Default::default(),
            structure_compare: Default::default(),
            schema_graph: Default::default(),
            graph_layouts: Default::default(),
            pending_close: None,
            reveal_active_tab: false,
            pending_editor_focus: false,
            pending_action_focus: None,
            mcp: Default::default(),
        };
        if let Some(initial) = initial {
            app.loaded_config(initial);
        }
        app
    }

    fn loaded_config(&mut self, result: Result<crate::persistence::Loaded, String>) {
        let discard = self
            .dialog
            .as_ref()
            .is_some_and(|dialog| dialog.reload_discard);
        let result = result.and_then(|loaded| {
            if discard
                && loaded
                    .config
                    .data_sources
                    .iter()
                    .any(|source| source.credentials_unavailable)
            {
                Err(loaded.warning.unwrap_or_else(|| {
                    "Saved passwords could not be loaded. Unsaved changes were kept.".into()
                }))
            } else {
                Ok(loaded)
            }
        });
        match result {
            Err(error) => {
                self.config_loading_error = Some(error.clone());
                if let Some(dialog) = &mut self.dialog {
                    dialog.busy = false;
                    dialog.reload_discard = false;
                    dialog.error = Some(error);
                }
            }
            Ok(loaded) => {
                let first = !self.config_loaded;
                if first {
                    self.tiles = map::TileCache::new(
                        loaded.config.tile_url.clone(),
                        Config::data_dir().join("tiles"),
                    );
                    self.config = loaded.config;
                    self.backend.ctx().set_zoom_factor(zoom::clamp(self.config.ui_zoom));
                    theme::set_appearance(self.backend.ctx(), &self.config.appearance);
                } else {
                    let changed: Vec<_> = self
                        .config
                        .data_sources
                        .iter()
                        .filter(|old| {
                            loaded.config.data_sources.iter().find(|s| s.id == old.id) != Some(*old)
                        })
                        .map(|source| source.id.clone())
                        .collect();
                    for id in changed {
                        self.disconnect(&id);
                    }
                    self.config.data_sources = loaded.config.data_sources;
                    self.config.credential_namespace = loaded.config.credential_namespace;
                    self.config.credential_vault_version = loaded.config.credential_vault_version;
                    self.config.credential_vault_cleanup = loaded.config.credential_vault_cleanup;
                    self.config.credential_cleanup = loaded.config.credential_cleanup;
                    for index in 0..self.tabs.len() {
                        self.update_metadata_key(index);
                    }
                    self.explorer_ui.invalidate();
                }
                self.config_loaded = true;
                self.config_loading_error = None;
                if let Some(warning) = loaded.warning {
                    self.notice = Some(ServicesNotice::error(warning));
                }
                if first {
                    self.load_mcp_state();
                    match session::load() {
                        Some(saved) => self.restore_session(saved),
                        None => {
                            for console in self.config.consoles.clone() {
                                let i = self.add_tab(
                                    console.title,
                                    console.ds_id,
                                    TabKind::Console { sql: console.sql },
                                );
                                self.tabs[i].database = console.database;
                                self.tabs[i].schema = console.schema;
                                self.update_metadata_key(i);
                            }
                            self.active = 0;
                        }
                    }
                }
                if let Some(dialog) = &mut self.dialog {
                    if discard {
                        let selected = dialog
                            .selected
                            .and_then(|i| dialog.sources.get(i))
                            .map(|s| s.id.clone());
                        *dialog = DataSourceDialog::new(
                            self.config.data_sources.clone(),
                            selected.as_deref(),
                        );
                    }
                    for draft in &mut dialog.sources {
                        if draft.credentials_unavailable
                            && let Some(source) = self.config.data_sources.iter().find(|source| {
                                source.id == draft.id && source.identity() == draft.identity()
                            })
                        {
                            crate::credentials::Secrets::from_source(source).apply(draft);
                            draft.credential_ref = source.credential_ref.clone();
                            draft.credentials_unavailable = source.credentials_unavailable;
                        }
                    }
                    dialog.busy = false;
                    dialog.error = None;
                }
            }
        }
        self.refresh_mcp_resources();
    }

    fn source(&self, id: &str) -> Option<&DataSource> {
        find_source(&self.config, &self.files, id)
    }

    fn save_config(&mut self) {
        self.config.consoles = self
            .tabs
            .iter()
            .filter(|t| !t.ds_id.starts_with("file:") && t.bound_query.is_none())
            .filter_map(|t| match &t.kind {
                TabKind::Console { sql } => Some(SavedConsole {
                    title: t.title.clone(),
                    ds_id: t.ds_id.clone(),
                    sql: sql.clone(),
                    database: t.database.clone(),
                    schema: t.schema.clone(),
                }),
                TabKind::Table { .. } => None,
            })
            .collect();
        if !self.config_loaded {
            return;
        }
        if self.config_apply_pending.is_some() {
            self.config_dirty = true;
            return;
        }
        self.config_saves_pending += 1;
        self.backend.save_config(self.config.clone(), false);
    }

    fn add_tab(&mut self, title: String, ds_id: String, kind: TabKind) -> usize {
        self.related_rows.cancel();
        self.explorer_ui.cancel_reveal();
        self.stage_active_cell();
        let id = self.next_id;
        self.next_id += 1;
        let metadata_key = self
            .source(&ds_id)
            .map(MetadataKey::for_source)
            .unwrap_or_else(|| MetadataKey {
                ds_id: ds_id.clone(),
                database: None,
            });
        self.tabs.push(Tab {
            id,
            title,
            ds_id,
            kind,
            bound_query: None,
            refresh_on_activate: false,
            results: Vec::new(),
            plan: None,
            log: Default::default(),
            view: ServicesView::Output,
            running: None,
            running_origin: None,
            cursor: None,
            manual_tx: false,
            in_tx: false,
            completion: None,
            filter_history: None,
            transposed: false,
            record_view: false,
            aggregate_view: false,
            show_value_editor: true,
            unsaved_writes: false,
            saving_file: false,
            file_save_queued: false,
            folds: Vec::new(),
            search: Default::default(),
            editor_history: Default::default(),
            editor_cache: Default::default(),
            database: None,
            schema: SchemaSelection::Default,
            context_revision: 0,
            context_pending: None,
            resolved_context: None,
            metadata_key,
            context_ui: Default::default(),
        });
        self.active = self.tabs.len() - 1;
        self.reveal_active_tab = true;
        self.pending_editor_focus = true;
        self.active
    }

    fn new_console(&mut self, ds_id: String) {
        self.open_console(ds_id, None, String::new());
    }

    fn new_console_in(&mut self, ds_id: String, database: Option<String>) {
        self.open_console(ds_id, None, String::new());
        if let Some(tab) = self.tabs.last_mut() {
            tab.database = database;
        }
        self.update_metadata_key(self.active);
        self.ensure_console_metadata(self.active);
        self.save_config();
    }

    fn open_console(&mut self, ds_id: String, title: Option<String>, sql: String) {
        let title = title.unwrap_or_else(|| {
            let n = self
                .tabs
                .iter()
                .filter(|t| t.ds_id == ds_id && matches!(t.kind, TabKind::Console { .. }))
                .count();
            if n == 0 {
                "console".to_string()
            } else {
                format!("console_{n}")
            }
        });
        self.add_tab(title, ds_id, TabKind::Console { sql });
        self.save_config();
    }

    fn connect(&mut self, ds_id: &str) {
        let Some(ds) = self.source(ds_id).cloned() else {
            return;
        };
        if ds.credentials_unavailable {
            self.notice = Some(ServicesNotice::error("Passwords are unavailable. Open Data Sources and Drivers to retry or replace them.".into()));
            return;
        }
        let st = self.sources.entry(ds.id.clone()).or_default();
        if matches!(st.status, ConnStatus::Connecting) {
            return;
        }
        st.status = ConnStatus::Connecting;
        st.roots = None;
        st.loading.clear();
        st.loading.insert(Vec::new());
        self.expanded.insert((ds_id.to_owned(), Vec::new()));
        self.explorer_ui.invalidate();
        self.invalidate_source_metadata(ds_id);
        let request_id = self.begin_metadata_request(MetadataKey::for_source(&ds));
        self.backend.connect(ds, request_id);
    }

    fn disconnect(&mut self, ds_id: &str) {
        self.explorer_ui.cancel_reveal_for_source(ds_id);
        self.explorer_ui.invalidate();
        self.backend.disconnect(ds_id);
        self.invalidate_source_metadata(ds_id);
        self.sources.remove(ds_id);
        self.expanded.retain(|(id, _)| id != ds_id);
        for t in self.tabs.iter_mut().filter(|t| t.ds_id == ds_id) {
            if matches!(t.running, Some(RunKind::Explain { .. })) {
                self.backend.cancel(t.id);
            }
            t.in_tx = false;
            t.context_revision = t.context_revision.wrapping_add(1);
            t.context_pending = None;
            t.resolved_context = None;
            t.context_ui.invalidate();
        }
    }

    // ---------------------------------------------------------------- events

    fn handle_events(&mut self) {
        while let Ok(ev) = self.backend.rx.try_recv() {
            match ev {
                Event::McpChanged => self.mcp_changed(),
                Event::ConfigLoaded(result) => self.loaded_config(result),
                Event::McpStateLoaded(result) => self.mcp_state_loaded(result),
                Event::McpStateSaved(result) => {
                    if let Err(error) = result {
                        self.mcp_save_failed(error);
                    }
                }
                Event::ConfigSaved { apply, result } => {
                    self.config_saves_pending = self.config_saves_pending.saturating_sub(1);
                    if result.is_err() {
                        self.closing = false;
                    }
                    if apply {
                        self.finish_config_apply(result);
                    } else if let Err(error) = result {
                        self.notice = Some(ServicesNotice::error(error));
                    } else if let Ok(loaded) = result
                        && let Some(warning) = loaded.warning
                    {
                        self.notice = Some(ServicesNotice::error(warning));
                    }
                }
                Event::TestDone {
                    ds_id,
                    request_id,
                    result,
                } => {
                    if let Some(d) = &mut self.dialog {
                        d.finish_test(&ds_id, &request_id, result);
                    }
                }
                Event::Connected { ds_id, result } => {
                    // A late reply must not recreate a disconnected source.
                    let Some(st) = self.sources.get_mut(&ds_id) else {
                        continue;
                    };
                    if result.is_err() {
                        self.explorer_ui.cancel_reveal_for_source(&ds_id);
                        st.loading.remove(&Vec::new());
                    }
                    st.status = match result {
                        Ok(info) => ConnStatus::Connected(info),
                        Err(e) => ConnStatus::Failed(e),
                    };
                    self.explorer_ui.invalidate();
                }
                Event::Children {
                    ds_id,
                    path,
                    result,
                } => self.children_loaded(ds_id, path, result),
                Event::StatementDone {
                    console_id,
                    sql,
                    page,
                    result,
                } => self.statement_done(console_id, sql, page, result),
                Event::Notice { console_id, text } => {
                    if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == console_id) {
                        tab.log(LogKind::Info, text);
                    }
                }
                Event::RunFinished {
                    console_id,
                    in_tx,
                } => self.run_finished(console_id, in_tx),
                Event::StructureCompareDone { request_id, result } => {
                    self.structure_compared(request_id, result.map(|diff| *diff));
                }
                Event::SchemaGraph { request_id, result } => {
                    self.schema_graph.loaded(request_id, result);
                }
                Event::SchemaGraphLayout {
                    request_id,
                    flat,
                    layout,
                } => {
                    self.schema_graph.relaid(request_id, flat, layout);
                }
                Event::CompareDataDone { request_id, result } => {
                    self.compare.done(request_id, result);
                }
                Event::ExplainDone {
                    console_id,
                    request_id,
                    completion,
                    result,
                } => self.explain_done(console_id, request_id, completion, result),
                Event::Completion {
                    key,
                    request_id,
                    result,
                } => self.completion_loaded(key, request_id, result),
                Event::SchemaChanged {
                    console_id,
                    ds_id,
                    revision,
                    requested,
                    result,
                } => {
                    self.schema_changed(console_id, ds_id, revision, requested, result);
                }
                Event::ResultMetadata {
                    console_id,
                    result_id,
                    request_id,
                    result,
                } => {
                    self.result_metadata_loaded(console_id, result_id, request_id, result);
                }
                Event::RelatedMetadata {
                    console_id,
                    result_id,
                    request_id,
                    direction,
                    result,
                } => {
                    self.related_metadata_loaded(
                        console_id, result_id, request_id, direction, result,
                    );
                }
                Event::UpdatesDone {
                    console_id,
                    result_id,
                    request_id,
                    in_tx,
                    result,
                } => {
                    self.result_edits_done(console_id, result_id, request_id, in_tx, result);
                }
                Event::Ddl {
                    ds_id,
                    title,
                    result,
                } => match result {
                    Ok(sql) => self.open_console(ds_id, Some(format!("{title}.sql")), sql),
                    Err(e) => {
                        self.notice = Some(ServicesNotice::error(format!("DDL for {title}: {e}")))
                    }
                },
                Event::FileOpened { path, result } => self.file_opened(path, result),
                Event::FileSaved { console_id, result } => {
                    if let Some(source) = self.complete_file_save(console_id, result) {
                        self.save_next_file(&source);
                    }
                }
                Event::Task { id, event } => self.transfer_event(id, event),
                Event::TransferPreview { request, result } => {
                    self.export_preview_loaded(request, result);
                }
                Event::TransferTables { request, result } => {
                    if !self.import_tables_loaded(request, &result) {
                        self.export_tables_loaded(request, result);
                    }
                }
                Event::ImportPreview { request, result } => self.import_preview_loaded(request, result),
                Event::ImportColumns { request, result } => self.import_columns_loaded(request, result),
                Event::ToolVersion { request, result } => self.tool_version_loaded(request, result),
                Event::ActivitySample { monitor, request, result } => self.activity_sample_loaded(monitor, request, result),
                Event::ActivitySignal { monitor, pid, terminate, result } => self.activity_signal_done(monitor, pid, terminate, result),
            }
        }
    }

    fn file_opened(&mut self, path: std::path::PathBuf, result: Result<Box<OpenedFile>, String>) {
        let opened = match result {
            Ok(f) => *f,
            Err(e) => {
                self.notice = Some(ServicesNotice::error(format!(
                    "cannot open {}: {e}",
                    path.display()
                )));
                return;
            }
        };
        let ds_id = opened.source.id.clone();
        let first_table = opened.tables.first().map(|t| t.table.clone());
        let kind = opened.kind;
        self.notice = Some(ServicesNotice::info(format!(
            "opened {} ({})",
            opened.origin.display(),
            kind.label()
        )));
        self.files.push(opened);
        self.refresh_mcp_resources();
        self.expanded.insert((ds_id.clone(), Vec::new()));
        self.connect(&ds_id);
        match first_table {
            // CSV and Data Package: show the (first) table right away.
            Some(table) => {
                let node = DbObject::new(ObjectKind::Table, table, &["main".to_string()]);
                self.open_table(ds_id, &node);
            }
            None => self.open_console(ds_id, None, String::new()),
        }
    }

    /// Opens a data file, or focuses it when it is already open.
    fn open_file(&mut self, path: std::path::PathBuf) {
        if let Some(f) = self.files.iter().find(|f| f.origin == path) {
            let id = f.source.id.clone();
            if let Some(i) = self.tabs.iter().position(|t| t.ds_id == id) {
                self.select_tab_by_id(self.tabs[i].id);
            }
            return;
        }
        let temp = crate::config::Config::data_dir().join("open-files");
        self.backend.open_file(path, temp);
    }

    fn close_file(&mut self, ds_id: &str) {
        self.request_close_file(ds_id);
    }

    /// Called only after the shared close request has removed all source tabs.
    fn cleanup_opened_file(&mut self, ds_id: &str) {
        debug_assert!(!self.tabs.iter().any(|tab| tab.ds_id == ds_id));
        self.disconnect(ds_id);
        self.invalidate_source_metadata(ds_id);
        if self.selected.as_ref().is_some_and(|(id, _)| id == ds_id) {
            self.selected = None;
        }
        if let Some(i) = self.files.iter().position(|f| f.source.id == ds_id) {
            self.mcp_unshare_source(ds_id);
            self.files.remove(i).close();
        }
        self.refresh_mcp_resources();
        self.explorer_ui.invalidate();
    }

    /// Loads nodes left expanded (from a restored session or before a
    /// reconnect) whose children are not known yet.
    fn load_expanded(&mut self, ds_id: &str) {
        fn walk(nodes: &[DbObject], want: &dyn Fn(&DbObject) -> bool, out: &mut Vec<DbObject>) {
            for n in nodes {
                if want(n) {
                    out.push(n.clone());
                }
                if let Some(c) = &n.children {
                    walk(c, want, out);
                }
            }
        }
        let Some(st) = self.sources.get(ds_id) else {
            return;
        };
        let want = |n: &DbObject| {
            n.children.is_none()
                && n.kind != ObjectKind::Folder
                && !st.loading.contains(&n.path)
                && self.expanded.contains(&(ds_id.to_string(), n.path.clone()))
        };
        let mut pending = Vec::new();
        walk(st.roots.as_deref().unwrap_or_default(), &want, &mut pending);
        for node in pending {
            self.load_node(ds_id, node);
        }
    }

    fn children_loaded(
        &mut self,
        ds_id: String,
        path: Option<Vec<String>>,
        result: Result<Vec<DbObject>, String>,
    ) {
        let Some(st) = self.sources.get_mut(&ds_id) else {
            return;
        };
        if path.is_none() {
            st.loading.remove(&Vec::new());
        }
        self.explorer_ui.invalidate();
        if result.is_err() {
            self.explorer_ui.cancel_reveal_for_source(&ds_id);
        }
        match (path, result) {
            (None, Ok(roots)) => {
                // Expand the first level so schemas are visible right away.
                let auto_open = roots
                    .iter()
                    .find(|r| {
                        r.kind == ObjectKind::Database
                            && (r.detail == crate::drivers::DEFAULT_DATABASE || roots.len() == 1)
                    })
                    .cloned();
                st.roots = Some(roots);
                if let Some(db) = auto_open
                    && self.expanded.contains(&(ds_id.clone(), Vec::new()))
                    && !self.expanded.contains(&(ds_id.clone(), db.path.clone()))
                {
                    self.tree_action(TreeAction::Toggle(ds_id.clone(), db));
                }
                self.load_expanded(&ds_id);
            }
            (Some(path), Ok(children)) => {
                st.loading.remove(&path);
                if let Some(node) = st
                    .roots
                    .as_mut()
                    .and_then(|r| r.iter_mut().find_map(|n| n.find_mut(&path)))
                {
                    node.expandable = !children.is_empty();
                    node.children = Some(children);
                }
                self.load_expanded(&ds_id);
            }
            (path, Err(e)) => {
                if let Some(p) = path {
                    st.loading.remove(&p);
                }
                st.status = ConnStatus::Failed(e);
            }
        }
    }

    fn statement_done(
        &mut self,
        console_id: u64,
        sql: String,
        page: Page,
        result: Result<QueryResult, String>,
    ) {
        let Some(tab_index) = self.tabs.iter().position(|t| t.id == console_id) else {
            return;
        };
        let tab = &mut self.tabs[tab_index];
        let kind = tab.running.unwrap_or(RunKind::Query);
        let r = match result {
            Ok(r) => r,
            Err(e) => {
                tab.log(LogKind::Error, e);
                tab.view = ServicesView::Output;
                return;
            }
        };
        if let Some(context) = &r.context {
            tab.resolved_context = Some(context.clone());
            tab.context_ui.invalidate_context();
        }
        let ms = r.elapsed.as_millis();
        if kind == RunKind::Query
            && tab.bound_query.is_none()
            && matches!(tab.kind, TabKind::Console { .. })
            && crate::backend::tx_effect(&sql).is_none()
        {
            self.config.remember(&tab.ds_id, &sql);
        }
        if !r.has_result_set() {
            if crate::backend::tx_effect(&sql).is_none()
                && self.files.iter().any(|file| {
                    file.source.id == tab.ds_id
                        && matches!(
                            file.kind,
                            crate::files::FileKind::Csv | crate::files::FileKind::DataPackage
                        )
                        && !file.tables.is_empty()
                })
            {
                tab.unsaved_writes = true;
            }
            let n = r.affected.unwrap_or(0);
            let text = if n > 0 {
                format!("{} affected in {ms} ms", rows_word(n as usize))
            } else {
                format!("completed in {ms} ms")
            };
            tab.log(LogKind::Info, text);
            return;
        }

        if kind == RunKind::Query
            && let TabKind::Table {
                schema,
                table,
                filter,
                order,
                ..
            } = &tab.kind
        {
            let key = table_key(schema.as_deref(), table);
            self.config
                .remember_filter(&tab.ds_id, &key, "WHERE", filter);
            self.config
                .remember_filter(&tab.ds_id, &key, "ORDER BY", order);
        }
        let n = r.rows.len();
        tab.log(
            LogKind::Info,
            format!(
                "{} retrieved starting from {} in {ms} ms",
                rows_word(n),
                page.skip + 1
            ),
        );
        if let RunKind::Page(idx) = kind
            && let Some(old) = tab.results.get_mut(idx)
        {
            let mut fresh = ResultSet::new(old.title.clone(), old.sql.clone(), r, page);
            // Related queries keep their bound origin; others belong to this run.
            fresh.origin = if old
                .origin
                .as_ref()
                .is_some_and(|origin| origin.bound_query.is_some())
            {
                old.origin.clone()
            } else {
                tab.running_origin.clone()
            };
            fresh.transposed = old.transposed;
            fresh.record_view = old.record_view;
            fresh.aggregate_view = old.aggregate_view;
            fresh.show_value_editor = old.show_value_editor;
            fresh.take_geo_from(old);
            fresh.take_find_from(old);
            fresh.take_local_filters_from(old);
            fresh.depth.carry_from(&mut old.depth);
            *old = fresh;
            self.check_result_editability(tab_index, idx);
            return;
        }
        let title = match &tab.kind {
            TabKind::Table { table, .. } => table.clone(),
            TabKind::Console { .. } => {
                guess_title(&sql).unwrap_or_else(|| format!("Result {}", tab.results.len() + 1))
            }
        };
        let mut rs = ResultSet::new(title, sql, r, page);
        rs.origin = tab.running_origin.clone();
        rs.transposed = tab.transposed;
        rs.record_view = tab.record_view;
        rs.aggregate_view = tab.aggregate_view;
        rs.show_value_editor = tab.show_value_editor;
        tab.results.push(rs);
        tab.view = ServicesView::Result(tab.results.len() - 1);
        let result_index = tab.results.len() - 1;
        self.check_result_editability(tab_index, result_index);
    }

    fn run_finished(&mut self, console_id: u64, in_tx: bool) {
        let Some(idx) = self.tabs.iter().position(|t| t.id == console_id) else {
            return;
        };
        let tab = &mut self.tabs[idx];
        tab.running = None;
        tab.running_origin = None;
        tab.in_tx = in_tx;
        self.save_opened_file(idx);
        self.save_config();
    }

    fn refresh_restored_table(&mut self) {
        let index = self.active;
        if self.config_loaded
            && self.tabs.get(index).is_some_and(|tab| {
                tab.refresh_on_activate
                    && tab.results.first().is_some_and(|rs| rs.edit_requested)
                    && !tab.is_running()
                    && tab.context_pending.is_none()
            })
        {
            let tab = &mut self.tabs[index];
            tab.refresh_on_activate = false;
            // Reload the saved page in place, keeping its view settings.
            match tab.results.first() {
                Some(result) => {
                    let (sql, page) = (result.sql.clone(), result.page);
                    self.start_run(index, vec![sql], page, RunKind::Page(0));
                }
                None => self.run_tab(index, false),
            }
        }
    }

    /// Writes CSV-backed tables back to their files once changes are committed.
    fn save_opened_file(&mut self, idx: usize) {
        let Some(source) = self.queue_file_save(idx) else {
            return;
        };
        self.save_next_file(&source);
    }

    fn queue_file_save(&mut self, idx: usize) -> Option<String> {
        let tab = self.tabs.get_mut(idx)?;
        if !tab.unsaved_writes || tab.in_tx || tab.saving_file {
            return None;
        }
        if !self.files.iter().any(|file| {
            file.source.id == tab.ds_id
                && matches!(
                    file.kind,
                    crate::files::FileKind::Csv | crate::files::FileKind::DataPackage
                )
                && !file.tables.is_empty()
        }) {
            return None;
        }
        tab.file_save_queued = true;
        Some(tab.ds_id.clone())
    }

    pub(super) fn source_is_saving(&self, source: &str) -> bool {
        self.tabs
            .iter()
            .any(|tab| tab.ds_id == source && tab.saving_file)
    }

    /// Selects and marks one queued writer without performing any disk work.
    fn begin_queued_file_save(
        &mut self,
        source: &str,
    ) -> Option<(u64, OpenedFile, Option<String>)> {
        if self.source_is_saving(source) {
            return None;
        }
        let opened = self.files.iter().find(|file| {
            file.source.id == source
                && matches!(
                    file.kind,
                    crate::files::FileKind::Csv | crate::files::FileKind::DataPackage
                )
                && !file.tables.is_empty()
        })?;
        let tab = self.tabs.iter_mut().find(|tab| {
            tab.ds_id == source
                && tab.file_save_queued
                && tab.unsaved_writes
                && !tab.in_tx
                && !tab.is_running()
        })?;
        tab.file_save_queued = false;
        tab.saving_file = true;
        // Keep dirty until success; a failure can be retried by the next explicit run.
        let table = match &tab.kind {
            TabKind::Table { table, .. } => Some(table.clone()),
            TabKind::Console { .. } => None,
        };
        Some((tab.id, opened.clone(), table))
    }

    fn save_next_file(&mut self, source: &str) {
        let Some((id, opened, table)) = self.begin_queued_file_save(source) else {
            return;
        };
        self.backend.save_file(id, opened, table);
    }

    /// A failed write stays dirty but is not automatically requeued. Otherwise
    /// two failing tabs could retry each other forever without user input.
    fn complete_file_save(&mut self, id: u64, result: Result<usize, String>) -> Option<String> {
        let tab = self
            .tabs
            .iter_mut()
            .find(|tab| tab.id == id && tab.saving_file)?;
        tab.saving_file = false;
        match result {
            Ok(rows) => {
                tab.unsaved_writes = false;
                tab.log(
                    LogKind::Info,
                    format!("{} written back to the file", rows_word(rows)),
                );
            }
            Err(error) => {
                tab.log(LogKind::Error, format!("saving the file failed: {error}"));
            }
        }
        Some(tab.ds_id.clone())
    }

    // ------------------------------------------------------------- execution

    fn start_run(&mut self, idx: usize, statements: Vec<String>, page: Page, kind: RunKind) {
        self.related_rows.cancel();
        if kind == RunKind::Query
            && self.tabs.get(idx).is_some_and(|tab| {
                tab.results
                    .iter()
                    .any(|result| result.action_state().has_changes)
            })
        {
            let reason = "Submit or revert pending grid changes before executing another query.";
            self.tabs[idx].log(LogKind::Info, reason);
            self.notice = Some(ServicesNotice::info(reason.into()));
            return;
        }
        if self
            .tabs
            .get(idx)
            .is_some_and(|tab| self.source_is_saving(&tab.ds_id))
        {
            self.tabs[idx].log(
                LogKind::Info,
                "wait for the data source file to finish saving",
            );
            return;
        }
        if let RunKind::Page(result) = kind
            && let Some(reason) = self
                .result_context_blocked_reason(idx, result)
                .or_else(|| self.result_replay_blocked_reason(idx, result))
        {
            self.tabs[idx].log(LogKind::Error, reason);
            self.notice = Some(ServicesNotice::error(reason.into()));
            return;
        }
        if matches!(kind, RunKind::Query | RunKind::Page(_)) {
            let query = match kind {
                RunKind::Page(result) => self
                    .tabs
                    .get(idx)
                    .and_then(|tab| tab.results.get(result))
                    .and_then(|result| result.origin.as_ref())
                    .and_then(|origin| origin.bound_query.clone()),
                _ => self.tabs.get(idx).and_then(|tab| tab.bound_query.clone()),
            };
            if let Some(query) = query {
                self.start_related_run(idx, query, page, kind, None);
                return;
            }
            if self
                .tabs
                .get(idx)
                .is_some_and(|tab| tab.bound_query.is_some())
            {
                self.notice = Some(ServicesNotice::error("The related query's parameters are unavailable. Reopen Related Rows from its source.".into()));
                return;
            }
        }
        self.ensure_console_metadata(idx);
        let Some(tab) = self.tabs.get_mut(idx) else {
            return;
        };
        let Some(ds) = find_source(&self.config, &self.files, &tab.ds_id).cloned() else {
            tab.log(LogKind::Error, "choose a data source for this console");
            return;
        };
        if tab.is_running() || tab.context_pending.is_some() || statements.is_empty() {
            return;
        }
        self.notice = None;
        let ds = match (&tab.database, ds.driver) {
            (Some(db), DriverKind::Postgres) => ds.with_database(db),
            _ => ds,
        };
        let prompt = match &tab.database {
            Some(db) if ds.driver == DriverKind::Postgres => format!("{}.{db}", ds.display_name()),
            _ => ds.display_name(),
        };
        if kind == RunKind::Query {
            tab.results.clear();
            tab.view = ServicesView::Output;
        }
        // Mirror the transaction statements the backend adds, so every result line has its SQL.
        let opens_tx = !tab.in_tx && tab.manual_tx && kind != RunKind::Transaction;
        if opens_tx {
            tab.log(
                LogKind::Sql,
                format!("{prompt}> {}", crate::drivers::begin_statement(ds.driver)),
            );
        }
        for s in &statements {
            tab.log(LogKind::Sql, format!("{prompt}> {s}"));
        }
        tab.running = Some(kind);
        tab.running_origin = Some(results::ResultOrigin {
            ds_id: tab.ds_id.clone(),
            source: Some(ds.identity()),
            context: ExecutionContext {
                database: tab.database.clone(),
                schema: tab.schema.clone(),
            },
            bound_query: None,
        });
        // The table editor's own SELECT is paged by the server; other
        // statements are read up to the page from the full result.
        let table_page = match (&tab.kind, kind) {
            (TabKind::Table { schema, table, .. }, RunKind::Query | RunKind::Page(_)) => {
                Some(crate::backend::TablePage {
                    schema: schema.clone(),
                    table: table.clone(),
                    // A result's SQL may predate an edit of the ORDER BY field.
                    ordered: statements.iter().any(|sql| sql.contains(" ORDER BY ")),
                })
            }
            _ => None,
        };
        let request = RunRequest {
            console_id: tab.id,
            ds,
            statements,
            page,
            manual_tx: tab.manual_tx && kind != RunKind::Transaction,
            context: ExecutionContext {
                database: tab.database.clone(),
                schema: tab.schema.clone(),
            },
            expected_context: match kind {
                RunKind::Page(result) => tab
                    .results
                    .get(result)
                    .and_then(|rs| rs.data.context.clone()),
                _ => None,
            },
        };
        match table_page {
            Some(table) => self.backend.run_table_page(request, table),
            None => self.backend.run(request),
        }
    }

    fn run_tab(&mut self, idx: usize, whole_script: bool) {
        let page = Page::first(self.config.max_rows);
        let Some(tab) = self.tabs.get(idx) else {
            return;
        };
        if tab.bound_query.is_some() {
            if !whole_script {
                self.start_run(idx, vec![String::new()], page, RunKind::Query);
            }
            return;
        }
        let Some(driver) = self.source(&tab.ds_id).map(|d| d.driver) else {
            self.start_run(idx, vec!["-".into()], page, RunKind::Query);
            return;
        };
        let statements: Vec<String> = match &tab.kind {
            TabKind::Console { sql } => {
                let cursor = tab.cursor.clone().unwrap_or(0..0);
                let byte = |c: usize| sql.char_indices().nth(c).map_or(sql.len(), |(b, _)| b);
                let texts = |v: Vec<splitter::Statement>| v.into_iter().map(|s| s.text).collect();
                if cursor.start != cursor.end {
                    texts(splitter::split(
                        &sql[byte(cursor.start)..byte(cursor.end)],
                        driver,
                    ))
                } else if whole_script {
                    texts(splitter::split(sql, driver))
                } else {
                    texts(
                        splitter::statement_at(sql, driver, byte(cursor.start))
                            .into_iter()
                            .collect(),
                    )
                }
            }
            TabKind::Table { .. } => vec![table_query(&tab.kind, driver)],
        };
        self.start_run(idx, statements, page, RunKind::Query);
    }

    /// Re-runs one result set's query from its first page.
    fn reload_result(&mut self, idx: usize, result: usize) {
        let Some(rs) = self.tabs.get(idx).and_then(|t| t.results.get(result)) else {
            return;
        };
        if matches!(self.tabs[idx].kind, TabKind::Table { .. }) {
            self.run_tab(idx, false);
        } else {
            let sql = rs.sql.clone();
            let page = Page::first(self.config.max_rows);
            self.start_run(idx, vec![sql], page, RunKind::Page(result));
        }
    }

    fn grid_action(&mut self, idx: usize, result: usize, action: GridAction) {
        match action {
            GridAction::Rerun => self.reload_result(idx, result),
            GridAction::Page(page) => {
                let sql = self.tabs[idx].results[result].sql.clone();
                self.start_run(idx, vec![sql], page, RunKind::Page(result));
            }
            GridAction::PageSize(size) => {
                self.config.max_rows = size;
                self.save_config();
                self.reload_result(idx, result);
            }
            GridAction::Submit => self.submit_edits(idx, result),
            GridAction::ExportData => self.export(idx, result),
            GridAction::RelatedRows => self.request_related_rows(idx, result),
            GridAction::CompareData => self.open_compare_data(idx, result),
        }
    }

    fn submit_edits(&mut self, idx: usize, result: usize) {
        self.submit_result_edits(idx, result);
    }

    fn export(&mut self, idx: usize, result: usize) {
        if let Some(reason) = self.result_replay_blocked_reason(idx, result) {
            self.tabs[idx].log(LogKind::Error, reason);
            self.notice = Some(ServicesNotice::error(reason.into()));
            return;
        }
        if self
            .tabs
            .get(idx)
            .is_some_and(|tab| tab.in_tx || tab.context_pending.is_some())
        {
            let reason =
                "Finish the transaction or schema change before exporting the full result.";
            self.tabs[idx].log(LogKind::Error, reason);
            self.notice = Some(ServicesNotice::error(reason.into()));
            return;
        }
        let tab = &self.tabs[idx];
        let rs = &tab.results[result];
        let source_id = rs
            .origin
            .as_ref()
            .map_or(tab.ds_id.as_str(), |origin| origin.ds_id.as_str());
        let Some(mut ds) = self.source(source_id).cloned() else {
            return;
        };
        let context = rs.data.context.clone();
        if ds.driver == DriverKind::Postgres
            && let Some(database) = context
                .as_ref()
                .and_then(|context| context.database.as_deref())
                .or(tab.database.as_deref())
        {
            ds = ds.with_database(database);
        }
        let source = export_dialog::ExportSource::Result {
            console_id: tab.id,
            ds,
            sql: rs.sql.clone(),
            context,
            related: rs.origin.as_ref().and_then(|origin| origin.bound_query.clone()),
            title: rs.title.clone(),
        };
        let preview = Some(rs.data.clone());
        self.open_export_dialog(source, preview);
    }

    fn end_transaction(&mut self, idx: usize, commit: bool) {
        let sql = if commit { "COMMIT" } else { "ROLLBACK" };
        self.start_run(
            idx,
            vec![sql.to_string()],
            Page::first(0),
            RunKind::Transaction,
        );
    }

    fn open_table(&mut self, ds_id: String, node: &DbObject) {
        let Some(ds) = self.source(&ds_id).cloned() else {
            return;
        };
        // Path is [db,] schema, table; SQLite's "main" schema is left implicit.
        let schema = match ds.driver {
            DriverKind::Sqlite
                if node.path.len() >= 2 && node.path[node.path.len() - 2] == "main" =>
            {
                None
            }
            _ => node.path.len().checked_sub(2).map(|i| node.path[i].clone()),
        };
        // PostgreSQL paths start with the database, which needs its own connection.
        let database = (ds.driver == DriverKind::Postgres)
            .then(|| node.path.first().cloned())
            .flatten();
        if let Some(i) = self.tabs.iter().position(|t| {
            t.ds_id == ds_id
                && t.database == database
                && matches!(&t.kind, TabKind::Table { table, schema: s, .. } if *table == node.name && *s == schema)
        }) {
            self.select_tab_by_id(self.tabs[i].id);
            return;
        }
        let kind = TabKind::Table {
            schema: schema.clone(),
            table: node.name.clone(),
            filter: String::new(),
            order: String::new(),
            select_rowid: false,
        };
        let idx = self.add_tab(node.name.clone(), ds_id, kind);
        self.tabs[idx].database = database;
        self.update_metadata_key(idx);
        self.run_tab(idx, false);
    }

    // ------------------------------------------------------------------ views

    fn open_data_sources(&mut self) {
        self.dialog = Some(DataSourceDialog::new(
            self.config.data_sources.clone(),
            None,
        ));
    }

    /// Main toolbar with the project widget.
    fn top_bar(&mut self, ui: &mut Ui) {
        // The traffic lights are native and ignore Zoom IDE, so the integrated
        // title bar keeps their size in screen points at every zoom.
        let native = if self.integrated_titlebar { 1.0 / ui.ctx().zoom_factor() } else { 1.0 };
        egui::Panel::top("main_toolbar")
            .exact_size((TOOLBAR_HEIGHT * native).max(30.0))
            .frame(
                egui::Frame::new()
                    .fill(theme::BG_PANEL)
                    .inner_margin(egui::Margin::symmetric(10, 0)),
            )
            .show(ui, |ui| {
                if self.integrated_titlebar {
                    // Registered first, so every widget drawn later wins the pointer.
                    let titlebar = ui.interact(
                        ui.max_rect(),
                        egui::Id::new("integrated_titlebar"),
                        egui::Sense::click_and_drag(),
                    );
                    if titlebar.drag_started_by(egui::PointerButton::Primary) {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
                    }
                    if titlebar.double_clicked() {
                        let maximized = ui.input(|i| i.viewport().maximized.unwrap_or(false));
                        ui.ctx()
                            .send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                    }
                }
                ui.horizontal_centered(|ui| {
                    // The traffic lights are hidden in full screen.
                    if self.integrated_titlebar
                        && !ui.input(|i| i.viewport().fullscreen.unwrap_or(false))
                    {
                        ui.add_space(TRAFFIC_LIGHTS_WIDTH * native);
                    }
                    self.branding.thumbnail(ui);
                    theme::tool_menu(
                        ui,
                        RichText::new("DBeast  ⏷").font(theme::semibold(13.0)),
                        "Project",
                        |ui| {
                            let context = self.capture_action_context(ui.ctx());
                            self.action_menu_item(ui, ActionId::About, &context);
                            ui.separator();
                            if ui
                                .add(
                                    egui::Button::new("Find Action…").shortcut_text(
                                        ui.ctx().format_shortcut(&actions::FIND_ACTION),
                                    ),
                                )
                                .clicked()
                            {
                                self.open_find_action(ui.ctx());
                                ui.close();
                            }
                            for action in [
                                ActionId::NewConsole,
                                ActionId::DataSources,
                                ActionId::McpServer,
                            ] {
                                self.action_menu_item(ui, action, &context);
                            }
                            ui.separator();
                            if ui.button("Open File…").clicked() {
                                if let Some(path) = files_panel::pick_file() {
                                    self.open_file(path);
                                }
                                ui.close();
                            }
                            ui.separator();
                            for action in [
                                ActionId::DatabaseExplorer,
                                ActionId::Files,
                                ActionId::Services,
                                ActionId::Run,
                            ] {
                                self.action_menu_item(ui, action, &context);
                            }
                            ui.separator();
                            self.action_menu_item(ui, ActionId::Appearance, &context);
                            ui.menu_button(format!("Zoom IDE ({:.0} %)", self.config.ui_zoom * 100.0), |ui| {
                                for action in [ActionId::ZoomIn, ActionId::ZoomOut, ActionId::ResetZoom] {
                                    self.action_menu_item(ui, action, &context);
                                }
                            });
                        },
                    );
                    if let Some(tab) = self.tabs.get(self.active)
                        && let Some(ds) = self.source(&tab.ds_id)
                    {
                        ui.add_space(8.0);
                        ui.label(
                            RichText::new(format!(
                                "{}  {}",
                                driver_badge(ds.driver),
                                ds.display_name()
                            ))
                            .color(theme::TEXT_MUTED),
                        );
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if theme::tool_button(ui, "MCP", "MCP Server settings").clicked() {
                            self.open_mcp_settings();
                        }
                        if theme::tool_button(
                            ui,
                            RichText::new("⚙").size(15.0),
                            "Data Sources and Drivers",
                        )
                        .clicked()
                        {
                            let context = self.capture_action_context(ui.ctx());
                            self.dispatch_action(ActionId::DataSources, &context, ui.ctx());
                        }
                    });
                });
            });
    }

    fn action_menu_item(
        &mut self,
        ui: &mut Ui,
        action: ActionId,
        context: &actions::ActionContext,
    ) {
        let definition = actions::action_definition(action);
        let availability = self.action_availability(action, context);
        let checked = match action {
            ActionId::DatabaseExplorer => self.show_explorer,
            ActionId::Files => self.show_files,
            ActionId::Services => self.show_services && !self.show_run,
            ActionId::Run => self.show_run,
            _ => false,
        };
        let mut button = egui::Button::new(definition.name).selected(checked);
        if let Some(shortcut) = definition.shortcut {
            button = button.shortcut_text(actions::format_action_shortcut(ui.ctx(), &shortcut));
        }
        let response = ui.add_enabled(availability.enabled, button);
        if response.clicked() {
            self.dispatch_action(action, context, ui.ctx());
            ui.close();
        } else if let Some(reason) = availability.reason {
            response.on_disabled_hover_text(reason);
        }
    }

    fn current_ds(&self) -> Option<String> {
        self.tabs
            .get(self.active)
            .map(|t| t.ds_id.clone())
            .or_else(|| self.selected.as_ref().map(|(id, _)| id.clone()))
            .or_else(|| self.config.data_sources.first().map(|d| d.id.clone()))
    }

    /// Narrow strip with tool window toggles: explorer at the top, Services at the bottom.
    fn tool_stripe(&mut self, ui: &mut Ui) {
        egui::Panel::left("stripe")
            .resizable(false)
            .exact_size(40.0)
            .frame(
                egui::Frame::new()
                    .fill(theme::BG_PANEL)
                    .inner_margin(egui::Margin::symmetric(4, 6)),
            )
            .show(ui, |ui| {
                // Tool window stripe: the open window has a neutral rounded
                // background, icons are light enough to read on the panel.
                let stripe_button = |ui: &mut Ui, icon: &str, on: bool, tip: &str| {
                    let (rect, response) = ui.allocate_exact_size(egui::vec2(30.0, 30.0), egui::Sense::click());
                    let fill = if on {
                        Some(theme::BG_TOGGLE_ON)
                    } else if response.hovered() {
                        Some(theme::BG_HOVER)
                    } else {
                        None
                    };
                    if let Some(fill) = fill {
                        ui.painter().rect_filled(rect, 6.0, fill);
                    }
                    let color = if on || response.hovered() { theme::TEXT } else { theme::ICON };
                    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, icon, egui::FontId::proportional(16.0), color);
                    response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, on, icon));
                    ui.add_space(4.0);
                    response.on_hover_text(tip).clicked()
                };
                ui.vertical_centered(|ui| {
                    // Both tool windows share the left side, so one replaces the other.
                    if stripe_button(ui, "🗄", self.show_explorer, "Database Explorer") {
                        self.apply_tool_window_action(ActionId::DatabaseExplorer, ui.ctx());
                    }
                    if stripe_button(ui, "🗀", self.show_files, "Files") {
                        self.apply_tool_window_action(ActionId::Files, ui.ctx());
                    }
                    ui.with_layout(Layout::bottom_up(Align::Center), |ui| {
                        // Services and Run share the bottom area, so one replaces the other.
                        if stripe_button(ui, "☰", self.show_services && !self.show_run, "Services") {
                            self.apply_tool_window_action(ActionId::Services, ui.ctx());
                        }
                        if stripe_button(ui, "▷", self.show_run, "Run") {
                            self.apply_tool_window_action(ActionId::Run, ui.ctx());
                        }
                    });
                });
            });
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        let mut run_intents = Vec::new();
        egui::Panel::bottom("status")
            .exact_size(26.0)
            .frame(
                egui::Frame::new()
                    .fill(theme::BG_PANEL)
                    .inner_margin(egui::Margin::symmetric(10, 0)),
            )
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    let crumb = |ui: &mut Ui, text: &str| {
                        ui.add(
                            egui::Label::new(RichText::new(text).small().color(theme::TEXT_MUTED))
                                .selectable(false),
                        );
                    };
                    let sep = |ui: &mut Ui| {
                        ui.label(RichText::new("›").small().color(theme::LINE_NUMBER));
                    };
                    crumb(ui, "DBeast");
                    let Some(tab) = self.tabs.get(self.active) else {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            transfers::indicator(ui, &self.transfers, &mut run_intents);
                        });
                        return;
                    };
                    if let Some(ds) = self.source(&tab.ds_id) {
                        sep(ui);
                        crumb(ui, &ds.display_name());
                    }
                    sep(ui);
                    crumb(ui, &tab.title);
                    if let Some(n) = &self.notice {
                        ui.add_space(12.0);
                        let color = if n.error { theme::RED } else { theme::GREEN };
                        ui.label(RichText::new(&n.text).small().color(color));
                        if theme::tool_button(ui, RichText::new("×").small(), "Dismiss").clicked()
                        {
                            self.notice = None;
                        }
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let tx = match (tab.manual_tx, tab.in_tx) {
                            (_, true) => RichText::new("Tx: open").color(theme::SQL_KEYWORD),
                            (true, false) => RichText::new("Tx: Manual").color(theme::TEXT_MUTED),
                            (false, false) => RichText::new("Tx: Auto").color(theme::TEXT_MUTED),
                        };
                        ui.label(tx.small());
                        ui.add_space(10.0);
                        if let TabKind::Console { sql } = &tab.kind {
                            ui.label(RichText::new("UTF-8").small().color(theme::TEXT_MUTED));
                            ui.add_space(10.0);
                            let offset = tab.cursor.as_ref().map_or(0, |c| c.start);
                            let (line, col) = tab
                                .editor_cache
                                .line_col(offset)
                                .unwrap_or_else(|| line_col(sql, offset));
                            ui.label(
                                RichText::new(format!("{line}:{col}"))
                                    .small()
                                    .color(theme::TEXT_MUTED),
                            );
                            ui.add_space(10.0);
                        }
                        let status = match self.sources.get(&tab.ds_id).map(|s| &s.status) {
                            Some(ConnStatus::Connected(info)) => {
                                RichText::new(info.split(" (").next().unwrap_or(info).to_string())
                                    .color(theme::TEXT_MUTED)
                            }
                            Some(ConnStatus::Failed(_)) => {
                                RichText::new("connection failed").color(theme::RED)
                            }
                            _ => RichText::new("not connected").color(theme::TEXT_MUTED),
                        };
                        ui.label(status.small());
                        if tab.is_running() {
                            ui.spinner();
                        }
                        ui.add_space(10.0);
                        transfers::indicator(ui, &self.transfers, &mut run_intents);
                    });
                });
            });
        self.apply_run_intents(run_intents);
    }

    fn services(&mut self, ui: &mut Ui) {
        let idx = self.active;
        let page_size = self.config.max_rows;
        if !self.show_services {
            return;
        }
        let Some(tab) = self.tabs.get_mut(idx) else {
            return;
        };
        if !matches!(tab.kind, TabKind::Console { .. }) {
            return;
        }
        let driver = find_source(&self.config, &self.files, &tab.ds_id)
            .map_or(DriverKind::Postgres, |d| d.driver);
        let mut action = None;
        let mut hide = false;
        egui::Panel::bottom("services")
            .resizable(true)
            .default_size(320.0)
            .min_size(120.0)
            .frame(
                egui::Frame::new()
                    .fill(theme::BG_PANEL)
                    .inner_margin(egui::Margin::symmetric(6, 0)),
            )
            .show(ui, |ui| {
                // "Hide" is placed first from the right; result tabs scroll instead of overlapping it.
                ui.interact(
                    ui.available_rect_before_wrap(),
                    tool_windows::services_focus_id(),
                    egui::Sense::focusable_noninteractive(),
                );
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), theme::HEADER_HEIGHT),
                    Layout::right_to_left(Align::Center),
                    |ui| {
                        hide = theme::tool_button(ui, "−", "Hide").clicked();
                        if tab.is_running() {
                            ui.spinner();
                        }
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            ui.label(RichText::new("Services").font(theme::semibold(13.0)));
                            ui.add_space(10.0);
                            egui::ScrollArea::horizontal()
                                .id_salt("services_tabs")
                                .scroll_bar_visibility(
                                    egui::scroll_area::ScrollBarVisibility::AlwaysHidden,
                                )
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        let output = tab.view == ServicesView::Output;
                                        if theme::flat_tab(
                                            ui,
                                            RichText::new("Output"),
                                            output,
                                            false,
                                        )
                                        .0
                                        .clicked()
                                        {
                                            tab.view = ServicesView::Output;
                                        }
                                        for (i, r) in tab.results.iter().enumerate() {
                                            let active = tab.view == ServicesView::Result(i);
                                            let label = RichText::new(format!("⊞ {}", r.title));
                                            if theme::flat_tab(ui, label, active, false).0.clicked()
                                            {
                                                tab.view = ServicesView::Result(i);
                                            }
                                        }
                                        if tab.plan.is_some()
                                            && theme::flat_tab(
                                                ui,
                                                RichText::new("Query Plan"),
                                                tab.view == ServicesView::Plan,
                                                false,
                                            )
                                            .0
                                            .clicked()
                                        {
                                            tab.view = ServicesView::Plan;
                                        }
                                    });
                                });
                        });
                    },
                );
                theme::hairline(ui);
                match tab.view {
                    ServicesView::Plan if tab.plan.is_some() => {
                        ui.push_id(tab.id, |ui| tab.plan.as_mut().unwrap().show(ui));
                    }
                    ServicesView::Result(i) if i < tab.results.len() => {
                        let editing_busy =
                            matches!(tab.running, Some(RunKind::Edits { .. }));
                        let rs = &mut tab.results[i];
                        let pageable = crate::result_edit::can_replay(&rs.sql, driver);
                        ui.add_enabled_ui(!editing_busy, |ui| {
                            if let Some(a) =
                                results::grid_toolbar(ui, rs, driver, page_size, pageable)
                            {
                                action = Some((i, a));
                            }
                            results::value_panel(ui, egui::Id::new("value_viewer"), rs);
                            results::grid(ui, rs, &mut self.tiles);
                        });
                        let transposed = rs.transposed;
                        tab.transposed = transposed;
                        tab.record_view = rs.record_view;
                        tab.aggregate_view = rs.aggregate_view;
                        tab.show_value_editor = rs.show_value_editor;
                    }
                    _ => {
                        egui::Frame::new()
                            .fill(theme::BG_EDITOR)
                            .inner_margin(6)
                            .show(ui, |ui| results::output(ui, &tab.log))
                            .inner
                    }
                }
            });
        if hide {
            self.apply_tool_window_action(ActionId::Services, ui.ctx());
        }
        if let Some((i, a)) = action {
            let context = self.capture_action_context(ui.ctx());
            match a {
                GridAction::Submit => {
                    self.dispatch_action(ActionId::Submit, &context, ui.ctx());
                }
                GridAction::Rerun => {
                    self.dispatch_action(ActionId::Reload, &context, ui.ctx());
                }
                action => self.grid_action(idx, i, action),
            }
        }
    }

    /// Editor tabs: flat, the active one underlined, with a close button.
    fn tab_strip(&mut self, ui: &mut Ui) {
        let mut close = None;
        let mut select = None;
        let mut action = None;
        let reveal = std::mem::take(&mut self.reveal_active_tab);
        egui::ScrollArea::horizontal()
            .id_salt("tabs")
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    for (i, t) in self.tabs.iter().enumerate() {
                        let icon = match t.kind {
                            TabKind::Console { .. } => "QL",
                            TabKind::Table { .. } => "⊞",
                        };
                        let ds = self
                            .source(&t.ds_id)
                            .map(|d| d.display_name())
                            .unwrap_or_default();
                        let dirty = t.in_tx || t.results.iter().any(|r| !r.edits.is_empty());
                        let mark = if dirty { " •" } else { "" };
                        let label = RichText::new(format!("{icon} {}{mark}", t.title));
                        let (r, closed) = theme::flat_tab(ui, label, i == self.active, true);
                        let r = r.on_hover_text(format!("{} [{ds}]", t.title));
                        if reveal && i == self.active {
                            r.scroll_to_me(None);
                        }
                        if r.clicked() {
                            select = Some(t.id);
                        }
                        if closed || r.middle_clicked() {
                            close = Some(t.id);
                        }
                        let context = actions::ActionContext {
                            tab_id: Some(t.id),
                            ..Default::default()
                        };
                        r.context_menu(|ui| {
                            for command in [
                                ActionId::CloseTab,
                                ActionId::CloseOtherTabs,
                                ActionId::CloseTabsToRight,
                                ActionId::CloseAllTabs,
                            ] {
                                let definition = actions::action_definition(command);
                                let availability = self.action_availability(command, &context);
                                let mut button = egui::Button::new(definition.name);
                                if let Some(shortcut) = definition.shortcut {
                                    button = button.shortcut_text(actions::format_action_shortcut(
                                        ui.ctx(),
                                        &shortcut,
                                    ));
                                }
                                if ui.add_enabled(availability.enabled, button).clicked() {
                                    action = Some((command, context.clone()));
                                    ui.close();
                                }
                            }
                        });
                    }
                });
            });
        if let Some(id) = select {
            self.select_tab_by_id(id);
        }
        if let Some(id) = close {
            self.request_close_tabs(id, tabs::CloseScope::Tab);
        } else if let Some((action, context)) = action {
            self.dispatch_action(action, &context, ui.ctx());
        }
    }

    fn finish_config_apply(&mut self, result: Result<crate::persistence::Loaded, String>) {
        let Some((close, selected)) = self.config_apply_pending.take() else {
            return;
        };
        match result {
            Err(error) => {
                if let Some(dialog) = &mut self.dialog {
                    dialog.busy = false;
                    dialog.error = Some(error.clone());
                }
                self.notice = Some(ServicesNotice::error(error));
            }
            Ok(loaded) => {
                let changed: Vec<_> = self
                    .config
                    .data_sources
                    .iter()
                    .filter(|old| {
                        loaded.config.data_sources.iter().find(|s| s.id == old.id) != Some(*old)
                    })
                    .map(|s| s.id.clone())
                    .collect();
                for id in changed {
                    self.disconnect(&id);
                }
                self.config.data_sources = loaded.config.data_sources;
                self.config.credential_namespace = loaded.config.credential_namespace;
                self.config.credential_vault_version = loaded.config.credential_vault_version;
                self.config.credential_vault_cleanup = loaded.config.credential_vault_cleanup;
                self.config.credential_cleanup = loaded.config.credential_cleanup;
                for index in 0..self.tabs.len() {
                    self.update_metadata_key(index);
                }
                self.explorer_ui.invalidate();
                if let Some(warning) = loaded.warning {
                    self.notice = Some(ServicesNotice::error(warning));
                }
                if close {
                    self.dialog = None;
                    if let Some(id) = selected {
                        self.expanded.insert((id.clone(), Vec::new()));
                        self.connect(&id);
                        if !self.tabs.iter().any(|tab| tab.ds_id == id) {
                            self.new_console(id);
                        }
                    }
                } else if let Some(dialog) = &mut self.dialog {
                    dialog.sources = self.config.data_sources.clone();
                    dialog.busy = false;
                }
            }
        }
        if std::mem::take(&mut self.config_dirty) {
            self.save_config();
        }
        self.refresh_mcp_resources();
    }

    fn handle_dialog(&mut self, ctx: &egui::Context) {
        let Some(dlg) = &mut self.dialog else { return };
        let action = dlg.show(ctx);
        if matches!(action, DialogAction::Apply | DialogAction::Ok) {
            let close = matches!(action, DialogAction::Ok);
            let selected = dlg
                .selected
                .and_then(|i| dlg.sources.get(i))
                .map(|s| s.id.clone());
            let mut config = self.config.clone();
            config.data_sources = dlg.sources.clone();
            dlg.busy = true;
            dlg.error = None;
            self.config_apply_pending = Some((close, selected));
            self.config_saves_pending += 1;
            self.backend.save_config(config, true);
            return;
        }
        match action {
            DialogAction::None => {}
            DialogAction::Test(ds, request_id) => self.backend.test_connection(*ds, request_id),
            DialogAction::Apply | DialogAction::Ok => unreachable!("handled below"),
            DialogAction::Retry => {
                dlg.busy = true;
                self.backend.reload_credentials();
            }
            DialogAction::ReloadSettings => {
                dlg.busy = true;
                dlg.reload_discard = true;
                dlg.error = None;
                self.backend.reload_credentials();
            }
            DialogAction::Cancel => self.dialog = None,
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        branding::trace_first_frame();
        let splash_active = self.startup_splash.show(ui.ctx());
        if splash_active && self.config_loaded {
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(theme::BG_EDITOR))
                .show(ui, |ui| {
                    self.branding.splash(ui, branding::SplashStatus::Starting)
                });
            return;
        }
        self.handle_events();
        self.refresh_restored_table();
        if ui.ctx().input(|i| i.viewport().close_requested())
            && !self.close_ready
            && self.config_loaded
        {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if !self.closing {
                self.closing = true;
                self.save_config();
            }
        }
        if self.closing && self.config_saves_pending == 0 && self.config_apply_pending.is_none() {
            self.close_ready = true;
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if self.closing {
            egui::Window::new("Saving settings")
                .collapsible(false)
                .resizable(false)
                .show(ui.ctx(), |ui| {
                    ui.spinner();
                    ui.label("Waiting for password storage and configuration writes.");
                    if ui.button("Cancel closing").clicked() {
                        self.closing = false;
                    }
                    if ui.button("Exit without waiting").clicked() {
                        self.close_ready = true;
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
        }
        if !self.config_loaded {
            // Loading stays on the splash; a Keychain prompt may be open meanwhile.
            let error = self.config_loading_error.clone();
            let status = match &error {
                Some(error) => branding::SplashStatus::Failed(error),
                None => branding::SplashStatus::Loading,
            };
            let retry = egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(theme::BG_EDITOR))
                .show(ui, |ui| self.branding.splash(ui, status))
                .inner;
            if retry {
                self.config_loading_error = None;
                self.backend.reload_credentials();
            }
            return;
        }
        let ctx = ui.ctx().clone();
        let mcp_modal = self.mcp_modal_open();
        if !mcp_modal {
            self.handle_action_shortcuts(&ctx);
            self.apply_pending_action(&ctx);
        }
        if !self.find_action.visible
            && !self.related_rows.is_open()
            && !self.compare.is_open()
            && !self.structure_compare.is_open()
            && self.pending_close.is_none()
            && !mcp_modal
            && ctx.memory(|memory| memory.top_modal_layer().is_none())
            && let Some(focus) = self.pending_action_focus.take()
        {
            ctx.memory_mut(|memory| memory.request_focus(focus));
        }
        if !mcp_modal {
            self.apply_pending_editor_focus(&ctx);
        }
        let modal = self.find_action.visible
            || self.pending_close.is_some()
            || self.related_rows.is_open()
            || self.structure_compare.is_open()
            || mcp_modal;
        // Disabled widgets do not block manual shortcut handlers. Hold events
        // while drawing the background, then deliver them only to the modal.
        let events = if modal {
            Some(ctx.input_mut(|input| std::mem::take(&mut input.events)))
        } else if self.compare.is_open() {
            // The comparison is modeless: source tabs remain mouse-accessible,
            // while editor key handlers must not consume its keyboard input.
            Some(ctx.input_mut(|input| {
                let mut held = Vec::new();
                input.events.retain(|event| {
                    if matches!(
                        event,
                        egui::Event::Key { .. }
                            | egui::Event::Text(_)
                            | egui::Event::Paste(_)
                            | egui::Event::Copy
                            | egui::Event::Cut
                            | egui::Event::Ime(_)
                    ) {
                        held.push(event.clone());
                        false
                    } else {
                        true
                    }
                });
                held
            }))
        } else {
            None
        };
        ui.add_enabled_ui(!modal, |ui| {
            self.top_bar(ui);
            self.status_bar(ui);
            self.tool_stripe(ui);
            self.files_panel(ui);
            self.explorer(ui);
            if self.show_run {
                self.run_window(ui);
            } else {
                self.services(ui);
            }
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(theme::BG_EDITOR))
                .show(ui, |ui| {
                    egui::Frame::new()
                        .fill(theme::BG_PANEL)
                        .inner_margin(egui::Margin::symmetric(4, 0))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            self.tab_strip(ui);
                        });
                    theme::hairline(ui);
                    egui::Frame::new()
                        .inner_margin(egui::Margin::symmetric(6, 4))
                        .show(ui, |ui| self.editor(ui));
                });
        });
        if let Some(events) = events {
            ctx.input_mut(|input| input.events.extend(events));
        }
        self.handle_dialog(&ctx);
        self.scope_chooser_window(&ctx);
        self.find_action_window(&ctx);
        self.appearance_window(&ctx);
        self.about_window(&ctx);
        self.export_dialog_window(&ctx);
        self.import_dialog_window(&ctx);
        self.dump_dialog_window(&ctx);
        self.activity_windows(&ctx);
        self.related_rows_ui(&ctx);
        self.close_tabs_dialog(&ctx);
        if !self.structure_compare.is_open()
            && let Some(request) = self.compare.show(&ctx)
        {
            self.backend.compare_data(request);
        }
        if let Some(intent) = self.structure_compare.show(&ctx) {
            self.dispatch_structure_compare(intent);
        }
        if let Some(focus) = self.structure_compare.take_restore_focus() {
            self.pending_action_focus = Some(focus);
        }
        for intent in self.schema_graph.show(&ctx) {
            self.dispatch_graph(intent);
        }
        self.refresh_mcp_documents();
        self.group_dialog_ui(&ctx);
        self.mcp_ui(&ctx);
        self.save_session(false);
    }

    #[cfg(not(target_os = "macos"))]
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.exit();
    }

    #[cfg(target_os = "macos")]
    fn on_exit(&mut self) {
        self.exit();
    }
}

impl App {
    fn exit(&mut self) {
        self.backend.mcp.stop();
        self.structure_compare.cancel();
        if !self.config_loaded {
            return;
        }
        self.finish_session();
        for f in &self.files {
            f.close();
        }
    }
}

#[cfg(test)]
fn active_tab_after_close(active: usize, closed: usize, remaining: usize) -> usize {
    if closed < active {
        active - 1
    } else {
        active.min(remaining.saturating_sub(1))
    }
}

/// A configured data source or the temporary one of an opened file.
fn find_source<'a>(
    config: &'a Config,
    files: &'a [OpenedFile],
    id: &str,
) -> Option<&'a DataSource> {
    config
        .data_sources
        .iter()
        .chain(files.iter().map(|f| &f.source))
        .find(|d| d.id == id)
}

/// Short colored driver marker used in the tree and lists.
pub fn driver_badge(d: DriverKind) -> String {
    match d {
        DriverKind::Postgres => "🐘".into(),
        DriverKind::MySql => "🐬".into(),
        DriverKind::Sqlite => "🗄".into(),
    }
}

/// 1-based line and column of a char offset.
fn line_col(text: &str, char_offset: usize) -> (usize, usize) {
    let (mut line, mut col) = (1, 1);
    for ch in text.chars().take(char_offset) {
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// Key of a table in the filter history.
fn table_key(schema: Option<&str>, table: &str) -> String {
    match schema {
        Some(s) => format!("{s}.{table}"),
        None => table.to_string(),
    }
}

fn rows_word(n: usize) -> String {
    if n == 1 {
        "1 row".into()
    } else {
        format!("{n} rows")
    }
}

/// Width reserved for the macOS close, minimize and zoom buttons, in screen points.
const TRAFFIC_LIGHTS_WIDTH: f32 = 72.0;
/// Main toolbar height at 100 % zoom; with the integrated title bar it stays
/// this tall on screen at every zoom, so the traffic lights fit.
const TOOLBAR_HEIGHT: f32 = 40.0;

/// The SELECT a table editor tab runs, with its WHERE and ORDER BY fields.
fn table_query(kind: &TabKind, driver: DriverKind) -> String {
    let TabKind::Table {
        schema,
        table,
        filter,
        order,
        select_rowid,
    } = kind
    else {
        return String::new();
    };
    let mut sql = select_all(driver, schema.as_deref(), table);
    if *select_rowid && driver == DriverKind::Sqlite {
        sql = sql.replacen("SELECT *", "SELECT rowid, *", 1);
    }
    if !filter.trim().is_empty() {
        sql.push_str(&format!(" WHERE {}", filter.trim()));
    }
    if !order.trim().is_empty() {
        sql.push_str(&format!(" ORDER BY {}", order.trim()));
    }
    sql
}

/// Names the result tab after the first table in `FROM`.
fn guess_title(sql: &str) -> Option<String> {
    let mut words = sql.split_whitespace();
    words.find(|w| w.eq_ignore_ascii_case("from"))?;
    let name = words.next()?.trim_end_matches([';', ',', ')']);
    let name = name
        .rsplit('.')
        .next()?
        .trim_matches(|c| c == '"' || c == '`');
    (!name.is_empty() && !name.starts_with('(')).then(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::active_tab_after_close;

    #[test]
    fn settings_reload_replaces_draft_only_on_success_and_invalidates_changed_sources() {
        use super::*;
        let ctx = egui::Context::default();
        let mut app = actions::tests::isolated_app(&ctx);
        let mut source = DataSource::new(DriverKind::Postgres);
        source.name = "saved".into();
        let source_id = source.id.clone();
        let removed = DataSource::new(DriverKind::Sqlite);
        let removed_id = removed.id.clone();
        app.config.data_sources = vec![source.clone(), removed.clone()];
        app.sources
            .insert(source_id.clone(), SourceState::default());
        app.sources
            .insert(removed_id.clone(), SourceState::default());
        app.expanded.insert((source_id.clone(), Vec::new()));
        app.expanded.insert((removed_id.clone(), Vec::new()));
        let tab = app.add_tab(
            "reload".into(),
            source_id.clone(),
            TabKind::Console { sql: String::new() },
        );
        let revision = app.tabs[tab].context_revision;
        app.dialog = Some(DataSourceDialog::new(
            app.config.data_sources.clone(),
            Some(&source_id),
        ));
        let dialog = app.dialog.as_mut().unwrap();
        dialog.sources[0].name = "draft".into();
        dialog.reload_discard = true;
        dialog.busy = true;
        app.loaded_config(Err("synthetic read failure".into()));
        let dialog = app.dialog.as_ref().unwrap();
        assert_eq!(dialog.sources[0].name, "draft");
        assert!(!dialog.busy);
        assert!(!dialog.reload_discard);
        assert!(app.sources.contains_key(&source_id));
        source.host = "different.invalid".into();
        source.name = "latest saved".into();
        let mut loaded = app.config.clone();
        loaded.data_sources = vec![source];
        app.dialog.as_mut().unwrap().reload_discard = true;
        app.loaded_config(Ok(crate::persistence::Loaded {
            config: loaded,
            warning: None,
        }));
        let dialog = app.dialog.as_ref().unwrap();
        assert_eq!(dialog.sources[0].name, "latest saved");
        assert_eq!(dialog.selected, Some(0));
        assert!(dialog.error.is_none());
        assert!(!app.sources.contains_key(&source_id));
        assert!(!app.sources.contains_key(&removed_id));
        assert!(app.expanded.is_empty());
        assert!(app.tabs[tab].context_revision > revision);
        assert_eq!(app.config.data_sources.len(), 1);
    }

    #[test]
    fn closing_tabs_preserves_the_active_document_or_selects_a_neighbor() {
        for (active, closed, expected) in [
            (2, 0, "C"), // Closing a tab to the left keeps C active.
            (1, 3, "B"), // Closing a tab to the right keeps B active.
            (1, 1, "C"), // Closing the active tab selects its right neighbor.
            (3, 3, "C"), // Closing the last active tab selects its left neighbor.
        ] {
            let mut tabs = vec!["A", "B", "C", "D"];
            tabs.remove(closed);
            let next = active_tab_after_close(active, closed, tabs.len());
            assert_eq!(tabs[next], expected, "active={active}, closed={closed}");
        }
    }

    #[test]
    fn closing_the_only_tab_leaves_the_empty_editor_index() {
        assert_eq!(active_tab_after_close(0, 0, 0), 0);
    }
}
