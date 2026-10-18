//! Runs database work on a tokio runtime and reports back to the UI thread
//! through a channel, waking egui after each event.

mod activity;
mod compare;
mod ddl_diff;
mod explain;
mod related;
mod dump;
mod import;
pub use dump::{ToolJob, ToolTask};
mod transfer;
pub use import::{ImportJob, OnError};
pub use transfer::{ExportItem, ExportJob, LogLine, PREVIEW_ROWS, Progress, TaskEvent, TaskId, bytes_text, elapsed_text, rows_text};
pub use ddl_diff::{DdlObjectRequest, StructureCompareRequest};
pub use explain::{ExplainCompletion, ExplainExecution, ExplainRequest};
pub use related::{BoundRunRequest, RelatedMetadataRequest, RelatedSourceGuard};

use crate::drivers::{CancelHandle, Connection, begin_statement};
use crate::export;
use crate::files::{self, OpenedFile};
use crate::model::{
    BoundUpdate, CompletionMetadata, DataSource, DbObject, DriverKind, ExecutionContext,
    MetadataKey, MutationFailure, MutationReport, OriginTable, Page, QueryResult, ResolvedContext,
    ResultEditMetadata, ResultSource, SchemaSelection, SourceIdentity, TableEditMetadata,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use tokio::runtime::Runtime;

pub enum Event {
    McpChanged,
    McpStateLoaded(Result<Option<crate::mcp::McpState>, String>),
    McpStateSaved(Result<(), String>),
    ConfigLoaded(Result<crate::persistence::Loaded, String>),
    ConfigSaved {
        apply: bool,
        result: Result<crate::persistence::Loaded, String>,
    },
    TestDone {
        ds_id: String,
        request_id: String,
        result: Result<String, String>,
    },
    Connected {
        ds_id: String,
        result: Result<String, String>,
    },
    Children {
        ds_id: String,
        path: Option<Vec<String>>,
        result: Result<Vec<DbObject>, String>,
    },
    StatementDone {
        console_id: u64,
        sql: String,
        page: Page,
        result: Result<QueryResult, String>,
    },
    /// An informational line for the console's Output log.
    Notice {
        console_id: u64,
        text: String,
    },
    RunFinished {
        console_id: u64,
        /// Whether the console session is inside an open transaction afterwards.
        in_tx: bool,
    },
    Completion {
        key: MetadataKey,
        request_id: u64,
        result: Result<CompletionMetadata, String>,
    },
    SchemaChanged {
        console_id: u64,
        ds_id: String,
        revision: u64,
        requested: ExecutionContext,
        result: Result<ResolvedContext, String>,
    },
    Ddl {
        ds_id: String,
        title: String,
        result: Result<String, String>,
    },
    /// First rows for the preview of the Export or Import dialog.
    TransferPreview {
        request: u64,
        result: Result<QueryResult, String>,
    },
    /// Tables of a schema, with a view flag, for the Export dialog.
    TransferTables {
        request: u64,
        result: Result<Vec<(String, bool)>, String>,
    },
    /// First records of a file for the Import dialog.
    ImportPreview {
        request: u64,
        result: Result<crate::import::CsvPreview, String>,
    },
    /// Columns and types of the import target table.
    ImportColumns {
        request: u64,
        result: Result<Vec<(String, String)>, String>,
    },
    /// A sample of the Activity Monitor.
    ActivitySample {
        monitor: u64,
        request: u64,
        result: Result<Box<crate::activity::Snapshot>, String>,
    },
    /// Outcome of Cancel Query or Terminate Session; `false` when the
    /// session no longer existed.
    ActivitySignal {
        monitor: u64,
        pid: i32,
        terminate: bool,
        result: Result<bool, String>,
    },
    /// `--version` output of a PostgreSQL client tool.
    ToolVersion {
        request: u64,
        result: Result<String, String>,
    },
    /// Progress, log and outcome of a transfer shown in the Run tool window.
    Task {
        id: TaskId,
        event: TaskEvent,
    },
    FileOpened {
        path: PathBuf,
        result: Result<Box<OpenedFile>, String>,
    },
    /// Edited tables of an opened CSV file were written back to it.
    FileSaved {
        console_id: u64,
        result: Result<usize, String>,
    },
    ResultMetadata {
        console_id: u64,
        result_id: u64,
        request_id: u64,
        result: Result<TableEditMetadata, String>,
    },
    UpdatesDone {
        console_id: u64,
        result_id: u64,
        request_id: u64,
        in_tx: bool,
        result: Result<MutationReport, MutationFailure>,
    },
    RelatedMetadata {
        console_id: u64,
        result_id: u64,
        request_id: u64,
        direction: crate::model::RelatedDirection,
        result: Result<crate::model::RelatedMetadata, String>,
    },
    ExplainDone {
        console_id: u64,
        request_id: u64,
        completion: ExplainCompletion,
        result: Result<crate::query_plan::QueryPlan, String>,
    },
    CompareDataDone {
        request_id: u64,
        result: Result<crate::compare::ComparedData, String>,
    },
    StructureCompareDone {
        request_id: u64,
        result: Result<Box<crate::ddl_diff::ComparedDdl>, String>,
    },
    /// Catalog scan of Database Graph with both layouts, computed off the UI thread.
    SchemaGraph {
        request_id: u64,
        result: Result<Box<SchemaGraphLoaded>, String>,
    },
    /// New layout after pinning, for the graph of the same request.
    SchemaGraphLayout {
        request_id: u64,
        flat: bool,
        layout: crate::schema_graph::Layout,
    },
}

/// Layout to compute with a scan: the shown mode, its saved pins by schema
/// and name, and the forces.
#[derive(Clone, Debug, Default)]
pub struct GraphLayoutRequest {
    pub flat: bool,
    pub pins: Vec<(String, String, [f32; 3])>,
    pub params: crate::schema_graph::LayoutParams,
}

pub struct SchemaGraphLoaded {
    pub graph: Arc<crate::schema_graph::SchemaGraph>,
    /// Layout of the mode that was shown when the scan started.
    pub layout: crate::schema_graph::Layout,
    pub flat: bool,
}

pub struct ResultMetadataRequest {
    pub console_id: u64,
    pub result_id: u64,
    pub request_id: u64,
    pub ds: DataSource,
    pub source_identity: SourceIdentity,
    pub expected_context: ResolvedContext,
    pub source: ResultSource,
}

pub struct MutationRequest {
    pub console_id: u64,
    pub result_id: u64,
    pub request_id: u64,
    pub ds: DataSource,
    pub source_identity: SourceIdentity,
    pub expected_context: ResolvedContext,
    pub session_epoch: u64,
    pub metadata: ResultEditMetadata,
    /// DELETE, UPDATE and INSERT statements; each must change exactly one row.
    pub mutations: Vec<BoundUpdate>,
    pub manual_tx: bool,
}

/// What to execute in a console session.
pub struct RunRequest {
    pub console_id: u64,
    pub ds: DataSource,
    pub statements: Vec<String>,
    pub page: Page,
    /// Opens a transaction before the first statement unless one is already open.
    pub manual_tx: bool,
    pub context: ExecutionContext,
    /// Result replay must leave the console's current namespace untouched.
    pub expected_context: Option<ResolvedContext>,
}

/// The table of a table editor tab whose SELECT is paged on the server.
#[derive(Clone, Debug)]
pub struct TablePage {
    pub schema: Option<String>,
    pub table: String,
    /// The SQL has the user's ORDER BY; otherwise the primary key orders pages.
    pub ordered: bool,
}

/// The paged SQL and the window to read from its result. Without an ORDER BY
/// consecutive reads could return the rows of a page in another order, or
/// other rows, so the primary key gives a stable order when there is one.
async fn table_page_query(
    conn: &mut Connection,
    driver: DriverKind,
    table: &TablePage,
    sql: &str,
    page: Page,
) -> (String, Page) {
    // An unqualified PostgreSQL table would be looked up in "public" only.
    let keys = if table.ordered || (driver == DriverKind::Postgres && table.schema.is_none()) {
        Vec::new()
    } else {
        // Without the key the page is still correct, only its order is not fixed.
        conn.primary_key(table.schema.as_deref(), &table.table)
            .await
            .unwrap_or_default()
    };
    crate::drivers::page_query(driver, sql, &keys, page)
}

#[derive(Default)]
struct SessionState {
    conn: Option<Connection>,
    in_tx: bool,
    open_database: Option<String>,
    configured_schema: SchemaSelection,
    source_identity: Option<SourceIdentity>,
    edit_metadata: HashMap<OriginTable, TableEditMetadata>,
    related_metadata:
        HashMap<(OriginTable, crate::model::RelatedDirection), crate::model::RelatedMetadata>,
    /// A failed or ambiguous execution must never silently reconnect and retry.
    reconnect_required: Option<String>,
}

impl SessionState {
    fn ensure_usable(&self) -> anyhow::Result<()> {
        if let Some(reason) = &self.reconnect_required {
            anyhow::bail!("{reason} Disconnect and reconnect the data source before continuing.");
        }
        Ok(())
    }

    fn close_uncertain(&mut self, reason: String) {
        self.conn = None;
        self.in_tx = false;
        self.edit_metadata.clear();
        self.related_metadata.clear();
        self.reconnect_required = Some(reason);
    }

    async fn conn(&mut self, ds: &DataSource) -> anyhow::Result<&mut Connection> {
        anyhow::ensure!(
            !ds.credentials_unavailable,
            "Saved passwords are unavailable. Retry loading or replace them in Data Sources and Drivers."
        );
        self.ensure_usable()?;
        if self.conn.is_none() {
            self.conn = Some(Connection::open(ds).await?);
            self.open_database = MetadataKey::for_source(ds).database;
            self.configured_schema = SchemaSelection::Default;
            self.source_identity = Some(ds.identity());
            self.edit_metadata.clear();
            self.related_metadata.clear();
        }
        anyhow::ensure!(
            self.source_identity.as_ref() == Some(&ds.identity()),
            "The data source definition has changed. Reconnect before executing queries."
        );
        Ok(self.conn.as_mut().unwrap())
    }

    fn validate_result_session(
        &self,
        ds: &DataSource,
        identity: &SourceIdentity,
        epoch: u64,
    ) -> anyhow::Result<()> {
        self.ensure_usable()?;
        anyhow::ensure!(
            self.conn.as_ref().is_some_and(|conn| conn.epoch() == epoch),
            "The result's database session has ended. Execute the query again before editing."
        );
        anyhow::ensure!(
            self.source_identity.as_ref() == Some(identity) && ds.identity() == *identity,
            "The data source definition has changed. Execute the query again before editing."
        );
        Ok(())
    }

    async fn configure(&mut self, ds: &DataSource, schema: &SchemaSelection) -> anyhow::Result<()> {
        self.conn(ds).await?;
        anyhow::ensure!(
            self.open_database == MetadataKey::for_source(ds).database,
            "The console is connected to a different database. Reconnect before executing queries."
        );
        if &self.configured_schema != schema {
            anyhow::ensure!(
                !self.in_tx,
                "Commit or roll back the open transaction before changing the schema."
            );
            self.conn.as_mut().unwrap().change_schema(schema).await?;
            self.configured_schema = schema.clone();
        }
        Ok(())
    }
}

type Session = Arc<tokio::sync::Mutex<SessionState>>;

/// Open Activity Monitors with their idle connection; `None` while a sample
/// uses it or before the first one.
type ActivityConnections = Arc<Mutex<HashMap<u64, Option<Connection>>>>;

pub struct Backend {
    pub mcp: crate::mcp::McpController,
    rt: Runtime,
    persistence: Option<Sender<crate::persistence::Command>>,
    tx: Sender<Event>,
    pub rx: Receiver<Event>,
    ctx: egui::Context,
    /// Keyed by (data source id, session name): "meta" for the explorer, console id otherwise.
    sessions: Arc<Mutex<HashMap<(String, String), Session>>>,
    cancels: Arc<Mutex<HashMap<u64, CancelHandle>>>,
    explain_requests: Arc<Mutex<explain::cancel::Registry>>,
    query_requests: Arc<Mutex<explain::cancel::Registry>>,
    metadata_tasks: Mutex<HashMap<String, Vec<tokio::task::AbortHandle>>>,
    compare_work: Arc<Mutex<compare::CompareWork>>,
    structure_work: Arc<Mutex<ddl_diff::StructureWork>>,
    /// Latest Database Graph layout request for 3D (0) and 2D (1); older ones stop early.
    graph_layouts: Arc<[std::sync::atomic::AtomicU64; 2]>,
    /// Running transfers by task, with the token that stops each.
    tasks: Arc<Mutex<HashMap<TaskId, tokio_util::sync::CancellationToken>>>,
    /// Connections of open Activity Monitors, by monitor.
    activity: ActivityConnections,
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.mcp.stop();
        self.query_requests.lock().unwrap().cancel_all();
        if let Ok(requests) = self.explain_requests.lock() {
            requests.cancel_all();
        }
        // Runtime teardown waits for blocking work. Cancel before any fields
        // are dropped, including when a comparison window is still open.
        if let Ok(mut work) = self.compare_work.lock() {
            work.cancel_all();
        }
        if let Ok(mut work) = self.structure_work.lock() {
            work.cancel_all();
        }
    }
}

