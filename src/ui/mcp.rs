//! Local MCP controls. The UI is the only surface that can grant permissions.

use super::{App, TabKind, theme};
use crate::mcp::{
    Approval, AuditView, LocalTarget, McpState, Operation, RequestView, ShareAccess, SharedFile,
    SharedSource, SourceAccess, StoredAudience, StoredShare, StoredTable, TableAccess, TableRef,
    UiSnapshot,
};
use egui::{Color32, Id, RichText, Ui};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Section {
    #[default]
    Server,
    Sharing,
    Sessions,
    Activity,
}

/// One shared database: a data source, optionally in another of its databases.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct ShareKey {
    pub source_id: String,
    /// `None` is the database configured in the data source.
    pub database: Option<String>,
}

/// What the Database Explorer's "Share with AI Agents" menu points at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ShareTarget {
    Database(ShareKey),
    Table(ShareKey, TableRef),
}

/// Sharing level offered in menus. `Ask` exists only for whole databases.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ShareLevel {
    NotShared,
    Ask,
    Read,
    ReadWrite,
}

impl ShareLevel {
    pub(super) fn short_label(self) -> &'static str {
        match self {
            Self::NotShared => "Not Shared",
            Self::Ask => "Catalog Only",
            Self::Read => "Read-Only",
            Self::ReadWrite => "Read and Write",
        }
    }
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::NotShared => "Not Shared",
            Self::Ask => "Catalog Only (Ask Each Time)",
            Self::Read => "Read-Only Access",
            Self::ReadWrite => "Read and Write Access",
        }
    }
}

/// Standing access of one audience: every agent (`None`) or one connection.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Audience {
    client_id: Option<String>,
    access: SourceAccess,
    tables: Vec<(TableRef, TableAccess)>,
}

/// A database shared with agents; it is shared when any audience exists.
struct ShareChoice {
    alias: String,
    audiences: Vec<Audience>,
}

impl ShareChoice {
    fn new(alias: String) -> Self {
        Self {
            alias,
            audiences: Vec::new(),
        }
    }
    fn shared(&self) -> bool {
        !self.audiences.is_empty()
    }
    fn audience(&self, client_id: Option<&str>) -> Option<&Audience> {
        self.audiences
            .iter()
            .find(|a| a.client_id.as_deref() == client_id)
    }
}

/// Open documents are shared with every agent connection.
struct DocumentShare {
    shared: bool,
    alias: String,
}

struct DocumentChoice {
    choice: DocumentShare,
    id: String,
    revision: u64,
    version: u64,
    text: Arc<str>,
}

struct ConfigurationReveal {
    text: String,
    created: Instant,
}

struct ApprovalReview {
    request_id: String,
    preview: String,
    escaped_controls: bool,
    scope: Approval,
    understands_full_sql: bool,
    opened: Instant,
    focus_deny: bool,
}

pub(super) struct McpUi {
    pub(super) visible: bool,
    section: Section,
    port: String,
    client_label: String,
    error: Option<String>,
    scroll_to_top: bool,
    snapshot: Arc<UiSnapshot>,
    sources: HashMap<ShareKey, ShareChoice>,
    documents: HashMap<u64, DocumentChoice>,
    next_alias: usize,
    reveal: Option<ConfigurationReveal>,
    review: Option<ApprovalReview>,
    persist: Persist,
    save_pending: bool,
    /// Mirror of `Config::mcp_autostart` for the settings view.
    autostart: bool,
}

/// Saving must not overwrite remembered connections that have not been read yet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Persist {
    /// Configuration not loaded yet, or the Keychain read is in progress.
    #[default]
    Waiting,
    Ready,
    /// The Keychain read failed; changes stay in memory until restart.
    Failed,
}

impl Default for McpUi {
    fn default() -> Self {
        Self {
            visible: false,
            section: Section::Server,
            port: crate::mcp::DEFAULT_PORT.to_string(),
            client_label: "Agent".into(),
            error: None,
            scroll_to_top: true,
            snapshot: Arc::new(UiSnapshot::default()),
            sources: HashMap::new(),
            documents: HashMap::new(),
            next_alias: 1,
            reveal: None,
            review: None,
            persist: Persist::Waiting,
            save_pending: false,
            autostart: false,
        }
    }
}

impl McpUi {
    fn new_alias(&mut self, kind: &str) -> String {
        let alias = format!("{kind} {}", self.next_alias);
        self.next_alias += 1;
        alias
    }
}

enum Intent {
    Start(u16),
    Stop,
    CreateClient,
    RevokeClient(String),
    EndSession(String),
    RevokeGrant(String, String),
    Decision(String, Approval),
    /// Deny the request and put its SQL into a new query console.
    DenyAndOpen(String, LocalTarget, String),
    OpenInConsole(LocalTarget, String),
    ShareChanged,
    Autostart(bool),
    HideConfiguration,
    Close,
}

struct LocalResource {
    key: ShareKey,
    label: String,
    kind: &'static str,
    read_only: bool,
}

impl App {
    pub(super) fn open_mcp_settings(&mut self) {
        self.mcp.visible = true;
        self.mcp.scroll_to_top = true;
        self.mcp.port = self.config.mcp_port.to_string();
        self.mcp.autostart = self.config.mcp_autostart;
        self.mcp.snapshot = self.backend.mcp.snapshot();
        self.pending_editor_focus = false;
        self.pending_action_focus = None;
    }

    pub(super) fn open_mcp_sharing(&mut self) {
        self.open_mcp_settings();
        self.mcp.section = Section::Sharing;
    }

    pub(super) fn mcp_changed(&mut self) {
        self.mcp.snapshot = self.backend.mcp.snapshot();
        if !self.mcp.snapshot.running && !self.mcp.snapshot.starting {
            self.mcp.reveal = None;
        }
    }

    pub(super) fn mcp_modal_open(&self) -> bool {
        self.mcp.visible
            || self
                .mcp
                .snapshot
                .requests
                .iter()
                .any(|r| r.status == "pending")
    }

    pub(super) fn mcp_unshare_source(&mut self, source_id: &str) {
        let before = self.mcp.sources.len();
        self.mcp.sources.retain(|key, _| key.source_id != source_id);
        if before != self.mcp.sources.len() {
            self.mcp_sharing_changed();
        }
    }

    /// Agent connections offered as audiences, after "All Agents".
    pub(super) fn mcp_clients(&self) -> Vec<(String, String)> {
        self.mcp
            .snapshot
            .clients
            .iter()
            .map(|c| (c.id.clone(), c.label.clone()))
            .collect()
    }

    /// Current level for one audience, as shown in the explorer menu.
    pub(super) fn mcp_share_level(&self, target: &ShareTarget, client_id: Option<&str>) -> ShareLevel {
        let (key, table) = match target {
            ShareTarget::Database(key) => (key, None),
            ShareTarget::Table(key, table) => (key, Some(table)),
        };
        let Some(audience) = self.mcp.sources.get(key).and_then(|c| c.audience(client_id)) else {
            return ShareLevel::NotShared;
        };
        match table {
            None => match audience.access {
                SourceAccess::Ask => ShareLevel::Ask,
                SourceAccess::Read => ShareLevel::Read,
                SourceAccess::ReadWrite => ShareLevel::ReadWrite,
            },
            Some(table) => match audience.tables.iter().find(|(t, _)| t == table) {
                Some((_, TableAccess::Read)) => ShareLevel::Read,
                Some((_, TableAccess::ReadWrite)) => ShareLevel::ReadWrite,
                None => ShareLevel::NotShared,
            },
        }
    }

    pub(super) fn set_mcp_share(
        &mut self,
        target: ShareTarget,
        client_id: Option<String>,
        level: ShareLevel,
    ) {
        let key = match &target {
            ShareTarget::Database(key) | ShareTarget::Table(key, _) => key.clone(),
        };
        // A read-only source can never be given write access, even by a stale menu.
        let level = if self.mcp_source_read_only(&key.source_id) && level == ShareLevel::ReadWrite {
            ShareLevel::Read
        } else {
            level
        };
        if client_id
            .as_ref()
            .is_some_and(|id| !self.mcp.snapshot.clients.iter().any(|c| &c.id == id))
        {
            return;
        }
        if !self.mcp.sources.contains_key(&key) {
            if level == ShareLevel::NotShared {
                return;
            }
            let alias = self.mcp.new_alias("Database");
            self.mcp.sources.insert(key.clone(), ShareChoice::new(alias));
        }
        let choice = self.mcp.sources.get_mut(&key).expect("share entry");
        let position = choice
            .audiences
            .iter()
            .position(|a| a.client_id == client_id);
        match target {
            ShareTarget::Database(_) => {
                let access = match level {
                    ShareLevel::NotShared => None,
                    ShareLevel::Ask => Some(SourceAccess::Ask),
                    ShareLevel::Read => Some(SourceAccess::Read),
                    ShareLevel::ReadWrite => Some(SourceAccess::ReadWrite),
                };
                match (access, position) {
                    (None, Some(index)) => {
                        choice.audiences.remove(index);
                    }
                    (None, None) => {}
                    (Some(access), Some(index)) => choice.audiences[index].access = access,
                    (Some(access), None) => choice.audiences.push(Audience {
                        client_id,
                        access,
                        tables: Vec::new(),
                    }),
                }
            }
            ShareTarget::Table(_, table) => {
                let access = match level {
                    ShareLevel::Read => Some(TableAccess::Read),
                    ShareLevel::ReadWrite => Some(TableAccess::ReadWrite),
                    ShareLevel::NotShared | ShareLevel::Ask => None,
                };
                let index = match (position, access) {
                    (Some(index), _) => index,
                    (None, None) => return,
                    (None, Some(_)) => {
                        // Sharing a single table lists the database without wider access.
                        choice.audiences.push(Audience {
                            client_id,
                            access: SourceAccess::Ask,
                            tables: Vec::new(),
                        });
                        choice.audiences.len() - 1
                    }
                };
                let audience = &mut choice.audiences[index];
                audience.tables.retain(|(t, _)| *t != table);
                if let Some(access) = access {
                    audience.tables.push((table, access));
                }
            }
        }
        self.mcp_sharing_changed();
    }

    pub(super) fn mcp_save_failed(&mut self, error: String) {
        self.mcp.error = Some(format!(
            "Agent connections and sharing could not be saved ({error}). They work until restart."
        ));
    }

    /// Applies sharing to the broker and remembers it.
    fn mcp_sharing_changed(&mut self) {
        self.refresh_mcp_resources();
        self.persist_mcp_state();
    }

    /// Called once after the configuration is first loaded.
    pub(super) fn load_mcp_state(&mut self) {
        if self.config.mcp_autostart {
            self.backend.mcp.start(self.config.mcp_port);
        }
        self.mcp.persist = if self.config.mcp_state_saved && self.backend.load_mcp_state() {
            Persist::Waiting
        } else {
            Persist::Ready
        };
        if self.mcp.persist == Persist::Ready && self.mcp.save_pending {
            self.persist_mcp_state();
        }
    }

    pub(super) fn mcp_state_loaded(&mut self, result: Result<Option<McpState>, String>) {
        match result {
            Ok(state) => {
                if let Some(state) = state {
                    self.restore_mcp_state(state);
                }
                self.mcp.persist = Persist::Ready;
                self.refresh_mcp_resources();
                self.mcp_changed();
                if std::mem::take(&mut self.mcp.save_pending) {
                    self.persist_mcp_state();
                }
            }
            Err(error) => {
                self.mcp.persist = Persist::Failed;
                self.mcp.error = Some(format!(
                    "Saved agent connections could not be loaded ({error}). Changes are kept only until restart."
                ));
            }
        }
    }

    fn restore_mcp_state(&mut self, state: McpState) {
        self.backend.mcp.restore_clients(state.clients);
        let clients: Vec<String> = self
            .backend
            .mcp
            .snapshot()
            .clients
            .iter()
            .map(|c| c.id.clone())
            .collect();
        for share in state.shares.into_iter().take(128) {
            let key = ShareKey {
                source_id: share.source_id,
                database: share.database,
            };
            // Choices made in this run before the Keychain answered take precedence.
            if self.mcp.sources.contains_key(&key) || !valid_alias(&share.alias) {
                continue;
            }
            let audiences = share
                .audiences
                .into_iter()
                .filter(|a| a.client_id.as_ref().is_none_or(|id| clients.contains(id)))
                .map(|a| Audience {
                    client_id: a.client_id,
                    access: a.access,
                    tables: a
                        .tables
                        .into_iter()
                        .map(|t| {
                            (
                                TableRef {
                                    schema: t.schema,
                                    name: t.name,
                                },
                                t.access,
                            )
                        })
                        .collect(),
                })
                .collect();
            self.mcp.sources.insert(
                key,
                ShareChoice {
                    alias: share.alias,
                    audiences,
                },
            );
            self.mcp.next_alias += 1;
        }
    }

    /// Connections and sharing are written to the Keychain off the UI thread.
    fn persist_mcp_state(&mut self) {
        if self.mcp.persist != Persist::Ready {
            self.mcp.save_pending = self.mcp.persist == Persist::Waiting;
            return;
        }
        let shares = self
            .mcp
            .sources
            .iter()
            .filter(|(_, choice)| choice.shared())
            .map(|(key, choice)| StoredShare {
                source_id: key.source_id.clone(),
                database: key.database.clone(),
                alias: choice.alias.clone(),
                audiences: choice
                    .audiences
                    .iter()
                    .map(|a| StoredAudience {
                        client_id: a.client_id.clone(),
                        access: a.access,
                        tables: a
                            .tables
                            .iter()
                            .map(|(table, access)| StoredTable {
                                schema: table.schema.clone(),
                                name: table.name.clone(),
                                access: *access,
                            })
                            .collect(),
                    })
                    .collect(),
            })
            .collect();
        let state = McpState {
            clients: self.backend.mcp.stored_clients(),
            shares,
        };
        if !self.config.mcp_state_saved && state == McpState::default() {
            return;
        }
        if self.backend.save_mcp_state(state) && !self.config.mcp_state_saved {
            self.config.mcp_state_saved = true;
            self.save_config();
        }
    }

    pub(super) fn mcp_source_read_only(&self, source_id: &str) -> bool {
        self.files.iter().any(|f| f.source.id == source_id)
            || self.source(source_id).is_none_or(|s| s.read_only)
    }

    /// Source clones happen only after a local edit or a sharing decision.
    pub(super) fn refresh_mcp_resources(&mut self) {
        self.mcp.sources.retain(|key, _| {
            self.config.data_sources.iter().any(|s| s.id == key.source_id)
                || self.files.iter().any(|f| f.source.id == key.source_id)
        });
        let sources = self
            .mcp
            .sources
            .iter()
            .filter_map(|(key, choice)| {
                if !choice.shared() || !valid_alias(&choice.alias) {
                    return None;
                }
                let mut source = self.source(&key.source_id)?.clone();
                if let Some(database) = &key.database {
                    source = source.with_database(database);
                }
                // Imported files are always shared through the strict read tools.
                if self.files.iter().any(|file| file.source.id == key.source_id) {
                    source.read_only = true;
                }
                Some(SharedSource {
                    alias: choice.alias.trim().into(),
                    source,
                    audiences: choice
                        .audiences
                        .iter()
                        .map(|a| {
                            (
                                a.client_id.clone(),
                                ShareAccess {
                                    source: a.access,
                                    tables: a.tables.clone(),
                                },
                            )
                        })
                        .collect(),
                })
            })
            .collect();
        let files = self
            .mcp
            .documents
            .values()
            .filter(|d| d.choice.shared)
            .map(|d| SharedFile {
                id: d.id.clone(),
                alias: d.choice.alias.trim().into(),
                version: d.version,
                text: Arc::clone(&d.text),
            })
            .collect();
        self.backend.mcp.update_resources(sources, files);
    }

    /// Mutation counters are constant-size reads; text is copied only after edits.
    pub(super) fn refresh_mcp_documents(&mut self) {
        let mut changed = false;
        self.mcp.documents.retain(|id, document| {
            let exists = self
                .tabs
                .iter()
                .any(|t| t.id == *id && matches!(t.kind, TabKind::Console { .. }));
            changed |= !exists && document.choice.shared;
            exists
        });
        let mut shared_bytes: usize = self
            .mcp
            .documents
            .values()
            .filter(|document| document.choice.shared)
            .map(|document| document.text.len())
            .sum();
        for (id, document) in &mut self.mcp.documents {
            if !document.choice.shared {
                continue;
            }
            let Some(tab) = self.tabs.iter().find(|t| t.id == *id) else {
                continue;
            };
            let revision = tab.editor_cache.source_revision();
            if revision != document.revision
                && let TabKind::Console { sql } = &tab.kind
            {
                let next_bytes = shared_bytes
                    .saturating_sub(document.text.len())
                    .saturating_add(sql.len());
                if sql.len() > crate::mcp::MAX_SHARED_FILE_BYTES
                    || next_bytes > crate::mcp::MAX_SHARED_TEXT_BYTES
                {
                    shared_bytes = shared_bytes.saturating_sub(document.text.len());
                    document.choice.shared = false;
                    document.text = Arc::from("");
                    self.mcp.error = Some(
                            "Document sharing was disabled: each document may use at most 4 MiB and all shared documents at most 16 MiB.".into(),
                        );
                } else {
                    shared_bytes = next_bytes;
                    document.text = Arc::from(sql.as_str());
                }
                document.version = document.version.wrapping_add(1);
                document.revision = revision;
                changed = true;
            }
        }
        if changed {
            self.refresh_mcp_resources();
        }
    }

    /// Opens agent SQL in a new console on the same local source and database.
    fn open_mcp_sql_in_console(&mut self, target: &LocalTarget, sql: String) {
        let Some(source) = self.source(&target.source_id) else {
            self.mcp.error = Some("The data source of this request no longer exists.".into());
            return;
        };
        let database = (source.database != target.database).then(|| target.database.clone());
        self.open_console(target.source_id.clone(), None, sql);
        if let Some(tab) = self.tabs.last_mut() {
            tab.database = database;
        }
        self.update_metadata_key(self.active);
        self.ensure_console_metadata(self.active);
        self.mcp.visible = false;
        self.mcp.reveal = None;
        self.pending_editor_focus = true;
    }

    pub(super) fn mcp_ui(&mut self, ctx: &egui::Context) {
        if !self.mcp_modal_open() {
            return;
        }
        let mut state = std::mem::take(&mut self.mcp);
        let mut intents = Vec::new();
        if state.visible {
            let local_sources = self
                .config
                .data_sources
                .iter()
                .map(|source| LocalResource {
                    key: ShareKey {
                        source_id: source.id.clone(),
                        database: None,
                    },
                    label: source.display_name(),
                    kind: "Database",
                    read_only: source.read_only,
                })
                .chain(self.files.iter().map(|file| LocalResource {
                    key: ShareKey {
                        source_id: file.source.id.clone(),
                        database: None,
                    },
                    label: file.origin.file_name().map_or_else(
                        || "Opened data file".into(),
                        |n| n.to_string_lossy().into_owned(),
                    ),
                    kind: "Data file",
                    read_only: true,
                }))
                .collect::<Vec<_>>();
            let local_documents = self
                .tabs
                .iter()
                .filter_map(|tab| match &tab.kind {
                    TabKind::Console { sql } => Some((
                        tab.id,
                        tab.title.as_str(),
                        sql.as_str(),
                        tab.editor_cache.source_revision(),
                    )),
                    _ => None,
                })
                .collect::<Vec<_>>();
            state.settings(ctx, &local_sources, &local_documents, &mut intents);
        }
        state.approval(ctx, &mut intents);
        self.mcp = state;
        for intent in intents {
            match intent {
                Intent::Start(port) => {
                    self.mcp.error = None;
                    self.config.mcp_port = port;
                    self.save_config();
                    self.backend.mcp.start(port);
                    self.refresh_mcp_resources();
                }
                Intent::Stop => {
                    self.backend.mcp.stop();
                    self.mcp.reveal = None;
                    self.mcp.review = None;
                }
                Intent::CreateClient => match self
                    .backend
                    .mcp
                    .create_client(self.mcp.client_label.trim().to_owned(), self.config.mcp_port)
                {
                    Ok(connection) => {
                        self.mcp.reveal = Some(ConfigurationReveal {
                            text: connection.configuration,
                            created: Instant::now(),
                        });
                        self.mcp.error = None;
                        self.mcp.snapshot = self.backend.mcp.snapshot();
                        self.persist_mcp_state();
                    }
                    Err(error) => self.mcp.error = Some(error),
                },
                Intent::RevokeClient(id) => {
                    self.backend.mcp.revoke_client(&id);
                    self.mcp.reveal = None;
                    for choice in self.mcp.sources.values_mut() {
                        choice
                            .audiences
                            .retain(|a| a.client_id.as_deref() != Some(id.as_str()));
                    }
                    self.mcp.snapshot = self.backend.mcp.snapshot();
                    self.mcp_sharing_changed();
                }
                Intent::EndSession(id) => self.backend.mcp.end_session(&id),
                Intent::RevokeGrant(session, grant) => {
                    self.backend.mcp.revoke_grant(&session, &grant)
                }
                Intent::Decision(id, decision) => {
                    if let Err(error) = self.backend.mcp.approve(&id, decision) {
                        self.mcp.error = Some(error);
                    }
                    self.mcp.review = None;
                }
                Intent::DenyAndOpen(id, target, sql) => {
                    if let Err(error) = self.backend.mcp.approve(&id, Approval::Deny) {
                        self.mcp.error = Some(error);
                    }
                    self.mcp.review = None;
                    self.open_mcp_sql_in_console(&target, sql);
                }
                Intent::OpenInConsole(target, sql) => self.open_mcp_sql_in_console(&target, sql),
                Intent::ShareChanged => self.mcp_sharing_changed(),
                Intent::Autostart(enabled) => {
                    self.config.mcp_autostart = enabled;
                    self.save_config();
                }
                Intent::HideConfiguration => {
                    self.mcp.reveal = None;
                    self.mcp.scroll_to_top = true;
                }
                Intent::Close => {
                    self.mcp.visible = false;
                    self.mcp.reveal = None;
                }
            }
        }
        self.mcp.snapshot = self.backend.mcp.snapshot();
    }
}