impl Backend {
    pub fn new(ctx: egui::Context) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime");
        let (tx, rx) = channel();
        let mcp_tx = tx.clone();
        let mcp_ctx = ctx.clone();
        let mcp = crate::mcp::McpController::new(rt.handle().clone(), move || {
            let _ = mcp_tx.send(Event::McpChanged);
            mcp_ctx.request_repaint();
        });
        Self {
            mcp,
            rt,
            persistence: None,
            tx,
            rx,
            ctx,
            sessions: Arc::default(),
            cancels: Arc::default(),
            explain_requests: Arc::default(),
            query_requests: Arc::default(),
            metadata_tasks: Mutex::default(),
            compare_work: Arc::default(),
            structure_work: Arc::default(),
            graph_layouts: Arc::default(),
            tasks: Arc::default(),
            activity: Arc::default(),
        }
    }

    pub(crate) fn start_persistence(
        &mut self,
        store: Box<dyn crate::credentials::CredentialStore>,
        synchronous_load: bool,
    ) -> Option<Result<crate::persistence::Loaded, String>> {
        let path = crate::config::Config::path()
            .unwrap_or_else(|| crate::config::Config::data_dir().join("config.json"));
        let mut persistence = crate::persistence::Persistence::new(path, store);
        let initial = synchronous_load.then(|| persistence.load());
        let (tx, rx) = channel();
        self.persistence = Some(tx);
        let send = self.send();
        // A native authorization dialog may wait indefinitely. A dedicated worker keeps
        // both UI and Tokio shutdown responsive without abandoning a committed write.
        std::thread::Builder::new()
            .name("credentials".into())
            .spawn(move || {
                if !synchronous_load {
                    send(Event::ConfigLoaded(persistence.load()));
                }
                while let Ok(command) = rx.recv() {
                    match command {
                        crate::persistence::Command::Load => {
                            send(Event::ConfigLoaded(persistence.load()))
                        }
                        crate::persistence::Command::Save { config, apply } => {
                            send(Event::ConfigSaved {
                                apply,
                                result: persistence.save(*config, apply),
                            })
                        }
                        crate::persistence::Command::LoadMcp => {
                            send(Event::McpStateLoaded(persistence.load_mcp()))
                        }
                        crate::persistence::Command::SaveMcp(state) => {
                            send(Event::McpStateSaved(persistence.save_mcp(*state)))
                        }
                    }
                }
            })
            .expect("credential worker");
        initial
    }

    pub(crate) fn save_config(&self, config: crate::config::Config, apply: bool) {
        if let Some(worker) = &self.persistence {
            let _ = worker.send(crate::persistence::Command::Save {
                config: Box::new(config),
                apply,
            });
        }
    }
    /// Returns false when persistence is not running (tests without a store).
    pub(crate) fn load_mcp_state(&self) -> bool {
        self.persistence
            .as_ref()
            .is_some_and(|worker| worker.send(crate::persistence::Command::LoadMcp).is_ok())
    }
    pub(crate) fn save_mcp_state(&self, state: crate::mcp::McpState) -> bool {
        self.persistence.as_ref().is_some_and(|worker| {
            worker
                .send(crate::persistence::Command::SaveMcp(Box::new(state)))
                .is_ok()
        })
    }
    pub(crate) fn reload_credentials(&self) {
        if let Some(worker) = &self.persistence {
            let _ = worker.send(crate::persistence::Command::Load);
        }
    }

    pub fn ctx(&self) -> &egui::Context {
        &self.ctx
    }

    fn send(&self) -> impl Fn(Event) + Send + Clone + 'static {
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        move |e| {
            let _ = tx.send(e);
            ctx.request_repaint();
        }
    }

    fn session(&self, ds_id: &str, name: &str) -> Session {
        self.sessions
            .lock()
            .unwrap()
            .entry((ds_id.to_string(), name.to_string()))
            .or_default()
            .clone()
    }

    /// Catalog requests have no transaction outcome to report after disconnect.
    /// Track their tasks so removing the session map also releases active owners.
    fn spawn_metadata(
        &self,
        ds_id: String,
        work: impl std::future::Future<Output = ()> + Send + 'static,
    ) {
        let mut tasks = self.metadata_tasks.lock().unwrap();
        let source = tasks.entry(ds_id).or_default();
        source.retain(|task| !task.is_finished());
        source.push(self.rt.spawn(work).abort_handle());
    }

    pub fn load_result_metadata(&self, request: ResultMetadataRequest) {
        let send = self.send();
        let session = self.session(&request.ds.id, &request.console_id.to_string());
        self.spawn_metadata(request.ds.id.clone(), async move {
            let mut state = session.lock().await;
            let result = async {
                state.validate_result_session(
                    &request.ds,
                    &request.source_identity,
                    request.source.session_epoch,
                )?;
                anyhow::ensure!(!request.ds.read_only, "This data source is read-only.");
                let context = state.conn.as_mut().unwrap().resolved_context().await?;
                anyhow::ensure!(
                    context == request.expected_context,
                    "The console context has changed. Execute the query again before editing."
                );
                anyhow::ensure!(
                    request.source.columns.len() == request.source.exact_columns.len(),
                    "Incomplete column origin metadata."
                );
                let mut origins = request.source.columns.iter().flatten();
                let origin = origins
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("No physical table columns in this result."))?
                    .table
                    .clone();
                anyhow::ensure!(
                    origins.all(|column| column.table == origin),
                    "Columns from multiple tables cannot be edited in this result."
                );
                if let Some(metadata) = state.edit_metadata.get(&origin) {
                    return Ok(metadata.clone());
                }
                let metadata = state
                    .conn
                    .as_mut()
                    .unwrap()
                    .table_edit_metadata(&origin)
                    .await?;
                if state.edit_metadata.len() >= 256 {
                    state.edit_metadata.clear();
                }
                state.edit_metadata.insert(origin, metadata.clone());
                anyhow::Ok(metadata)
            }
            .await;
            send(Event::ResultMetadata {
                console_id: request.console_id,
                result_id: request.result_id,
                request_id: request.request_id,
                result: result.map_err(fmt_err),
            });
        });
    }

    /// Catalog caches are session-local, including temporary and attached tables.
    pub fn invalidate_result_metadata(&self, ds_id: &str) {
        let sessions: Vec<_> = self
            .sessions
            .lock()
            .unwrap()
            .iter()
            .filter(|((id, _), _)| id == ds_id)
            .map(|(_, session)| session.clone())
            .collect();
        for session in sessions {
            self.rt.spawn(async move {
                let mut state = session.lock().await;
                state.edit_metadata.clear();
                state.related_metadata.clear();
            });
        }
    }

    pub fn apply_updates(&self, request: MutationRequest) {
        let send = self.send();
        let session = self.session(&request.ds.id, &request.console_id.to_string());
        let cancels = self.cancels.clone();
        self.rt.spawn(async move {
            let mut state = session.lock().await;
            let result = apply_update_batch(&mut state, &request, &cancels).await;
            cancels.lock().unwrap().remove(&request.console_id);
            send(Event::UpdatesDone {
                console_id: request.console_id,
                result_id: request.result_id,
                request_id: request.request_id,
                in_tx: state.in_tx,
                result,
            });
        });
    }

    pub fn test_connection(&self, ds: DataSource, request_id: String) {
        let send = self.send();
        self.rt.spawn(async move {
            let result = tokio::time::timeout(std::time::Duration::from_secs(15), async {
                let mut conn = Connection::open(&ds).await?;
                let info = conn.server_info().await?;
                anyhow::Ok(format!("{} {} · {}", info.product, info.version, info.transport))
            })
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("Connection test timed out after 15 seconds")));
            send(Event::TestDone {
                ds_id: ds.id,
                request_id,
                result: result.map_err(fmt_err),
            });
        });
    }

    /// Opens the explorer session, loads the root of the tree and the completion metadata.
    pub fn connect(&self, ds: DataSource, request_id: u64) {
        let send = self.send();
        let session = self.session(&ds.id, "meta");
        self.spawn_metadata(ds.id.clone(), async move {
            let mut guard = session.lock().await;
            guard.conn = None;
            let result = async {
                let conn = guard.conn(&ds).await?;
                let info = conn.server_info().await?;
                let root = conn.children(None).await?;
                anyhow::Ok((format!("{} {} · {}", info.product, info.version, info.transport), root))
            }
            .await;
            match result {
                Ok((info, root)) => {
                    send(Event::Connected {
                        ds_id: ds.id.clone(),
                        result: Ok(info),
                    });
                    send(Event::Children {
                        ds_id: ds.id.clone(),
                        path: None,
                        result: Ok(root),
                    });
                    let meta = guard.conn.as_mut().unwrap().completion_metadata().await;
                    send(Event::Completion {
                        key: MetadataKey::for_source(&ds),
                        request_id,
                        result: meta.map_err(fmt_err),
                    });
                }
                Err(e) => {
                    let error = fmt_err(e);
                    send(Event::Connected {
                        ds_id: ds.id.clone(),
                        result: Err(error.clone()),
                    });
                    send(Event::Completion {
                        key: MetadataKey::for_source(&ds),
                        request_id,
                        result: Err(error),
                    });
                }
            }
        });
    }

    /// Loads catalog data for the requested physical database on a metadata
    /// session. Callers deduplicate requests by MetadataKey::for_source.
    pub fn load_completion(&self, ds: DataSource, request_id: u64) {
        let send = self.send();
        let session = self.session(&ds.id, &meta_session(&ds));
        self.spawn_metadata(ds.id.clone(), async move {
            let mut guard = session.lock().await;
            let result = async { guard.conn(&ds).await?.completion_metadata().await }.await;
            send(Event::Completion {
                key: MetadataKey::for_source(&ds),
                request_id,
                result: result.map_err(fmt_err),
            });
        });
    }

    /// Explicit selection is always reapplied, including Default after SQL SET.
    pub fn change_schema(
        &self,
        console_id: u64,
        ds: DataSource,
        requested: ExecutionContext,
        revision: u64,
    ) {
        let send = self.send();
        let session = self.session(&ds.id, &console_id.to_string());
        self.spawn_metadata(ds.id.clone(), async move {
            let ds = context_source(ds, &requested);
            let mut state = session.lock().await;
            let result = async {
                anyhow::ensure!(!state.in_tx, "Commit or roll back the open transaction before changing the schema.");
                state.conn(&ds).await?;
                anyhow::ensure!(state.open_database == MetadataKey::for_source(&ds).database,
                    "The console is connected to a different database. Reconnect before changing the schema.");
                let resolved = state.conn.as_mut().unwrap().change_schema(&requested.schema).await?;
                state.configured_schema = requested.schema.clone();
                Ok(resolved)
            }.await;
            send(Event::SchemaChanged {
                console_id,
                ds_id: ds.id,
                revision,
                requested,
                result: result.map_err(fmt_err),
            });
        });
    }

    pub fn load_children(&self, ds: DataSource, node: DbObject) {
        let send = self.send();
        // A PostgreSQL connection sees one database: objects of another one need their own.
        let ds = match (ds.driver, node.path.first()) {
            (crate::model::DriverKind::Postgres, Some(db)) => ds.with_database(db),
            _ => ds,
        };
        let session = self.session(&ds.id, &meta_session(&ds));
        self.spawn_metadata(ds.id.clone(), async move {
            let mut guard = session.lock().await;
            let result = async { guard.conn(&ds).await?.children(Some(&node)).await }.await;
            send(Event::Children {
                ds_id: ds.id,
                path: Some(node.path),
                result: result.map_err(fmt_err),
            });
        });
    }

    pub fn generate_ddl(&self, ds: DataSource, node: DbObject) {
        let send = self.send();
        let session = self.session(&ds.id, &meta_session(&ds));
        self.spawn_metadata(ds.id.clone(), async move {
            let mut guard = session.lock().await;
            let result = async { guard.conn(&ds).await?.ddl(&node).await }.await;
            send(Event::Ddl {
                ds_id: ds.id,
                title: node.name,
                result: result.map_err(fmt_err),
            });
        });
    }

    /// Reads one database's catalogs for Database Graph and lays it out.
    /// Pins are node indexes with fixed positions from a saved layout.
    pub fn load_schema_graph(
        &self,
        ds: DataSource,
        database: String,
        schemas: Option<Vec<String>>,
        request: GraphLayoutRequest,
        request_id: u64,
    ) {
        use std::sync::atomic::Ordering;
        let send = self.send();
        let ds = match ds.driver {
            crate::model::DriverKind::Postgres => ds.with_database(&database),
            _ => ds,
        };
        // A new scan supersedes layouts of the previous one in both modes.
        for latest in self.graph_layouts.iter() {
            latest.store(request_id, Ordering::SeqCst);
        }
        let latest = Arc::clone(&self.graph_layouts);
        let session = self.session(&ds.id, &meta_session(&ds));
        self.spawn_metadata(ds.id.clone(), async move {
            let result = async {
                let mut guard = session.lock().await;
                let graph = guard.conn(&ds).await?.schema_graph(&database, schemas.as_deref()).await?;
                drop(guard);
                let names = graph.index_by_name();
                let pins: Vec<(usize, [f32; 3])> = request
                    .pins
                    .iter()
                    .filter_map(|(schema, name, position)| names.get(&(schema.as_str(), name.as_str())).map(|i| (*i, *position)))
                    .collect();
                drop(names);
                let graph = Arc::new(graph);
                let shared = graph.clone();
                let flat = request.flat;
                // Only the shown mode is laid out; the other one when it is first shown.
                let layout = tokio::task::spawn_blocking(move || {
                    let cancelled = || latest[flat as usize].load(Ordering::Relaxed) != request_id;
                    crate::schema_graph::layout_cancellable(&shared, flat, &pins, &request.params, &cancelled)
                })
                .await?
                .ok_or_else(|| anyhow::anyhow!("Superseded by a newer scan"))?;
                anyhow::Ok(Box::new(SchemaGraphLoaded { graph, layout, flat }))
            }
            .await;
            send(Event::SchemaGraph {
                request_id,
                result: result.map_err(fmt_err),
            });
        });
    }

    /// Lays out a loaded graph again around pinned nodes. A newer request for
    /// the same mode, or [`Backend::cancel_schema_graph`], stops this one.
    pub fn layout_schema_graph(
        &self,
        graph: Arc<crate::schema_graph::SchemaGraph>,
        flat: bool,
        pins: Vec<(usize, [f32; 3])>,
        params: crate::schema_graph::LayoutParams,
        request_id: u64,
    ) {
        use std::sync::atomic::Ordering;
        self.graph_layouts[flat as usize].store(request_id, Ordering::SeqCst);
        let latest = Arc::clone(&self.graph_layouts);
        let send = self.send();
        self.rt.spawn_blocking(move || {
            let cancelled = || latest[flat as usize].load(Ordering::Relaxed) != request_id;
            if let Some(layout) = crate::schema_graph::layout_cancellable(&graph, flat, &pins, &params, &cancelled) {
                send(Event::SchemaGraphLayout {
                    request_id,
                    flat,
                    layout,
                });
            }
        });
    }

    /// Stops layouts of a closed Database Graph window.
    pub fn cancel_schema_graph(&self) {
        for latest in self.graph_layouts.iter() {
            latest.store(0, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Executes statements one by one in the console's own session, stopping at the first error.
    pub fn run(&self, req: RunRequest) {
        self.run_paged(req, None);
    }

    /// Like [`Backend::run`] for a table editor's generated SELECT, which the
    /// server limits to the requested page.
    pub fn run_table_page(&self, req: RunRequest, table: TablePage) {
        self.run_paged(req, Some(table));
    }

    fn run_paged(&self, req: RunRequest, table_page: Option<TablePage>) {
        let Some((registration, token)) = explain::cancel::Registry::register(
            &self.query_requests,
            &req.ds.id,
            req.console_id,
            0,
        ) else {
            return;
        };
        let send = self.send();
        let session = self.session(&req.ds.id, &req.console_id.to_string());
        let cancels = self.cancels.clone();
        self.rt.spawn(async move {
            let mut registration = Some(registration);
            let console = req.console_id;
            let first_sql = req.statements.first().cloned().unwrap_or_default();
            let requested_page = req.page;
            let mut guard = tokio::select! {
                biased;
                _ = token.cancelled() => None,
                guard = session.lock() => Some(guard),
            };
            let interrupted = {
                let operation = async {
                    let RunRequest {
                        console_id,
                        ds,
                        statements,
                        page,
                        manual_tx,
                        context,
                        expected_context,
                    } = req;
                    let ds = context_source(ds, &context);
                    let state = &mut **guard.as_mut().unwrap();
                    let ending_transaction = statements.len() == 1 && tx_effect(&statements[0]) == Some(false);
                    let prepared = if ending_transaction {
                        Ok(())
                    } else if expected_context.is_some() {
                        // Restored results may initialize a new session's configured
                        // namespace. An existing session is only checked, never switched.
                        async {
                            if state.conn.is_none() {
                                state.configure(&ds, &context.schema).await?;
                            }
                            anyhow::ensure!(
                                state.open_database == MetadataKey::for_source(&ds).database,
                                "The result belongs to a different database. Execute the query again."
                            );
                            anyhow::ensure!(
                                state.configured_schema == context.schema,
                                "The console schema has changed. Execute the query again before reloading."
                            );
                            Ok(())
                        }
                        .await
                    } else {
                        state.configure(&ds, &context.schema).await
                    };
                    if let Err(error) = prepared {
                        send(Event::StatementDone {
                            console_id,
                            sql: statements.first().cloned().unwrap_or_default(),
                            page,
                            result: Err(fmt_err(error)),
                        });
                        drop(registration.take());
                        send(Event::RunFinished {
                            console_id,
                            in_tx: state.in_tx,
                        });
                        return;
                    }
                    // The server's state decides, not the statements seen so far:
                    // MySQL commits implicitly before DDL and similar statements.
                    let mut open = match state.conn(&ds).await {
                        Ok(conn) => conn.transaction_state().await.unwrap_or(state.in_tx),
                        Err(_) => state.in_tx,
                    };
                    state.in_tx = open;
                    // Whether an implicit commit could make earlier changes permanent.
                    let mut tx_has_work = open;
                    for (index, sql) in statements.into_iter().enumerate() {
                        let control = tx_effect(&sql);
                        // Manual mode keeps every statement in a transaction, also
                        // after one ended in the middle of a script.
                        if manual_tx && !open && (index == 0 || control.is_none()) {
                            let begin = begin_statement(ds.driver);
                            let result = async { state.conn(&ds).await?.execute(begin, page).await }.await;
                            match result {
                                Ok(result) if index == 0 => send(Event::StatementDone {
                                    console_id,
                                    sql: begin.to_string(),
                                    page,
                                    result: Ok(result),
                                }),
                                Ok(_) => send(Event::Notice {
                                    console_id,
                                    text: format!("{begin}: the remaining statements run in a new transaction"),
                                }),
                                Err(error) => {
                                    send(Event::StatementDone {
                                        console_id,
                                        sql: begin.to_string(),
                                        page,
                                        result: Err(fmt_err(error)),
                                    });
                                    break;
                                }
                            }
                            open = true;
                            state.in_tx = true;
                            tx_has_work = false;
                        }
                        let result = async {
                            let conn = state.conn(&ds).await?;
                            if let Some(h) = conn.cancel_handle() {
                                cancels.lock().unwrap().insert(console_id, h);
                            }
                            if control.is_some() {
                                conn.execute(&sql, page).await
                            } else if let Some(table) = &table_page {
                                let (paged, window) = table_page_query(conn, ds.driver, table, &sql, page).await;
                                conn.execute_limited_with_context(&paged, window, expected_context.as_ref())
                                    .await
                            } else {
                                conn.execute_with_context(&sql, page, expected_context.as_ref())
                                    .await
                            }
                        }
                        .await;
                        // An error packet carries no MySQL status, so ask the server.
                        let reported = match (&result, state.conn.as_mut()) {
                            (Ok(_), Some(conn)) => conn.transaction_state().await.ok(),
                            (Err(_), Some(conn)) => conn.has_open_transaction().await.ok(),
                            _ => None,
                        };
                        let after = reported.unwrap_or(match (&result, control) {
                            (Ok(_), Some(opened)) => opened,
                            _ => open,
                        });
                        let ended_implicitly = open && !after && control != Some(false) && tx_has_work;
                        tx_has_work = after
                            && control.is_none()
                            && (tx_has_work || result.as_ref().map_or(true, |result| !result.has_result_set()));
                        open = after;
                        state.in_tx = after;
                        if result
                            .as_ref()
                            .map_or(true, |result| !result.has_result_set())
                        {
                            state.edit_metadata.clear();
                            state.related_metadata.clear();
                        }
                        let failed = result.is_err();
                        send(Event::StatementDone {
                            console_id,
                            sql,
                            page,
                            result: result.map_err(fmt_err),
                        });
                        if ended_implicitly {
                            send(Event::Notice {
                                console_id,
                                text: implicit_end_notice(ds.driver, failed).into(),
                            });
                        }
                        if failed {
                            break;
                        }
                    }
                    cancels.lock().unwrap().remove(&console_id);
                    drop(registration.take());
                    send(Event::RunFinished {
                        console_id,
                        in_tx: state.in_tx,
                    });
                };
                tokio::pin!(operation);
                tokio::select! {
                    biased;
                    _ = token.cancelled() => true,
                    _ = &mut operation => false,
                }
            };
            if interrupted {
                // Cancellation must not depend on the original socket responding.
                // Keep the session lock until cancellation and closure finish.
                let handle = cancels.lock().unwrap().remove(&console);
                if let Some(handle) = handle {
                    let _ = tokio::time::timeout(
                        std::time::Duration::from_secs(1), handle.cancel(),
                    ).await;
                }
                let reason = "Execution interrupted; the database outcome is unknown. The session was closed. Disconnect and reconnect before continuing; verify any changes before retrying.".to_owned();
                // Do not wait for a different operation holding the session lock.
                if let Some(state) = guard.as_mut() {
                    state.close_uncertain(reason.clone());
                }
                send(Event::StatementDone {
                    console_id: console, sql: first_sql, page: requested_page,
                    result: Err(reason),
                });
                drop(registration.take());
                send(Event::RunFinished {
                    console_id: console, in_tx: false,
                });
            }
        });
    }

    /// Commits or rolls back the console's open transaction.
    pub fn end_transaction(&self, console_id: u64, ds: DataSource, commit: bool) {
        let sql = if commit { "COMMIT" } else { "ROLLBACK" };
        self.run(RunRequest {
            console_id,
            ds,
            statements: vec![sql.to_string()],
            page: Page::first(0),
            manual_tx: false,
            context: ExecutionContext::default(),
            expected_context: None,
        });
    }

    /// Runs the query again without a row limit on a separate connection and
    /// streams it to a file.
    #[allow(clippy::too_many_arguments)]
    pub fn export(
        &self,
        task: TaskId,
        ds: DataSource,
        sql: String,
        options: export::ExportOptions,
        table: export::TableName,
        path: PathBuf,
        context: Option<ResolvedContext>,
    ) {
        let ds = match (&context, ds.driver) {
            (Some(context), DriverKind::Postgres) => context
                .database
                .as_deref()
                .map_or_else(|| ds.clone(), |database| ds.with_database(database)),
            _ => ds,
        };
        self.export_data(
            task,
            ExportJob {
                items: vec![ExportItem {
                    ds,
                    query: crate::model::BoundQuery { sql, params: Vec::new() },
                    context,
                    table,
                    path,
                }],
                options,
            },
        );
    }

    /// Opens a data file; CSV import runs on the blocking pool.
    pub fn open_file(&self, path: PathBuf, temp_dir: PathBuf) {
        let send = self.send();
        self.rt.spawn_blocking(move || {
            let result = files::open(&path, &temp_dir).map(Box::new).map_err(fmt_err);
            send(Event::FileOpened { path, result });
        });
    }

    /// Writes imported tables of an opened file back to their CSV files.
    pub fn save_file(&self, console_id: u64, opened: OpenedFile, table: Option<String>) {
        let send = self.send();
        self.rt.spawn_blocking(move || {
            let result = files::save(&opened, table.as_deref()).map_err(fmt_err);
            send(Event::FileSaved { console_id, result });
        });
    }

    pub fn cancel(&self, console_id: u64) {
        if self.query_requests.lock().unwrap().cancel(console_id) {
            return;
        }
        if self.explain_requests.lock().unwrap().cancel(console_id) {
            return;
        }
        if let Some(h) = self.cancels.lock().unwrap().remove(&console_id) {
            self.rt.spawn(async move {
                let _ = h.cancel().await;
            });
        }
    }

    /// Drops all sessions of a data source; connections close when dropped,
    /// which also rolls back open transactions.
    pub fn disconnect(&self, ds_id: &str) {
        if let Some(tasks) = self.metadata_tasks.lock().unwrap().remove(ds_id) {
            for task in tasks {
                task.abort();
            }
        }
        self.query_requests.lock().unwrap().cancel_source(ds_id);
        self.explain_requests.lock().unwrap().cancel_source(ds_id);
        self.sessions
            .lock()
            .unwrap()
            .retain(|(id, _), _| id != ds_id);
    }

    /// Closes one console's session.
    pub fn close_session(&self, ds_id: &str, console_id: u64) {
        self.query_requests
            .lock()
            .unwrap()
            .cancel_session(ds_id, console_id);
        self.explain_requests
            .lock()
            .unwrap()
            .cancel_session(ds_id, console_id);
        self.sessions
            .lock()
            .unwrap()
            .remove(&(ds_id.to_string(), console_id.to_string()));
    }
}

async fn apply_update_batch(
    state: &mut SessionState,
    request: &MutationRequest,
    cancels: &Arc<Mutex<HashMap<u64, CancelHandle>>>,
) -> Result<MutationReport, MutationFailure> {
    let mut opened_transaction = false;
    let mut savepoint_active = false;
    let mut commit_attempted = false;
    let mut source_row = None;
    let savepoint = format!("dbeast_edits_{}", uuid::Uuid::new_v4().simple());
    let result = async {
        state.validate_result_session(&request.ds, &request.source_identity, request.session_epoch)?;
        anyhow::ensure!(request.session_epoch == request.metadata.session_epoch,
            "Editing metadata belongs to another database session.");
        anyhow::ensure!(!request.ds.read_only, "This data source is read-only.");
        anyhow::ensure!(!request.mutations.is_empty(), "There are no changes to submit.");
        let conn = state.conn.as_mut().unwrap();
        if let Some(handle) = conn.cancel_handle() { cancels.lock().unwrap().insert(request.console_id, handle); }
        anyhow::ensure!(conn.resolved_context().await? == request.expected_context,
            "The console context has changed. Execute the query again before submitting changes.");
        state.in_tx = conn.has_open_transaction().await?;
        if !state.in_tx {
            conn.execute_control(begin_statement(request.ds.driver)).await?;
            state.in_tx = true;
            opened_transaction = true;
        }
        conn.execute_control(&format!("SAVEPOINT {savepoint}")).await?;
        savepoint_active = true;
        if request.ds.driver != DriverKind::Sqlite {
            let quote = if request.ds.driver == DriverKind::MySql { '`' } else { '"' };
            let table = format!(
                "{}.{}",
                crate::drivers::quote_ident(&request.metadata.table.schema, quote),
                crate::drivers::quote_ident(&request.metadata.table.name, quote),
            );
            // Resolve the physical name before checking the catalog. SELECT
            // retains PostgreSQL's relation lock / MySQL's metadata lock until
            // transaction end, so concurrent DDL cannot replace the target or
            // change its engine after validation. The predicate reads no rows.
            // SQLite BEGIN IMMEDIATE already excludes concurrent schema writes.
            conn.execute_control(&format!("SELECT 1 FROM {table} WHERE 1=0")).await?;
        }
        let origin = match request.ds.driver {
            DriverKind::Postgres => OriginTable::Postgres(request.metadata.table.oid
                .ok_or_else(|| anyhow::anyhow!("Missing PostgreSQL relation identity."))?),
            _ => OriginTable::Named { schema: request.metadata.table.schema.clone(), table: request.metadata.table.name.clone() },
        };
        // A fresh catalog check is mandatory for writes even when the read-only
        // metadata view came from cache. Other clients may have changed the table.
        let fresh = conn.table_edit_metadata(&origin).await?;
        validate_edit_metadata(&request.metadata, &fresh)?;
        if let Err((index, error)) = conn.execute_bound_updates(&request.mutations, Arc::new(|| false)).await {
            source_row = request.mutations.get(index).map(|update| update.source_row);
            return Err(error);
        }
        conn.execute_control(&format!("RELEASE SAVEPOINT {savepoint}")).await?;
        savepoint_active = false;
        if opened_transaction && !request.manual_tx {
            commit_attempted = true;
            conn.execute_control("COMMIT").await?;
            state.in_tx = false;
        }
        anyhow::Ok(MutationReport { changed_rows: request.mutations.len() })
    }.await;
    match result {
        Ok(report) => Ok(report),
        Err(error) => {
            state.edit_metadata.clear();
            state.related_metadata.clear();
            let mut cleanup_error = None;
            if let Some(conn) = state.conn.as_mut() {
                if savepoint_active && !commit_attempted {
                    if let Err(error) = conn
                        .execute_control(&format!("ROLLBACK TO SAVEPOINT {savepoint}"))
                        .await
                    {
                        cleanup_error = Some(error);
                    } else if let Err(error) = conn
                        .execute_control(&format!("RELEASE SAVEPOINT {savepoint}"))
                        .await
                    {
                        cleanup_error = Some(error);
                    }
                }
                if opened_transaction
                    && (!request.manual_tx || !savepoint_active)
                    && !commit_attempted
                {
                    match conn.execute_control("ROLLBACK").await {
                        Ok(_) => state.in_tx = false,
                        Err(error) => {
                            cleanup_error = Some(error);
                        }
                    }
                }
            }
            let outcome_unknown = commit_attempted || cleanup_error.is_some();
            let mut message = fmt_err(error);
            if let Some(error) = cleanup_error {
                message.push_str(&format!(
                    "; rollback could not be confirmed: {}",
                    fmt_err(error)
                ));
            }
            if outcome_unknown {
                // Continuing on an uncertain connection risks repeating a committed
                // update or accidentally committing a partially recovered transaction.
                state.conn = None;
                state.in_tx = false;
                message.push_str(
                    "; the session was closed. Verify the database before submitting again.",
                );
            }
            Err(MutationFailure {
                message,
                source_row,
                outcome_unknown,
            })
        }
    }
}

fn validate_edit_metadata(
    expected: &ResultEditMetadata,
    actual: &TableEditMetadata,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        actual.transactional && expected.table == actual.table,
        "The source table has changed. Execute the query again before editing."
    );
    let count = expected.physical_columns.len();
    anyhow::ensure!(
        expected.writable.len() == count && expected.update_default.len() == count,
        "Incomplete editing metadata."
    );
    let key: Option<Vec<_>> = expected
        .primary_key
        .iter()
        .map(|&index| {
            expected
                .physical_columns
                .get(index)
                .and_then(Option::as_ref)
        })
        .collect();
    anyhow::ensure!(
        !actual.primary_key.is_empty()
            && key.as_ref().is_some_and(|key| {
                key.len() == actual.primary_key.len()
                    && key.iter().zip(&actual.primary_key).all(|(a, b)| *a == b)
            }),
        "The primary key has changed. Execute the query again before editing."
    );
    let by_name: HashMap<_, _> = actual
        .columns
        .iter()
        .map(|column| (column.name.as_str(), column))
        .collect();
    let key_positions: std::collections::HashSet<_> =
        expected.primary_key.iter().copied().collect();
    anyhow::ensure!(
        key_positions.len() == expected.primary_key.len(),
        "Duplicate primary key position."
    );
    for (index, name) in expected.physical_columns.iter().enumerate() {
        let Some(name) = name else {
            anyhow::ensure!(
                !expected.writable[index] && !expected.update_default[index],
                "A computed column cannot be writable."
            );
            continue;
        };
        let column = by_name.get(name.as_str()).copied().ok_or_else(|| {
            anyhow::anyhow!("A result column no longer exists. Execute the query again.")
        })?;
        anyhow::ensure!(
            !expected.writable[index] || !column.generated,
            "A column is no longer writable. Execute the query again before editing."
        );
        anyhow::ensure!(
            !expected.comparable[index] || column.comparable,
            "A column can no longer be compared safely. Execute the query again before editing."
        );
        anyhow::ensure!(
            !expected.update_default[index] || column.update_default,
            "A column's default is no longer writable."
        );
        if key_positions.contains(&index) {
            anyhow::ensure!(
                !column.nullable && column.comparable,
                "The primary key can no longer be compared safely."
            );
        }
    }
    Ok(())
}

/// Explorer session of a data source; PostgreSQL has one per database.
fn meta_session(ds: &DataSource) -> String {
    match ds.driver {
        crate::model::DriverKind::Postgres => format!("meta:{}", ds.database),
        _ => "meta".into(),
    }
}

fn context_source(ds: DataSource, context: &ExecutionContext) -> DataSource {
    match (ds.driver, context.database.as_deref()) {
        (DriverKind::Postgres, Some(database)) => ds.with_database(database),
        _ => ds,
    }
}

/// Why the transaction open before a statement is gone after it, although the
/// statement was not COMMIT or ROLLBACK.
fn implicit_end_notice(driver: DriverKind, failed: bool) -> &'static str {
    match (driver, failed) {
        (DriverKind::MySql, false) => "Transaction committed implicitly by this statement (MySQL commits before DDL and similar statements). Earlier changes can no longer be rolled back.",
        (DriverKind::MySql, true) => "The open transaction ended on the server with this error. MySQL commits before DDL even when it fails and rolls back after a deadlock; check which earlier changes were kept.",
        (_, false) => "The open transaction ended with this statement.",
        (_, true) => "The open transaction ended on the server with this error and its changes were rolled back.",
    }
}