impl McpUi {
    fn settings(
        &mut self,
        ctx: &egui::Context,
        sources: &[LocalResource],
        documents: &[(u64, &str, &str, u64)],
        intents: &mut Vec<Intent>,
    ) {
        let size = ctx.content_rect().size();
        let reset_scroll = std::mem::take(&mut self.scroll_to_top);
        // Fixed size: switching sections never resizes it.
        let width = (size.x - 48.0).clamp(300.0, 1040.0);
        let height = (size.y - 64.0).clamp(240.0, 820.0);
        let response = theme::Dialog::new(Id::new("mcp_settings"), "Settings")
            .default_size([width, height])
            .min_size([300.0, 240.0])
            .frame(egui::Frame::window(&ctx.global_style()).fill(theme::BG_PANEL).inner_margin(16))
            .show(ctx, |ui| {
                // Switching sections keeps the size; the user may resize the dialog.
                ui.set_min_size(ui.available_size());
                ui.spacing_mut().item_spacing.y = 7.0;
                // The footer is reserved first so sections scroll above it.
                egui::Panel::bottom("mcp_settings_footer")
                    .resizable(false)
                    .frame(egui::Frame::new())
                    .show(ui, |ui| {
                        theme::dialog_footer(ui, |_| {}, |ui| {
                            if theme::dialog_button(ui, "Close").on_hover_text("Changes here apply at once").clicked() {
                                intents.push(Intent::Close);
                            }
                        });
                    });
                ui.label(RichText::new("Tools / MCP Server").color(theme::TEXT_MUTED));
                theme::hairline(ui);
                const SECTIONS: [(Section, &str); 4] = [
                    (Section::Server, "Server"),
                    (Section::Sharing, "Sharing"),
                    (Section::Sessions, "Sessions"),
                    (Section::Activity, "Activity"),
                ];
                let mut section = self.section;
                let narrow = width < 640.0;
                let content = |this: &mut Self, ui: &mut Ui, intents: &mut Vec<Intent>| {
                    if let Some(error) = this.error.as_ref().or(this.snapshot.error.as_ref()) {
                        ui.label(RichText::new(visible_label(error)).color(theme::RED));
                    }
                    let mut scroll = egui::ScrollArea::vertical()
                        .id_salt("mcp_settings_scroll")
                        .auto_shrink([false, false]);
                    if reset_scroll {
                        scroll = scroll.vertical_scroll_offset(0.0);
                    }
                    scroll.show(ui, |ui| {
                        ui.set_min_width(ui.available_width() - 12.0);
                        match this.section {
                            Section::Server => this.server(ui, intents),
                            Section::Sharing => this.sharing(ui, sources, documents, intents),
                            Section::Sessions => this.sessions(ui, intents),
                            Section::Activity => this.activity(ui, intents),
                        }
                    });
                };
                if narrow {
                    ui.horizontal_wrapped(|ui| {
                        for (value, label) in SECTIONS {
                            ui.selectable_value(&mut section, value, label);
                        }
                    });
                    ui.separator();
                    self.switch_section(section);
                    content(self, ui, intents);
                } else {
                    ui.horizontal_top(|ui| {
                        ui.vertical(|ui| {
                            ui.set_width(170.0);
                            for (value, label) in SECTIONS {
                                let selected = section == value;
                                let button = egui::Button::selectable(
                                    selected,
                                    RichText::new(label).size(14.0),
                                )
                                .min_size(egui::vec2(170.0, 30.0));
                                if ui.add(button).clicked() {
                                    section = value;
                                }
                            }
                        });
                        ui.separator();
                        self.switch_section(section);
                        ui.vertical(|ui| content(self, ui, intents));
                    });
                }
            });
        if response.should_close() {
            intents.push(Intent::Close);
        }
    }

    fn switch_section(&mut self, section: Section) {
        if self.section != section {
            self.section = section;
            self.reveal = None;
            self.scroll_to_top = true;
        }
    }

    fn server(&mut self, ui: &mut Ui, intents: &mut Vec<Intent>) {
        ui.heading("Local MCP Server");
        ui.label("Give each agent its own connection. Access always starts without permissions.");
        let busy = self.snapshot.running || self.snapshot.starting;
        ui.horizontal_wrapped(|ui| {
            ui.label("Listen on 127.0.0.1");
            let label = ui.label("Port:");
            ui.add_enabled(
                !busy,
                egui::TextEdit::singleline(&mut self.port)
                    .desired_width(75.0)
                    .char_limit(5),
            )
            .labelled_by(label.id);
            if self.snapshot.starting {
                ui.spinner();
                ui.label("Starting…");
            } else if self.snapshot.running {
                ui.label(RichText::new("Running").color(theme::GREEN));
            } else {
                ui.label("Stopped");
            }
            if busy {
                if ui.button("Stop Server").clicked() {
                    intents.push(Intent::Stop);
                }
            } else if ui.button("Start Server").clicked() {
                match self.port.parse::<u16>() {
                    Ok(port) if port != 0 => intents.push(Intent::Start(port)),
                    _ => self.error = Some("Enter a port between 1 and 65535.".into()),
                }
            }
        });
        if let Some(endpoint) = &self.snapshot.endpoint {
            ui.monospace(endpoint);
        }
        let mut autostart = self.autostart;
        if ui
            .checkbox(&mut autostart, "Start automatically when DBeast opens")
            .changed()
        {
            self.autostart = autostart;
            intents.push(Intent::Autostart(autostart));
        }
        ui.label(RichText::new("Stop the server to change the port. Stopping ends every session and session permission; agent connections and sharing are remembered.").color(theme::TEXT_MUTED));
        ui.add_space(8.0);
        ui.separator();
        ui.strong("Agent connections");
        ui.label(RichText::new("Create one connection per agent (for example Claude Code or Codex), then choose in Sharing or Database Explorer what each one may access.").color(theme::TEXT_MUTED));
        ui.add_enabled_ui(true, |ui| {
            ui.horizontal_wrapped(|ui| {
                let label = ui.label("Connection label:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.client_label)
                        .desired_width(200.0)
                        .char_limit(60),
                )
                .labelled_by(label.id);
                if ui
                    .add_enabled(
                        !self.client_label.trim().is_empty(),
                        egui::Button::new("Create Agent Connection"),
                    )
                    .clicked()
                {
                    intents.push(Intent::CreateClient);
                }
            });
        });
        if self
            .reveal
            .as_ref()
            .is_some_and(|r| r.created.elapsed() > Duration::from_secs(120))
        {
            self.reveal = None;
        }
        if let Some(reveal) = &self.reveal {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.strong("Private client configuration");
                ui.label("Copy into one trusted MCP client. Database access still needs your approval. This private token is shown for two minutes.");
                immutable_text(ui, "mcp_client_configuration", &reveal.text, 160.0, false);
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Copy Configuration").clicked() { ui.ctx().copy_text(reveal.text.clone()); }
                    if ui.button("Hide Configuration").clicked() { intents.push(Intent::HideConfiguration); }
                });
            });
            ui.ctx().request_repaint_after(Duration::from_secs(1));
        }
        if self.snapshot.clients.is_empty() {
            ui.label(RichText::new("No agent connections.").color(theme::TEXT_MUTED));
        }
        for client in &self.snapshot.clients {
            ui.push_id(&client.id, |ui| {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal_wrapped(|ui| {
                        ui.strong(visible_label(&client.label));
                        if ui.button("Revoke Connection").clicked() {
                            intents.push(Intent::RevokeClient(client.id.clone()));
                        }
                    });
                    ui.label(
                        RichText::new(format!("Client ID: {}", client.id))
                            .monospace()
                            .color(theme::TEXT_MUTED),
                    );
                });
            });
        }
        ui.add_space(6.0);
        ui.label(RichText::new(format!("Unless automatic start is on, the server starts only when you click Start Server. Connections and sharing are kept in the {} (only a hash of each token); sessions and session permissions are kept in memory.", if cfg!(target_os = "macos") { "macOS Keychain" } else { "system keyring" })).color(theme::TEXT_MUTED));
    }

    fn sharing(
        &mut self,
        ui: &mut Ui,
        sources: &[LocalResource],
        documents: &[(u64, &str, &str, u64)],
        intents: &mut Vec<Intent>,
    ) {
        ui.heading("Shared resources");
        ui.label("Choose what appears in the agent's catalog and what it may do without asking. Anything beyond the chosen access still needs your approval.");
        ui.label(RichText::new("Access is set per agent connection; All Agents applies to every connection. Tip: right-click a database or table in Database Explorer and choose Share with AI Agents.").color(theme::TEXT_MUTED));
        ui.label(RichText::new("Agents see the alias and database type. Use aliases without private addresses, credentials or paths.").color(theme::TEXT_MUTED));
        if sources.is_empty() && documents.is_empty() {
            ui.label("Add a data source or open a document first.");
        }
        // Other databases of a source appear once shared from the explorer.
        let mut extra: Vec<LocalResource> = self
            .sources
            .keys()
            .filter(|key| key.database.is_some())
            .filter_map(|key| {
                let base = sources.iter().find(|s| s.key.source_id == key.source_id)?;
                Some(LocalResource {
                    key: key.clone(),
                    label: format!("{} / {}", base.label, key.database.as_deref().unwrap_or("")),
                    kind: base.kind,
                    read_only: base.read_only,
                })
            })
            .collect();
        extra.sort_by(|a, b| a.label.cmp(&b.label));
        let clients: Vec<(Option<String>, String)> = std::iter::once((None, "All Agents".to_owned()))
            .chain(
                self.snapshot
                    .clients
                    .iter()
                    .map(|c| (Some(c.id.clone()), visible_label(&c.label))),
            )
            .collect();
        for source in sources.iter().chain(extra.iter()) {
            let can_share = self.sources.values().filter(|choice| choice.shared()).count() < 128;
            if !self.sources.contains_key(&source.key) {
                let alias = self.new_alias(source.kind);
                self.sources.insert(source.key.clone(), ShareChoice::new(alias));
            }
            let choice = self.sources.get_mut(&source.key).expect("share entry");
            ui.push_id((&source.key.source_id, &source.key.database), |ui| {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        RichText::new(format!("{} (local name)", visible_label(&source.label)))
                            .strong(),
                    );
                    if alias_row(ui, &mut choice.alias) && choice.shared() {
                        intents.push(Intent::ShareChanged);
                    }
                    ui.add_enabled_ui(can_share || choice.shared(), |ui| {
                        if audience_rows(ui, choice, &clients, source.read_only) {
                            intents.push(Intent::ShareChanged);
                        }
                    });
                    if source.read_only {
                        ui.label(
                            RichText::new("Read-only source. Write access and Full SQL are unavailable.")
                                .small()
                                .color(theme::TEXT_MUTED),
                        );
                    }
                });
            });
        }
        if !documents.is_empty() {
            ui.add_space(8.0);
            ui.strong("Open query documents");
            ui.label("Shared with every agent connection. Shares the current in-memory text; editing or closing it revokes requests for its previous version.");
        }
        for &(id, title, sql, revision) in documents {
            let shared_bytes: usize = self
                .documents
                .values()
                .filter(|document| document.choice.shared)
                .map(|document| document.text.len())
                .sum();
            let can_share = self
                .documents
                .values()
                .filter(|doc| doc.choice.shared)
                .count()
                < 128;
            if !self.documents.contains_key(&id) {
                let alias = self.new_alias("Document");
                self.documents.insert(
                    id,
                    DocumentChoice {
                        choice: DocumentShare {
                            shared: false,
                            alias,
                        },
                        id: uuid::Uuid::new_v4().to_string(),
                        revision,
                        version: 1,
                        text: Arc::from(""),
                    },
                );
            }
            let document = self.documents.get_mut(&id).expect("document entry");
            ui.push_id(("document", id), |ui| {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.strong(format!("{} (local title)", visible_label(title)));
                    let was_shared = document.choice.shared;
                    let mut changed = false;
                    ui.add_enabled_ui(
                        was_shared || (can_share && sql.len() <= crate::mcp::MAX_SHARED_FILE_BYTES && shared_bytes.saturating_add(sql.len()) <= crate::mcp::MAX_SHARED_TEXT_BYTES),
                        |ui| {
                            changed = share_row(ui, &mut document.choice);
                        },
                    );
                    if changed {
                        if document.choice.shared && !was_shared {
                            document.text = Arc::from(sql);
                            document.revision = revision;
                            document.version = document.version.wrapping_add(1);
                        }
                        if !document.choice.shared {
                            document.text = Arc::from("");
                        }
                        intents.push(Intent::ShareChanged);
                    }
                    if sql.len() > crate::mcp::MAX_SHARED_FILE_BYTES {
                        ui.label("This document exceeds the 4 MiB sharing limit.");
                    } else if !was_shared && shared_bytes.saturating_add(sql.len()) > crate::mcp::MAX_SHARED_TEXT_BYTES {
                        ui.label("Unshare another document first. Shared text is limited to 16 MiB in total.");
                    }
                    ui.label(
                        RichText::new("Text snapshot only. No file path or filesystem access.")
                            .small()
                            .color(theme::TEXT_MUTED),
                    );
                });
            });
        }
        ui.label(
            RichText::new("Up to 128 databases and 128 documents can be shared. Shared text is limited to 16 MiB in total.")
                .small()
                .color(theme::TEXT_MUTED),
        );
    }

    fn sessions(&self, ui: &mut Ui, intents: &mut Vec<Intent>) {
        ui.heading("Agent sessions");
        ui.label("A new session starts without access. Revoke individual grants or end the session to cancel its requests.");
        ui.label(
            RichText::new(
                "Client labels are unverified. Compare the complete session ID with your agent.",
            )
            .color(theme::TEXT_MUTED),
        );
        if self.snapshot.sessions.is_empty() {
            ui.label("No active sessions.");
        }
        for session in &self.snapshot.sessions {
            ui.push_id(&session.id, |ui| {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal_wrapped(|ui| {
                        ui.strong(format!("{} (unverified)", visible_label(&session.label)));
                        if ui.button("End Session").clicked() {
                            intents.push(Intent::EndSession(session.id.clone()));
                        }
                    });
                    ui.label(RichText::new(format!("Session ID: {}", session.id)).monospace());
                    let left = session.expires_at.saturating_duration_since(std::time::Instant::now()).as_secs();
                    ui.label(format!("Expires in {} min {} sec", left / 60, left % 60));
                    // Only a visible countdown repaints, once per second.
                    ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
                    if session.grants.is_empty() {
                        ui.label(RichText::new("No session permissions. Shared access from the Sharing section still applies.").color(theme::TEXT_MUTED));
                    }
                    for grant in &session.grants {
                        ui.push_id(&grant.id, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(format!(
                                    "{}: {}",
                                    visible_label(&grant.alias),
                                    visible_label(&grant.scope)
                                ));
                                if ui.button("Revoke Permission").clicked() {
                                    intents.push(Intent::RevokeGrant(
                                        session.id.clone(),
                                        grant.id.clone(),
                                    ));
                                }
                            });
                        });
                    }
                });
            });
        }
    }

    fn activity(&self, ui: &mut Ui, intents: &mut Vec<Intent>) {
        ui.heading("Recent activity");
        ui.label("Temporary audit log with the exact SQL of each request. Tokens, credentials, results and document text are not included.");
        if self.snapshot.audit.is_empty() {
            ui.label("No activity yet.");
        }
        // Grouping and escaping run once per snapshot, not on every frame.
        let key = egui::Id::new("mcp_activity_entries");
        let snapshot = Arc::as_ptr(&self.snapshot) as usize;
        let entries = match ui.data(|data| data.get_temp::<(usize, Arc<Vec<ActivityEntry>>)>(key)) {
            Some((cached, entries)) if cached == snapshot => entries,
            _ => {
                let entries: Arc<Vec<ActivityEntry>> = Arc::new(
                    group_by_request(&self.snapshot.audit)
                        .iter()
                        .map(|events| ActivityEntry::new(events))
                        .collect(),
                );
                ui.data_mut(|data| data.insert_temp(key, (snapshot, Arc::clone(&entries))));
                entries
            }
        };
        for (index, entry) in entries.iter().enumerate() {
            ui.push_id(index, |ui| {
                ui.separator();
                activity_entry(ui, entry, intents);
            });
        }
    }

    fn approval(&mut self, ctx: &egui::Context, intents: &mut Vec<Intent>) {
        let snapshot = Arc::clone(&self.snapshot);
        // Newly queued requests cannot replace the operation currently being reviewed.
        let current = self.review.as_ref().and_then(|review| {
            snapshot
                .requests
                .iter()
                .find(|r| r.id == review.request_id && r.status == "pending")
        });
        let Some(request) =
            current.or_else(|| snapshot.requests.iter().find(|r| r.status == "pending"))
        else {
            self.review = None;
            return;
        };
        if self
            .review
            .as_ref()
            .is_none_or(|review| review.request_id != request.id)
        {
            let (preview, escaped_controls) = visible_text(&request.preview);
            self.review = Some(ApprovalReview {
                request_id: request.id.clone(),
                preview,
                escaped_controls,
                scope: Approval::Once,
                understands_full_sql: false,
                opened: Instant::now(),
                focus_deny: true,
            });
        }
        let review = self.review.as_mut().unwrap();
        let size = ctx.content_rect().size();
        let is_document = matches!(request.operation, Operation::ReadFile { .. });
        let response = theme::Dialog::new(Id::new("mcp_approval"), "Agent access request")
            .default_size([(size.x - 56.0).clamp(280.0, 820.0), 480.0])
            .frame(egui::Frame::window(&ctx.global_style()).fill(theme::BG_PANEL).inner_margin(16))
            .show(ctx, |ui| {
                if let Some(error) = &self.error { ui.label(RichText::new(visible_label(error)).color(theme::RED)); }
                ui.label(RichText::new(format!("{} (unverified client label)", visible_label(&request.client_label))).strong());
                ui.label(RichText::new(format!("Session ID: {}", request.session_id)).monospace());
                ui.label(RichText::new(format!("Request ID: {}", request.id)).small().monospace().color(theme::TEXT_MUTED));
                ui.strong(format!("Resource: {}", visible_label(&request.alias)));
                ui.separator();
                egui::ScrollArea::vertical().id_salt("mcp_approval_details").max_height((size.y - 275.0).max(100.0)).show(ui, |ui| {
                    operation_details(ui, request);
                    if review.escaped_controls {
                        ui.label(RichText::new("Invisible control characters are shown as \\u{...}. The original request is unchanged.").color(theme::RED));
                    }
                    ui.strong(if is_document { "Exact text to share" } else { "Exact SQL / operation" });
                    immutable_text(ui, ("mcp_approval_preview", &request.id), &review.preview, (size.y * 0.3).clamp(90.0, 230.0), true);
                    ui.separator();
                    ui.strong("Permission scope");
                    ui.radio_value(&mut review.scope, Approval::Once, "Only this request");
                    if request.operation.is_full_sql() {
                        if request.can_allow_full_sql {
                            ui.radio_value(&mut review.scope, Approval::FullSqlForSession, "Full SQL access for this session");
                            ui.label(RichText::new("Full SQL can modify or delete data and use every privilege of this connection, including access beyond this database. Cancellation cannot undo committed writes.").color(theme::RED));
                            ui.checkbox(&mut review.understands_full_sql, "I understand and allow the full SQL capability");
                        } else {
                            ui.label(RichText::new("Full SQL is blocked for this read-only source.").color(theme::RED));
                        }
                    } else if request.operation.source_id().is_some() {
                        if request.eligible_table.is_some() {
                            ui.radio_value(&mut review.scope, Approval::ReadTableForSession, "Read this table for this session");
                        }
                        ui.radio_value(&mut review.scope, Approval::ReadSourceForSession, "Read this database for this session");
                        ui.label(RichText::new("Read-only SQL is checked before execution. Unsupported expressions are denied; writes require a separate full SQL permission.").color(theme::TEXT_MUTED));
                    }
                });
                let can_allow = (!request.operation.is_full_sql() || (request.can_allow_full_sql && review.understands_full_sql))
                    && review.opened.elapsed() >= Duration::from_millis(400);
                ui.label(RichText::new("Only your decision here grants access.").small().color(theme::TEXT_MUTED));
                // Each side collects its intents; both closures cannot borrow the list.
                let (mut left, mut right) = (Vec::new(), Vec::new());
                theme::dialog_footer(
                    ui,
                    |ui| {
                        // Rarely needed actions stay out of the way of the decision.
                        ui.menu_button("More", |ui| {
                            if ui.button("End This Session").clicked() {
                                left.push(Intent::EndSession(request.session_id.clone()));
                                ui.close();
                            }
                            if ui.button("Stop MCP Server").clicked() {
                                left.push(Intent::Stop);
                                ui.close();
                            }
                        });
                        if !is_document
                            && let Some(local) = &request.local
                            && ui
                                .button("Deny and Open in Query Console")
                                .on_hover_text("Denies the agent's request and opens this SQL in a new query console, where you can edit and run it yourself.")
                                .clicked()
                        {
                            // Escaped text keeps hidden control characters visible in the editor too.
                            left.push(Intent::DenyAndOpen(request.id.clone(), local.clone(), review.preview.clone()));
                        }
                    },
                    |ui| {
                        // Right to left: Deny is the default (focused, Escape), Allow the primary action.
                        let deny = theme::dialog_button(ui, "Deny");
                        if review.focus_deny {
                            deny.request_focus();
                            review.focus_deny = false;
                        }
                        if deny.clicked() {
                            right.push(Intent::Decision(request.id.clone(), Approval::Deny));
                        }
                        let allow = if review.scope == Approval::Once { "Allow Once" } else { "Allow for Session" };
                        if ui
                            .add_enabled(
                                can_allow,
                                egui::Button::new(RichText::new(allow).color(Color32::WHITE))
                                    .fill(theme::ACCENT)
                                    .min_size(egui::vec2(72.0, 26.0)),
                            )
                            .clicked()
                        {
                            right.push(Intent::Decision(request.id.clone(), review.scope));
                        }
                    },
                );
                intents.extend(left);
                intents.extend(right);
            });
        if review.opened.elapsed() < Duration::from_millis(400) {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        if response.should_close() {
            intents.push(Intent::Decision(request.id.clone(), Approval::Deny));
        }
    }
}