/// How a successfully executed statement changes the transaction state:
/// `Some(true)` opens one, `Some(false)` ends it.
pub fn tx_effect(sql: &str) -> Option<bool> {
    let bytes = sql.as_bytes();
    let mut at = 0;
    let mut words = Vec::with_capacity(8);
    while at < bytes.len() && words.len() < 8 {
        match bytes[at] {
            byte if byte.is_ascii_whitespace() => at += 1,
            b'-' if bytes.get(at + 1) == Some(&b'-') => {
                while at < bytes.len() && bytes[at] != b'\n' {
                    at += 1;
                }
            }
            b'#' => {
                while at < bytes.len() && bytes[at] != b'\n' {
                    at += 1;
                }
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                at += 2;
                let mut depth = 1;
                while at < bytes.len() && depth > 0 {
                    if bytes.get(at..at + 2) == Some(b"/*") {
                        depth += 1;
                        at += 2;
                    } else if bytes.get(at..at + 2) == Some(b"*/") {
                        depth -= 1;
                        at += 2;
                    } else {
                        at += 1;
                    }
                }
            }
            byte if byte.is_ascii_alphabetic() => {
                let start = at;
                while at < bytes.len() && bytes[at].is_ascii_alphabetic() {
                    at += 1;
                }
                words.push(&sql[start..at]);
            }
            _ => break,
        }
    }
    let first = *words.first()?;
    if first.eq_ignore_ascii_case("begin")
        || (first.eq_ignore_ascii_case("start")
            && words
                .get(1)
                .is_some_and(|word| word.eq_ignore_ascii_case("transaction")))
    {
        return Some(true);
    }
    if !["commit", "rollback", "end", "abort"]
        .iter()
        .any(|word| first.eq_ignore_ascii_case(word))
    {
        return None;
    }
    let tail = &words[1..];
    let tail = if tail.first().is_some_and(|word| {
        word.eq_ignore_ascii_case("work") || word.eq_ignore_ascii_case("transaction")
    }) {
        &tail[1..]
    } else {
        tail
    };
    if first.eq_ignore_ascii_case("rollback")
        && tail
            .first()
            .is_some_and(|word| word.eq_ignore_ascii_case("to"))
    {
        return None;
    }
    Some(tail.windows(2).any(|words| {
        words[0].eq_ignore_ascii_case("and") && words[1].eq_ignore_ascii_case("chain")
    }))
}

/// Full error chain, so driver details (SQLSTATE, position) reach the user.
fn fmt_err(e: anyhow::Error) -> String {
    let mut out = e.to_string();
    for cause in e.chain().skip(1) {
        let s = cause.to_string();
        if !out.contains(&s) {
            out.push_str(": ");
            out.push_str(&s);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::tx_effect;

    #[test]
    fn transaction_statements() {
        assert_eq!(tx_effect("BEGIN"), Some(true));
        assert_eq!(tx_effect("start transaction read only"), Some(true));
        assert_eq!(tx_effect("commit;"), Some(false));
        assert_eq!(tx_effect("ROLLBACK"), Some(false));
        assert_eq!(tx_effect("rollback to savepoint a"), None);
        assert_eq!(tx_effect("select 1"), None);
        assert_eq!(
            tx_effect("-- comment\n START /* nested /* c */ */ TRANSACTION"),
            Some(true)
        );
        assert_eq!(tx_effect("COMMIT WORK AND CHAIN"), Some(true));
        assert_eq!(tx_effect("ROLLBACK AND NO CHAIN"), Some(false));
        assert_eq!(tx_effect("ROLLBACK WORK TO SAVEPOINT a"), None);
    }
}