/// The audit log has one event per lifecycle step (newest first). Each request
/// is shown once, newest request first, with its steps in chronological order.
fn group_by_request(audit: &[AuditView]) -> Vec<Vec<&AuditView>> {
    let mut groups: Vec<Vec<&AuditView>> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();
    for event in audit {
        let slot = *index.entry(event.request_id.as_str()).or_insert_with(|| {
            groups.push(Vec::new());
            groups.len() - 1
        });
        groups[slot].push(event);
    }
    for group in &mut groups {
        group.reverse();
    }
    groups
}

fn status_color(status: &str) -> Color32 {
    if status.starts_with("completed") {
        theme::GREEN
    } else if ["denied", "failed", "outcome_unknown", "revoked", "expired", "cancelled"]
        .iter()
        .any(|s| status.starts_with(s))
    {
        theme::RED
    } else {
        theme::TEXT
    }
}

/// One request: final status, how it was authorized, its steps and SQL once.
/// One request of the Activity log, with its texts prepared once.
#[derive(Clone)]
struct ActivityEntry {
    at: String,
    alias: String,
    status: String,
    authorization: &'static str,
    steps: String,
    ids: String,
    sql: Option<String>,
    local: Option<LocalTarget>,
}

impl ActivityEntry {
    fn new(events: &[&AuditView]) -> Self {
        let (first, last) = (events[0], events[events.len() - 1]);
        let authorization = events
            .iter()
            .find(|e| e.action != "request" && e.action != "result")
            .map(|e| match (e.action.as_str(), e.status.as_str()) {
                ("approval", "denied") => "denied by you",
                ("approval", _) => "approved by you",
                ("shared access", _) => "allowed by shared access",
                ("session permission", _) => "allowed by session permission",
                _ => "",
            })
            .unwrap_or("awaiting approval");
        Self {
            at: visible_label(&first.at),
            alias: visible_label(&first.alias),
            status: visible_label(&last.status),
            authorization,
            steps: events
                .iter()
                .map(|e| format!("{} {} {}", visible_label(&e.at), visible_label(&e.action), visible_label(&e.status)))
                .collect::<Vec<_>>()
                .join("  →  "),
            ids: format!("Session ID: {} · Request ID: {}", first.session_id, first.request_id),
            sql: first.sql.as_ref().map(|sql| visible_text(sql).0),
            local: first.local.clone(),
        }
    }
}

fn activity_entry(ui: &mut Ui, entry: &ActivityEntry, intents: &mut Vec<Intent>) {
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(&entry.at).monospace().color(theme::TEXT_MUTED));
        ui.strong(&entry.alias);
        ui.label(RichText::new(&entry.status).color(status_color(&entry.status)));
        ui.label(RichText::new(format!("· {}", entry.authorization)).color(theme::TEXT_MUTED));
    });
    ui.label(RichText::new(&entry.steps).small().color(theme::TEXT_MUTED));
    ui.label(RichText::new(&entry.ids).small().monospace().color(theme::TEXT_MUTED));
    if let Some(sql) = &entry.sql {
        egui::CollapsingHeader::new("SQL")
            .id_salt("sql")
            .default_open(true)
            .show(ui, |ui| {
                immutable_text(ui, "activity_sql", sql, 160.0, true);
                if let Some(local) = &entry.local
                    && ui.button("Open in Query Console").clicked()
                {
                    intents.push(Intent::OpenInConsole(local.clone(), sql.clone()));
                }
            });
    }
}

fn share_row(ui: &mut Ui, choice: &mut DocumentShare) -> bool {
    let mut changed = false;
    ui.horizontal_wrapped(|ui| {
        let valid = valid_alias(&choice.alias);
        changed |= ui
            .add_enabled(
                valid,
                egui::Checkbox::new(&mut choice.shared, "Share with agents"),
            )
            .changed();
        let label = ui.label("Agent alias:");
        changed |= ui
            .add(
                egui::TextEdit::singleline(&mut choice.alias)
                    .desired_width(240.0)
                    .char_limit(80),
            )
            .labelled_by(label.id)
            .changed()
            && choice.shared;
    });
    if !valid_alias(&choice.alias) {
        changed |= choice.shared;
        choice.shared = false;
        invalid_alias_hint(ui);
    }
    changed
}

fn invalid_alias_hint(ui: &mut Ui) {
    ui.label(
        RichText::new("Enter a short, non-empty alias without invisible control characters.")
            .small()
            .color(theme::RED),
    );
}

/// Reports a change once editing ends (focus lost or Enter), not per key: a
/// saved alias is written to the Keychain and renames the shared source.
fn alias_row(ui: &mut Ui, alias: &mut String) -> bool {
    let changed = ui
        .horizontal_wrapped(|ui| {
            let label = ui.label("Agent alias:");
            let response = ui
                .add(
                    egui::TextEdit::singleline(alias)
                        .desired_width(240.0)
                        .char_limit(80),
                )
                .labelled_by(label.id);
            let edited = response.id.with("edited");
            if response.changed() {
                ui.data_mut(|data| data.insert_temp(edited, true));
            }
            response.lost_focus() && ui.data_mut(|data| data.remove_temp::<bool>(edited).unwrap_or(false))
        })
        .inner;
    if !valid_alias(alias) {
        invalid_alias_hint(ui);
    }
    changed
}

fn level_of(access: Option<SourceAccess>) -> ShareLevel {
    match access {
        None => ShareLevel::NotShared,
        Some(SourceAccess::Ask) => ShareLevel::Ask,
        Some(SourceAccess::Read) => ShareLevel::Read,
        Some(SourceAccess::ReadWrite) => ShareLevel::ReadWrite,
    }
}

/// One row per audience (All Agents, then each connection) with its database
/// access, followed by the tables shared with that audience.
fn audience_rows(
    ui: &mut Ui,
    choice: &mut ShareChoice,
    clients: &[(Option<String>, String)],
    read_only: bool,
) -> bool {
    let mut changed = false;
    egui::Grid::new("audiences")
        .num_columns(2)
        .striped(true)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            for (client_id, label) in clients {
                ui.label(label);
                let current = level_of(choice.audience(client_id.as_deref()).map(|a| a.access));
                let mut selected = current;
                egui::ComboBox::from_id_salt(("database_access", client_id))
                    .selected_text(current.label())
                    .width(260.0)
                    .show_ui(ui, |ui| {
                        for level in [
                            ShareLevel::NotShared,
                            ShareLevel::Ask,
                            ShareLevel::Read,
                            ShareLevel::ReadWrite,
                        ] {
                            let enabled = !(read_only && level == ShareLevel::ReadWrite);
                            if ui
                                .add_enabled(
                                    enabled,
                                    egui::Button::selectable(current == level, level.label()),
                                )
                                .clicked()
                            {
                                selected = level;
                            }
                        }
                    });
                ui.end_row();
                if selected != current {
                    changed = true;
                    let position = choice
                        .audiences
                        .iter()
                        .position(|a| &a.client_id == client_id);
                    let access = match selected {
                        ShareLevel::Ask => Some(SourceAccess::Ask),
                        ShareLevel::Read => Some(SourceAccess::Read),
                        ShareLevel::ReadWrite => Some(SourceAccess::ReadWrite),
                        ShareLevel::NotShared => None,
                    };
                    match (access, position) {
                        (None, Some(index)) => {
                            choice.audiences.remove(index);
                        }
                        (Some(access), Some(index)) => choice.audiences[index].access = access,
                        (Some(access), None) => choice.audiences.push(Audience {
                            client_id: client_id.clone(),
                            access,
                            tables: Vec::new(),
                        }),
                        (None, None) => {}
                    }
                }
            }
        });
    if choice
        .audiences
        .iter()
        .any(|a| a.access == SourceAccess::ReadWrite)
    {
        ui.label(RichText::new("Read and Write Access lets that agent run any SQL with this connection's privileges without asking, including modifying or deleting data.").color(theme::RED));
    }
    let mut table_write = false;
    for audience in &mut choice.audiences {
        if audience.tables.is_empty() {
            continue;
        }
        let label = clients
            .iter()
            .find(|(id, _)| *id == audience.client_id)
            .map_or("Agent", |(_, label)| label.as_str());
        ui.label(format!("Tables shared with {label}:"));
        let mut remove = None;
        for (index, (table, access)) in audience.tables.iter_mut().enumerate() {
            ui.push_id(("table", &audience.client_id, index), |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.add_space(12.0);
                    ui.label(
                        RichText::new(format!(
                            "{}.{}",
                            visible_label(&table.schema),
                            visible_label(&table.name)
                        ))
                        .monospace(),
                    );
                    egui::ComboBox::from_id_salt("table_access")
                        .selected_text(table_access_label(*access))
                        .width(200.0)
                        .show_ui(ui, |ui| {
                            for option in [TableAccess::Read, TableAccess::ReadWrite] {
                                let enabled = !(read_only && option == TableAccess::ReadWrite);
                                if ui
                                    .add_enabled(
                                        enabled,
                                        egui::Button::selectable(
                                            *access == option,
                                            table_access_label(option),
                                        ),
                                    )
                                    .clicked()
                                    && *access != option
                                {
                                    *access = option;
                                    changed = true;
                                }
                            }
                        });
                    if ui.button("Stop Sharing Table").clicked() {
                        remove = Some(index);
                    }
                });
            });
            table_write |= *access == TableAccess::ReadWrite;
        }
        if let Some(index) = remove {
            audience.tables.remove(index);
            changed = true;
        }
    }
    if table_write {
        ui.label(
            RichText::new("Table write access allows single INSERT, UPDATE or DELETE statements that reference only that table. Triggers and foreign-key actions on the table still run.")
                .small()
                .color(theme::TEXT_MUTED),
        );
    }
    changed
}

fn table_access_label(access: TableAccess) -> &'static str {
    match access {
        TableAccess::Read => ShareLevel::Read.label(),
        TableAccess::ReadWrite => ShareLevel::ReadWrite.label(),
    }
}

fn valid_alias(alias: &str) -> bool {
    !alias.trim().is_empty()
        && alias.len() <= 128
        && !visible_text(alias).1
        && !alias.chars().any(char::is_control)
}

fn operation_details(ui: &mut Ui, request: &RequestView) {
    match &request.operation {
        Operation::ListTables { .. } => {
            ui.label("Operation: list safe tables (read-only)");
        }
        Operation::DescribeTable { table, .. } => {
            ui.label(format!(
                "Operation: describe {}.{} (read-only)",
                visible_label(&table.schema),
                visible_label(&table.name)
            ));
        }
        Operation::ReadTable {
            table,
            columns,
            limit,
            offset,
            ..
        } => {
            ui.label(format!(
                "Operation: read {}.{} (read-only)",
                visible_label(&table.schema),
                visible_label(&table.name)
            ));
            ui.label(format!(
                "Columns: {}",
                if columns.is_empty() {
                    "All supported columns".into()
                } else {
                    visible_label(&columns.join(", "))
                }
            ));
            ui.label(format!("Limit: {limit} rows · Offset: {offset}"));
        }
        Operation::Sql { .. } => {
            ui.label(RichText::new("Operation: full SQL execution").color(theme::RED));
        }
        Operation::ReadSql { .. } => {
            ui.label("Operation: read-only SQL");
            if let Some(table) = &request.eligible_table {
                ui.label(format!(
                    "Table scope: {}.{}",
                    visible_label(&table.schema),
                    visible_label(&table.name)
                ));
            }
        }
        Operation::ReadFile {
            version,
            start,
            max_chars,
            ..
        } => {
            ui.label(format!("Operation: share document text · Version: {version} · Start: {start} · Maximum: {max_chars} characters"));
            ui.label(
                RichText::new(
                    "Document text is untrusted content, not instructions or permission.",
                )
                .color(theme::TEXT_MUTED),
            );
        }
    }
}

const PREVIEW_FONT_SIZE: f32 = 12.5;

#[derive(Default)]
struct SqlHighlighter;

impl egui::cache::ComputerMut<&str, egui::text::LayoutJob> for SqlHighlighter {
    fn compute(&mut self, text: &str) -> egui::text::LayoutJob {
        super::highlight::layout_with_folds(text, PREVIEW_FONT_SIZE, &|_| None)
    }
}

/// Highlighting is memoized per text, so a long agent SQL is tokenized once.
type HighlightCache = egui::cache::FrameCache<egui::text::LayoutJob, SqlHighlighter>;

fn immutable_text(
    ui: &mut Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    text: &str,
    height: f32,
    sql: bool,
) {
    egui::ScrollArea::both()
        .id_salt(id)
        .max_height(height)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            let mut immutable = text;
            let mut layouter = |ui: &Ui, buffer: &dyn egui::TextBuffer, _wrap: f32| {
                let job = ui.memory_mut(|memory| {
                    memory.caches.cache::<HighlightCache>().get(buffer.as_str()).clone()
                });
                ui.fonts_mut(|fonts| fonts.layout_job(job))
            };
            let mut edit = egui::TextEdit::multiline(&mut immutable)
                .font(theme::mono(PREVIEW_FONT_SIZE))
                .desired_width(f32::INFINITY)
                .desired_rows(3)
                .code_editor();
            if sql {
                edit = edit.layouter(&mut layouter);
            }
            ui.add(edit);
        });
}

/// Preserve all content while making invisible directional/control text explicit.
fn visible_label(text: &str) -> String {
    visible_characters(text, false).0
}

fn visible_text(text: &str) -> (String, bool) {
    visible_characters(text, true)
}

fn visible_characters(text: &str, preserve_layout: bool) -> (String, bool) {
    let mut escaped = false;
    let mut visible = String::with_capacity(text.len());
    for ch in text.chars() {
        if (ch.is_control() && !(preserve_layout && matches!(ch, '\n' | '\t')))
            || matches!(ch, '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
        {
            use std::fmt::Write;
            let _ = write!(visible, "\\u{{{:04x}}}", ch as u32);
            escaped = true;
        } else {
            visible.push(ch);
        }
    }
    (visible, escaped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Key, accesskit::Role};
    use egui_kittest::{
        Harness,
        kittest::{NodeT, Queryable},
    };

    #[test]
    fn untrusted_labels_and_sql_show_hidden_controls() {
        let (shown, escaped) = visible_text("Agent\u{202e}admin\nSELECT\t1\0");
        assert!(escaped);
        assert_eq!(
            visible_label("Agent\nSession ID: spoof"),
            "Agent\\u{000a}Session ID: spoof"
        );
        assert_eq!(shown, "Agent\\u{202e}admin\nSELECT\t1\\u{0000}");
        assert_eq!(
            visible_text("SELECT 'safe'\nFROM data"),
            ("SELECT 'safe'\nFROM data".into(), false)
        );
    }

    struct ReviewFixture {
        ui: McpUi,
        intents: Vec<Intent>,
        styled: bool,
    }

    fn request(operation: Operation) -> RequestView {
        let eligible_table = operation.table().cloned();
        RequestView {
            id: "d242a9c7-030c-4f52-904f-b0254b98834d".into(),
            session_id: "034f2765-03a2-4fde-8fb8-cf413bf6e3e0".into(),
            client_label: "Research agent".into(),
            alias: "Database 1".into(),
            operation,
            preview: "SELECT \"id\", \"name\" FROM \"public\".\"cities\" LIMIT 25 OFFSET 0".into(),
            eligible_table,
            status: "pending".into(),
            can_allow_full_sql: true,
            local: Some(LocalTarget {
                source_id: "local-source".into(),
                database: "shop".into(),
            }),
        }
    }

    fn review_fixture(request: RequestView) -> ReviewFixture {
        ReviewFixture {
            ui: McpUi {
                snapshot: Arc::new(UiSnapshot {
                    requests: vec![request],
                    ..Default::default()
                }),
                ..Default::default()
            },
            intents: Vec::new(),
            styled: false,
        }
    }

    fn render_review(ui: &mut Ui, fixture: &mut ReviewFixture) {
        if !fixture.styled {
            theme::apply(ui.ctx());
            fixture.styled = true;
            ui.ctx().request_repaint();
            return;
        }
        fixture.ui.approval(ui.ctx(), &mut fixture.intents);
    }

    fn strict_read() -> Operation {
        Operation::ReadTable {
            source_id: "opaque-source".into(),
            table: crate::mcp::TableRef {
                schema: "public".into(),
                name: "cities".into(),
            },
            columns: vec!["id".into(), "name".into()],
            limit: 25,
            offset: 0,
        }
    }

    #[test]
    fn approval_uses_immutable_preview_and_exact_request_and_scope() {
        let expected = request(strict_read());
        let mut h = Harness::builder()
            .with_size([1100.0, 800.0])
            .with_pixels_per_point(1.5)
            .wgpu()
            .build_ui_state(render_review, review_fixture(expected.clone()));
        h.run_steps(4);
        assert_eq!(
            h.get_by_role(Role::MultilineTextInput).value().as_deref(),
            Some(expected.preview.as_str())
        );
        assert!(
            h.query_by_label(&format!("Session ID: {}", expected.session_id))
                .is_some()
        );
        h.state_mut().ui.review.as_mut().unwrap().opened = Instant::now();
        h.run_steps(1);
        assert!(h.get_by_label("Allow Once").accesskit_node().is_disabled());
        h.state_mut().ui.review.as_mut().unwrap().opened = Instant::now() - Duration::from_secs(1);
        h.run_steps(2);
        h.render()
            .unwrap()
            .save("docs/screenshots/86-mcp-approval.png")
            .unwrap();
        assert!(h.query_by_label("Deny and Open in Query Console").is_some());
        h.get_by_label("Read this table for this session").click();
        h.run_steps(2);
        h.get_by_label("Allow for Session").click();
        h.run_steps(2);
        assert!(h.state().intents.iter().any(|intent| matches!(intent, Intent::Decision(id, Approval::ReadTableForSession) if id == &expected.id)));
        assert_eq!(h.state().ui.snapshot.requests[0].preview, expected.preview);
    }

    #[test]
    fn full_sql_needs_explicit_acknowledgement_and_small_prompt_keeps_deny_visible() {
        let sql = "DELETE FROM orders WHERE id = 7";
        let mut pending = request(Operation::Sql {
            source_id: "opaque-source".into(),
            sql: sql.into(),
        });
        pending.preview = sql.into();
        let mut h = Harness::builder()
            .with_size([740.0, 500.0])
            .with_pixels_per_point(1.5)
            .wgpu()
            .build_ui_state(render_review, review_fixture(pending.clone()));
        h.run_steps(4);
        h.state_mut().ui.review.as_mut().unwrap().opened = Instant::now() - Duration::from_secs(1);
        h.run_steps(2);
        assert!(h.get_by_label("Allow Once").accesskit_node().is_disabled());
        let window = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(740.0, 500.0));
        assert!(window.contains_rect(h.get_by_label("Deny").rect()));
        h.get_by_label("Full SQL access for this session")
            .scroll_to_me();
        h.run_steps(15);
        h.get_by_label("Full SQL access for this session").click();
        h.run_steps(2);
        assert!(
            h.get_by_label("Allow for Session")
                .accesskit_node()
                .is_disabled()
        );
        h.get_by_label("I understand and allow the full SQL capability")
            .scroll_to_me();
        h.run_steps(15);
        h.get_by_label("I understand and allow the full SQL capability")
            .click();
        h.run_steps(2);
        h.render()
            .unwrap()
            .save("docs/screenshots/87-mcp-approval-small.png")
            .unwrap();
        h.get_by_label("Allow for Session").click();
        h.run_steps(2);
        assert!(h.state().intents.iter().any(|intent| matches!(intent, Intent::Decision(id, Approval::FullSqlForSession) if id == &pending.id)));
    }

    #[test]
    fn readonly_full_sql_is_blocked_and_escape_denies() {
        let mut pending = request(Operation::Sql {
            source_id: "opaque-source".into(),
            sql: "SELECT 1".into(),
        });
        pending.can_allow_full_sql = false;
        let mut h = Harness::builder()
            .with_size([1000.0, 700.0])
            .build_ui_state(render_review, review_fixture(pending.clone()));
        h.run_steps(4);
        h.state_mut().ui.review.as_mut().unwrap().opened = Instant::now() - Duration::from_secs(1);
        h.run_steps(2);
        assert!(
            h.query_by_label("Full SQL access for this session")
                .is_none()
        );
        assert!(h.get_by_label("Allow Once").accesskit_node().is_disabled());
        h.key_press(Key::Escape);
        h.run_steps(2);
        assert!(h.state().intents.iter().any(
            |intent| matches!(intent, Intent::Decision(id, Approval::Deny) if id == &pending.id)
        ));
    }

    #[test]
    fn new_request_resets_scope_and_enter_cannot_approve_it() {
        let pending = request(strict_read());
        let mut h = Harness::builder()
            .with_size([1000.0, 700.0])
            .build_ui_state(render_review, review_fixture(pending));
        h.run_steps(4);
        h.state_mut().ui.review.as_mut().unwrap().scope = Approval::ReadSourceForSession;
        let state = h.state_mut();
        let snapshot = Arc::make_mut(&mut state.ui.snapshot);
        snapshot.requests[0].id = "new-request".into();
        snapshot.requests[0].preview = "SELECT id FROM other_table LIMIT 1".into();
        h.run_steps(2);
        assert_eq!(h.state().ui.review.as_ref().unwrap().scope, Approval::Once);
        h.key_press(Key::Enter);
        h.run_steps(2);
        assert!(!h.state().intents.iter().any(|intent| matches!(
            intent,
            Intent::Decision(
                _,
                Approval::Once
                    | Approval::ReadTableForSession
                    | Approval::ReadSourceForSession
                    | Approval::FullSqlForSession
            )
        )));
    }

    #[test]
    fn new_pending_request_cannot_replace_the_reviewed_operation() {
        let original = request(strict_read());
        let mut h = Harness::builder()
            .with_size([1000.0, 700.0])
            .build_ui_state(render_review, review_fixture(original.clone()));
        h.run_steps(4);
        let mut additional = request(Operation::ReadSql {
            source_id: "opaque-source".into(),
            sql: "SELECT id FROM other_table".into(),
        });
        additional.id = "new-earlier-id".into();
        additional.preview = "SELECT id FROM other_table".into();
        Arc::make_mut(&mut h.state_mut().ui.snapshot)
            .requests
            .insert(0, additional);
        h.run_steps(2);
        assert_eq!(
            h.state().ui.review.as_ref().unwrap().request_id,
            original.id
        );
        assert_eq!(
            h.get_by_role(Role::MultilineTextInput).value().as_deref(),
            Some(original.preview.as_str())
        );
        // Rare actions are in the More menu of the footer.
        h.get_by_label("More").click();
        h.run_steps(2);
        h.get_by_label("End This Session").click();
        h.run_steps(2);
        assert!(
            h.state().intents.iter().any(
                |intent| matches!(intent, Intent::EndSession(id) if id == &original.session_id)
            )
        );
    }

    #[test]
    fn read_sql_only_offers_table_scope_when_the_broker_verified_one_table() {
        let sql = "SELECT c.name, COUNT(*) FROM cities c JOIN orders o ON c.id = o.city_id GROUP BY c.name";
        let mut pending = request(Operation::ReadSql {
            source_id: "opaque-source".into(),
            sql: sql.into(),
        });
        pending.preview = sql.into();
        let mut h = Harness::builder()
            .with_size([1000.0, 700.0])
            .build_ui_state(render_review, review_fixture(pending));
        h.run_steps(4);
        assert!(h.query_by_label("Operation: read-only SQL").is_some());
        assert!(
            h.query_by_label("Read this table for this session")
                .is_none()
        );
        assert!(
            h.query_by_label("Read this database for this session")
                .is_some()
        );
        assert!(
            h.query_by_label("Full SQL access for this session")
                .is_none()
        );
    }

    #[test]
    fn unchanged_shared_documents_reuse_text_and_close_removes_them() {
        let ctx = egui::Context::default();
        let mut app = super::super::actions::tests::isolated_app(&ctx);
        let index = app.add_tab(
            "Private title".into(),
            "source".into(),
            TabKind::Console {
                sql: "SELECT 1".into(),
            },
        );
        let tab = &app.tabs[index];
        let id = tab.id;
        let text: Arc<str> = Arc::from("SELECT 1");
        app.mcp.documents.insert(
            id,
            DocumentChoice {
                choice: DocumentShare {
                    shared: true,
                    alias: "Document 1".into(),
                },
                id: "private-document-id".into(),
                revision: tab.editor_cache.source_revision(),
                version: 1,
                text: Arc::clone(&text),
            },
        );
        for _ in 0..100 {
            app.refresh_mcp_documents();
        }
        assert!(Arc::ptr_eq(&app.mcp.documents[&id].text, &text));
        assert_eq!(app.mcp.documents[&id].version, 1);
        app.tabs.clear();
        app.refresh_mcp_documents();
        assert!(app.mcp.documents.is_empty());
    }

    #[test]
    fn sessions_revoke_only_the_selected_grant() {
        let session_id = "034f2765-03a2-4fde-8fb8-cf413bf6e3e0";
        let state = McpUi {
            visible: true,
            section: Section::Sessions,
            snapshot: Arc::new(UiSnapshot {
                running: true,
                sessions: vec![crate::mcp::SessionView {
                    id: session_id.into(),
                    client_id: "client".into(),
                    label: "Research agent".into(),
                    expires_at: std::time::Instant::now() + std::time::Duration::from_secs(1170),
                    grants: vec![crate::mcp::GrantView {
                        id: "grant-cities".into(),
                        alias: "Database 1".into(),
                        scope: "Read table public.cities".into(),
                    }],
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut h = Harness::builder()
            .with_size([1100.0, 720.0])
            .with_pixels_per_point(1.5)
            .wgpu()
            .build_ui_state(
                |ui, fixture: &mut ReviewFixture| {
                    if !fixture.styled {
                        theme::apply(ui.ctx());
                        fixture.styled = true;
                        ui.ctx().request_repaint();
                        return;
                    }
                    fixture
                        .ui
                        .settings(ui.ctx(), &[], &[], &mut fixture.intents);
                },
                ReviewFixture {
                    ui: state,
                    intents: Vec::new(),
                    styled: false,
                },
            );
        h.run_steps(4);
        h.render()
            .unwrap()
            .save("docs/screenshots/88-mcp-sessions.png")
            .unwrap();
        h.get_by_label("Revoke Permission").click();
        h.run_steps(2);
        assert!(h.state().intents.iter().any(|intent| matches!(intent, Intent::RevokeGrant(session, grant) if session == session_id && grant == "grant-cities")));
        h.set_size(egui::vec2(740.0, 500.0));
        h.run_steps(4);
        h.render()
            .unwrap()
            .save("docs/screenshots/89-mcp-sessions-small.png")
            .unwrap();
        h.get_by_label("End Session").click();
        h.run_steps(2);
        assert!(
            h.state().intents.iter().any(
                |intent| matches!(intent, Intent::EndSession(session) if session == session_id)
            )
        );
    }

    fn postgres_app(ctx: &egui::Context) -> App {
        let mut app = super::super::actions::tests::isolated_app(ctx);
        let mut source = crate::model::DataSource::new(crate::model::DriverKind::Postgres);
        source.id = "pg-local".into();
        source.database = "shop".into();
        app.config.data_sources.push(source);
        app
    }

    #[test]
    fn explorer_shares_databases_and_tables_with_standing_access() {
        use crate::model::{DbObject, ObjectKind};
        let ctx = egui::Context::default();
        let mut app = postgres_app(&ctx);
        let source = app.config.data_sources[0].clone();
        let explorer = super::super::explorer::share_target;
        let table = DbObject::new(ObjectKind::Table, "orders", &["shop".into(), "sales".into()]);
        let other_db = DbObject::new(ObjectKind::Database, "archive", &[]);
        let column = DbObject::new(ObjectKind::Column, "id", &table.path);
        let table_target = explorer(&source, Some(&table)).unwrap();
        assert_eq!(
            table_target,
            ShareTarget::Table(
                ShareKey {
                    source_id: "pg-local".into(),
                    database: None,
                },
                TableRef {
                    schema: "sales".into(),
                    name: "orders".into(),
                },
            )
        );
        let archive = explorer(&source, Some(&other_db)).unwrap();
        assert!(matches!(&archive, ShareTarget::Database(key) if key.database.as_deref() == Some("archive")));
        assert!(explorer(&source, Some(&column)).is_none());

        let claude = app
            .backend
            .mcp
            .create_client("Claude Code".into(), crate::mcp::DEFAULT_PORT)
            .unwrap()
            .id;
        let codex = app
            .backend
            .mcp
            .create_client("Codex".into(), crate::mcp::DEFAULT_PORT)
            .unwrap()
            .id;
        app.mcp_changed();
        let claude_id = Some(claude.as_str());
        app.set_mcp_share(table_target.clone(), Some(claude.clone()), ShareLevel::ReadWrite);
        assert_eq!(app.mcp_share_level(&table_target, claude_id), ShareLevel::ReadWrite);
        // Other connections and All Agents are unaffected.
        assert_eq!(app.mcp_share_level(&table_target, Some(&codex)), ShareLevel::NotShared);
        assert_eq!(app.mcp_share_level(&table_target, None), ShareLevel::NotShared);
        let root = explorer(&source, None).unwrap();
        // A single table lists the database without database-wide access.
        assert_eq!(app.mcp_share_level(&root, claude_id), ShareLevel::Ask);
        app.set_mcp_share(root.clone(), Some(codex.clone()), ShareLevel::Read);
        assert_eq!(app.mcp_share_level(&root, Some(&codex)), ShareLevel::Read);
        app.set_mcp_share(archive.clone(), None, ShareLevel::ReadWrite);
        assert_eq!(app.mcp_share_level(&archive, None), ShareLevel::ReadWrite);
        app.set_mcp_share(table_target.clone(), Some(claude.clone()), ShareLevel::NotShared);
        assert_eq!(app.mcp_share_level(&table_target, claude_id), ShareLevel::NotShared);
        assert_eq!(app.mcp_share_level(&root, claude_id), ShareLevel::Ask);
        // An unknown connection never receives access.
        app.set_mcp_share(root.clone(), Some("unknown".into()), ShareLevel::ReadWrite);
        assert_eq!(app.mcp_share_level(&root, Some("unknown")), ShareLevel::NotShared);

        app.config.data_sources[0].read_only = true;
        app.set_mcp_share(table_target.clone(), Some(claude.clone()), ShareLevel::ReadWrite);
        assert_eq!(app.mcp_share_level(&table_target, claude_id), ShareLevel::Read);
        app.mcp_unshare_source("pg-local");
        assert_eq!(app.mcp_share_level(&root, Some(&codex)), ShareLevel::NotShared);
    }

    #[test]
    fn remembered_state_restores_connections_and_per_agent_sharing() {
        let ctx = egui::Context::default();
        let app = postgres_app(&ctx);
        let connection = app
            .backend
            .mcp
            .create_client("Claude Code".into(), crate::mcp::DEFAULT_PORT)
            .unwrap();
        let state = McpState {
            clients: app.backend.mcp.stored_clients(),
            shares: vec![StoredShare {
                source_id: "pg-local".into(),
                database: None,
                alias: "Sales".into(),
                audiences: vec![
                    StoredAudience {
                        client_id: Some(connection.id.clone()),
                        access: SourceAccess::Read,
                        tables: vec![StoredTable {
                            schema: "sales".into(),
                            name: "orders".into(),
                            access: TableAccess::ReadWrite,
                        }],
                    },
                    StoredAudience {
                        client_id: Some("revoked-connection".into()),
                        access: SourceAccess::ReadWrite,
                        tables: Vec::new(),
                    },
                ],
            }],
        };
        let mut restarted = postgres_app(&ctx);
        restarted.mcp_state_loaded(Ok(Some(state)));
        assert_eq!(restarted.mcp.persist, Persist::Ready);
        assert_eq!(restarted.mcp_clients().len(), 1);
        let key = ShareKey {
            source_id: "pg-local".into(),
            database: None,
        };
        let choice = &restarted.mcp.sources[&key];
        assert_eq!(choice.audiences.len(), 1, "audience of a missing connection is dropped");
        assert_eq!(
            restarted.mcp_share_level(&ShareTarget::Database(key), Some(&connection.id)),
            ShareLevel::Read
        );
        let mut failed = postgres_app(&ctx);
        failed.mcp_state_loaded(Err("denied".into()));
        assert_eq!(failed.mcp.persist, Persist::Failed);
        assert!(failed.mcp.error.as_deref().unwrap().contains("until restart"));
    }

    #[test]
    fn agent_sql_opens_in_a_console_on_the_requested_database() {
        let ctx = egui::Context::default();
        let mut app = postgres_app(&ctx);
        app.mcp.visible = true;
        let sql = "UPDATE sales.orders SET status = 'ready' WHERE id = 42";
        app.open_mcp_sql_in_console(
            &LocalTarget {
                source_id: "pg-local".into(),
                database: "archive".into(),
            },
            sql.into(),
        );
        let tab = app.tabs.last().unwrap();
        assert!(matches!(&tab.kind, TabKind::Console { sql: text } if text == sql));
        assert_eq!(tab.ds_id, "pg-local");
        assert_eq!(tab.database.as_deref(), Some("archive"));
        assert!(!app.mcp.visible);
    }

    #[test]
    fn deny_and_open_sends_the_visible_sql_and_activity_shows_sql() {
        let sql = "DELETE FROM sales.orders WHERE status = 'draft'";
        let mut pending = request(Operation::Sql {
            source_id: "opaque-source".into(),
            sql: sql.into(),
        });
        pending.preview = sql.into();
        let mut h = Harness::builder()
            .with_size([1000.0, 700.0])
            .build_ui_state(render_review, review_fixture(pending.clone()));
        h.run_steps(4);
        h.get_by_label("Deny and Open in Query Console").click();
        h.run_steps(2);
        assert!(h.state().intents.iter().any(|intent| matches!(
            intent,
            Intent::DenyAndOpen(id, target, text)
                if id == &pending.id && target.source_id == "local-source" && text == sql
        )));

        let mut access = ShareChoice::new("Sales".into());
        access.audiences.push(Audience {
            client_id: None,
            access: SourceAccess::Ask,
            tables: Vec::new(),
        });
        access.audiences.push(Audience {
            client_id: Some("claude".into()),
            access: SourceAccess::Read,
            tables: vec![(
                TableRef {
                    schema: "sales".into(),
                    name: "orders".into(),
                },
                TableAccess::ReadWrite,
            )],
        });
        let key = ShareKey {
            source_id: "local-source".into(),
            database: None,
        };
        let audit = AuditView {
            at: "09:41:00".into(),
            session_id: pending.session_id.clone(),
            request_id: pending.id.clone(),
            alias: "Sales".into(),
            action: "request".into(),
            status: "pending".into(),
            sql: Some(Arc::from(sql)),
            local: pending.local.clone(),
        };
        let state = McpUi {
            visible: true,
            section: Section::Sharing,
            sources: HashMap::from([(key.clone(), access)]),
            snapshot: Arc::new(UiSnapshot {
                running: true,
                audit: vec![
                    AuditView {
                        at: "09:41:01".into(),
                        action: "result".into(),
                        status: "completed".into(),
                        ..audit.clone()
                    },
                    AuditView {
                        action: "shared access".into(),
                        status: "running".into(),
                        ..audit.clone()
                    },
                    audit,
                ],
                clients: vec![
                    crate::mcp::ClientView {
                        id: "claude".into(),
                        label: "Claude Code".into(),
                    },
                    crate::mcp::ClientView {
                        id: "codex".into(),
                        label: "Codex".into(),
                    },
                ],
                ..Default::default()
            }),
            ..Default::default()
        };
        let resources = [LocalResource {
            key,
            label: "Sales (production)".into(),
            kind: "Database",
            read_only: false,
        }];
        let mut h = Harness::builder()
            .with_size([1280.0, 800.0])
            .with_pixels_per_point(1.5)
            .wgpu()
            .build_ui_state(
                move |ui, fixture: &mut ReviewFixture| {
                    if !fixture.styled {
                        theme::apply(ui.ctx());
                        fixture.styled = true;
                        ui.ctx().request_repaint();
                        return;
                    }
                    fixture
                        .ui
                        .settings(ui.ctx(), &resources, &[], &mut fixture.intents);
                },
                ReviewFixture {
                    ui: state,
                    intents: Vec::new(),
                    styled: false,
                },
            );
        h.run_steps(4);
        assert!(h.query_by_label("sales.orders").is_some());
        h.render()
            .unwrap()
            .save("docs/screenshots/90-mcp-sharing-access.png")
            .unwrap();
        h.get_by_label("Activity").click();
        h.run_steps(4);
        assert_eq!(
            h.get_by_role(Role::MultilineTextInput).value().as_deref(),
            Some(sql)
        );
        h.render()
            .unwrap()
            .save("docs/screenshots/91-mcp-activity-sql.png")
            .unwrap();
        h.get_by_label("Open in Query Console").click();
        h.run_steps(2);
        assert!(h.state().intents.iter().any(
            |intent| matches!(intent, Intent::OpenInConsole(target, text) if target.source_id == "local-source" && text == sql)
        ));
    }

    #[test]
    fn autostart_starts_the_server_after_the_configuration_loads() {
        let ctx = egui::Context::default();
        let mut app = super::super::actions::tests::isolated_app(&ctx);
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        app.config.mcp_port = port;
        app.load_mcp_state();
        assert!(!app.backend.mcp.snapshot().starting && !app.backend.mcp.snapshot().running);
        app.config.mcp_autostart = true;
        app.load_mcp_state();
        let snapshot = app.backend.mcp.snapshot();
        assert!(snapshot.starting || snapshot.running);
        app.backend.mcp.stop();
    }

    #[test]
    fn activity_groups_lifecycle_events_into_one_entry_per_request() {
        let event = |request: &str, at: &str, action: &str| AuditView {
            at: at.into(),
            session_id: "s".into(),
            request_id: request.into(),
            alias: "Sales".into(),
            action: action.into(),
            status: "x".into(),
            sql: Some(Arc::from("SELECT 1")),
            local: None,
        };
        // Newest first, as the broker publishes it.
        let audit = vec![
            event("b", "10:00:03", "request"),
            event("a", "10:00:02", "result"),
            event("a", "10:00:01", "shared access"),
            event("a", "10:00:00", "request"),
        ];
        let groups = group_by_request(&audit);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0][0].request_id, "b");
        let steps: Vec<_> = groups[1].iter().map(|e| e.action.as_str()).collect();
        assert_eq!(steps, ["request", "shared access", "result"]);
    }
}
