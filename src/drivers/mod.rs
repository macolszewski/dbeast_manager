mod mysql;
mod postgres;

pub use postgres::DEFAULT_DATABASE;
mod sqlite;
mod tunnel;
mod tls;
pub(crate) mod mcp;

use crate::model::{
    BoundQuery, BoundUpdate, CompletionMetadata, DataSource, DbObject, DriverKind, ObjectKind,
    OriginTable, Page, QueryResult, RelatedDirection, RelatedMetadata, ResolvedContext,
    SchemaSelection, TableEditMetadata, TableMeta,
};
use anyhow::{Context, Result};

pub use tunnel::Tunnel;

/// The data source's SSH tunnel, shared with its open sessions; the client
/// tools connect through its local port.
pub async fn open_tunnel(ds: &DataSource) -> Result<std::sync::Arc<Tunnel>> {
    tunnel::open(ds).await.context("SSH tunnel")
}

/// Rows read by [`Connection::stream_rows`]: the columns once, then batches.
pub enum RowBatch {
    Columns(Vec<crate::model::Column>),
    Rows(Vec<Vec<crate::model::Value>>),
}

/// Rows per batch of a streamed read; big enough to amortize channel hops,
/// small enough to keep memory flat.
pub const STREAM_BATCH: usize = 1000;

/// Table and columns that imported rows go into.
#[derive(Clone, Debug)]
pub struct InsertTarget {
    pub schema: Option<String>,
    pub table: String,
    pub columns: Vec<String>,
}

impl InsertTarget {
    fn qualified(&self, quote: char) -> String {
        match &self.schema {
            Some(schema) => format!("{}.{}", quote_ident(schema, quote), quote_ident(&self.table, quote)),
            None => quote_ident(&self.table, quote),
        }
    }

    fn column_list(&self, quote: char) -> String {
        self.columns.iter().map(|c| quote_ident(c, quote)).collect::<Vec<_>>().join(", ")
    }
}

/// The receiving side of a streamed read went away: the transfer was stopped.
pub fn stopped() -> anyhow::Error {
    anyhow::anyhow!("Stopped")
}

/// Retain protocol outcome before converting driver errors to user-facing text.
#[derive(Debug)]
pub(crate) struct ExplainError {
    pub message: String,
    pub cancelled: bool,
    pub uncertain: bool,
}

impl ExplainError {
    pub(crate) fn local(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            cancelled: false,
            uncertain: false,
        }
    }

    pub(crate) fn uncertain(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            cancelled: false,
            uncertain: true,
        }
    }

    pub(crate) fn from_error(error: anyhow::Error, driver: DriverKind) -> Self {
        fn find<T: std::error::Error + Send + Sync + 'static>(error: &anyhow::Error) -> Option<&T> {
            error.chain().find_map(|cause| cause.downcast_ref::<T>())
        }
        let (cancelled, uncertain) = match driver {
            DriverKind::Postgres => {
                find::<tokio_postgres::Error>(&error).map_or((false, true), |error| {
                    error.as_db_error().map_or((false, true), |db| {
                        let (cancelled, uncertain) =
                            pg_explain_error_kind(db.code().code(), db.severity());
                        // The textual severity can be localized. Protocol V
                        // is language-independent on supported PG versions.
                        let fatal = matches!(
                            db.parsed_severity(),
                            Some(
                                tokio_postgres::error::Severity::Fatal
                                    | tokio_postgres::error::Severity::Panic
                            )
                        );
                        (cancelled, uncertain || fatal)
                    })
                })
            }
            DriverKind::MySql => match find::<mysql_async::Error>(&error) {
                Some(mysql_async::Error::Server(error)) => (error.code == 1317, false),
                _ => (false, true),
            },
            // An in-process engine has no transport: every outcome is known.
            DriverKind::Sqlite => match find::<rusqlite::Error>(&error) {
                Some(rusqlite::Error::SqliteFailure(error, _)) => (
                    error.code == rusqlite::ErrorCode::OperationInterrupted,
                    false,
                ),
                _ => (false, false),
            },
        };
        let error = if driver == DriverKind::Postgres {
            postgres::describe(error)
        } else {
            error
        };
        Self {
            message: format!("{error:#}"),
            cancelled,
            uncertain,
        }
    }
}

fn pg_explain_error_kind(code: &str, severity: &str) -> (bool, bool) {
    (
        code == "57014",
        code.starts_with("08") || code == "40003" || matches!(severity, "FATAL" | "PANIC"),
    )
}

pub(crate) struct PreparedExplain {
    statement: PreparedExplainKind,
    epoch: u64,
    started: std::time::Instant,
}

enum PreparedExplainKind {
    Postgres(postgres::PreparedExplain),
    MySql(mysql::PreparedExplain),
}

/// An open session to a data source. Each console and the explorer use their
/// own session, so a long query does not block browsing the schema.
pub struct Connection {
    kind: Kind,
    epoch: u64,
    /// PostgreSQL's client does not expose ReadyForQuery transaction status.
    /// Track explicit SQL; transactions hidden in procedures are not inferred.
    in_tx: bool,
    /// Keeps the shared SSH tunnel open while this session uses it.
    _tunnel: Option<std::sync::Arc<tunnel::Tunnel>>,
}

static NEXT_CONNECTION_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

enum Kind {
    // Boxed: the client and its connection task dwarf the other drivers.
    Postgres(Box<postgres::PgConn>),
    MySql(mysql::MyConn),
    Sqlite(sqlite::LiteConn),
}

/// Address the driver connects to: the database itself, or the local end of an SSH tunnel.
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub tunneled: bool,
}

pub struct ServerInfo {
    pub product: String,
    pub version: String,
    pub transport: String,
}

impl Connection {
    pub async fn open(ds: &DataSource) -> Result<Self> {
        anyhow::ensure!(
            !ds.credentials_unavailable,
            "Saved passwords are unavailable. Retry loading or replace them in Data Sources and Drivers."
        );
        tokio::time::timeout(std::time::Duration::from_secs(15), Self::open_inner(ds))
            .await
            .map_err(|_| anyhow::anyhow!("Database connection setup timed out after 15 seconds"))?
    }

    async fn open_inner(ds: &DataSource) -> Result<Self> {
        let tunnel = if ds.uses_ssh() {
            Some(tunnel::open(ds).await.context("SSH tunnel")?)
        } else {
            None
        };
        let endpoint = match &tunnel {
            Some(t) => Endpoint {
                host: "127.0.0.1".into(),
                port: t.local_port,
                tunneled: true,
            },
            None => Endpoint {
                host: ds.host.clone(),
                port: ds.port,
                tunneled: false,
            },
        };
        let kind = match ds.driver {
            DriverKind::Postgres => Kind::Postgres(Box::new(
                postgres::PgConn::open(ds, &endpoint)
                    .await
                    .map_err(postgres::describe)?,
            )),
            DriverKind::MySql => Kind::MySql(mysql::MyConn::open(ds, &endpoint).await?),
            DriverKind::Sqlite => Kind::Sqlite(sqlite::LiteConn::open(ds).await?),
        };
        Ok(Self {
            kind,
            epoch: NEXT_CONNECTION_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            in_tx: false,
            _tunnel: tunnel,
        })
    }

    pub async fn server_info(&mut self) -> Result<ServerInfo> {
        match &mut self.kind {
            Kind::Postgres(c) => c.server_info().await,
            Kind::MySql(c) => c.server_info().await,
            Kind::Sqlite(c) => c.server_info().await,
        }
    }

    /// Runs one statement and reads the requested window of rows.
    pub async fn execute(&mut self, sql: &str, page: Page) -> Result<QueryResult> {
        let started = std::time::Instant::now();
        let mut result = match &mut self.kind {
            Kind::Postgres(c) => c.execute(sql, page).await.map_err(postgres::describe),
            Kind::MySql(c) => c.execute(sql, page).await,
            Kind::Sqlite(c) => c.execute(sql, page).await,
        }?;
        result.elapsed = started.elapsed();
        if let Some(source) = &mut result.source {
            source.session_epoch = self.epoch;
        }
        if let Some(in_tx) = crate::backend::tx_effect(sql) {
            self.in_tx = in_tx;
        }
        Ok(result)
    }

    /// Records the actual namespace before user SQL and rejects stale result
    /// replay without temporarily changing the console's current namespace.
    pub async fn execute_with_context(
        &mut self,
        sql: &str,
        page: Page,
        expected: Option<&ResolvedContext>,
    ) -> Result<QueryResult> {
        let started = std::time::Instant::now();
        let mut result = match &mut self.kind {
            Kind::Postgres(c) => c
                .execute_with_context(sql, page, expected)
                .await
                .map_err(postgres::describe),
            Kind::MySql(c) => c.execute_with_context(sql, page, expected).await,
            Kind::Sqlite(c) => {
                let context = c.resolved_context();
                check_expected_context(Some(&context), expected)?;
                let mut result = c.execute(sql, page).await?;
                if result.has_result_set() {
                    result.context = Some(context);
                }
                Ok(result)
            }
        }?;
        result.elapsed = started.elapsed();
        if let Some(source) = &mut result.source {
            source.session_epoch = self.epoch;
        }
        if let Some(in_tx) = crate::backend::tx_effect(sql) {
            self.in_tx = in_tx;
        }
        Ok(result)
    }

    /// Internal transaction commands need neither preparation nor result metadata.
    pub(crate) async fn execute_control(&mut self, sql: &str) -> Result<()> {
        match &mut self.kind {
            Kind::Postgres(conn) => conn
                .execute_control(sql)
                .await
                .map_err(postgres::describe)?,
            Kind::MySql(conn) => conn.execute_control(sql).await?,
            Kind::Sqlite(conn) => conn.execute_control(sql).await?,
        }
        if let Some(in_tx) = crate::backend::tx_effect(sql) {
            self.in_tx = in_tx;
        }
        Ok(())
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub(crate) fn clear_prepared_updates(&mut self) {
        if let Kind::Postgres(conn) = &mut self.kind {
            conn.clear_prepared_updates();
        }
    }

    pub(crate) async fn prepare_explain(
        &mut self,
        sql: &str,
        params: &[crate::model::Value],
        expected: Option<&ResolvedContext>,
    ) -> std::result::Result<PreparedExplain, ExplainError> {
        let started = std::time::Instant::now();
        let statement = match &mut self.kind {
            Kind::Postgres(conn) => PreparedExplainKind::Postgres(
                conn.prepare_explain(sql)
                    .await
                    .map_err(|error| ExplainError::from_error(error, DriverKind::Postgres))?,
            ),
            Kind::MySql(conn) => PreparedExplainKind::MySql(
                conn.prepare_explain(sql)
                    .await
                    .map_err(|error| ExplainError::from_error(error, DriverKind::MySql))?,
            ),
            Kind::Sqlite(_) => {
                return Err(ExplainError::local(
                    "SQLite does not support Explain Analyse.",
                ));
            }
        };
        let (context, count) = match &statement {
            PreparedExplainKind::Postgres(prepared) => (&prepared.context, prepared.statement.params().len()),
            PreparedExplainKind::MySql(prepared) => (&prepared.context, usize::from(prepared.statement.num_params())),
        };
        check_expected_context(Some(context), expected).map_err(|error| ExplainError::local(error.to_string()))?;
        if count != params.len() {
            return Err(ExplainError::local("EXPLAIN parameter count mismatch."));
        }
        if matches!(statement, PreparedExplainKind::Postgres(_)) {
            postgres::validate_explain_params(params)
                .map_err(|error| ExplainError::local(error.to_string()))?;
        }
        Ok(PreparedExplain {
            statement,
            epoch: self.epoch,
            started,
        })
    }

    pub(crate) async fn execute_explain(
        &mut self,
        prepared: PreparedExplain,
        params: &[crate::model::Value],
        expected: Option<&ResolvedContext>,
    ) -> std::result::Result<QueryResult, ExplainError> {
        if prepared.epoch != self.epoch {
            return Err(ExplainError::local(
                "The prepared plan belongs to another database session.",
            ));
        }
        let mut data = match (&mut self.kind, prepared.statement) {
            (Kind::Postgres(conn), PreparedExplainKind::Postgres(statement)) => conn
                .execute_explain(statement, params, expected)
                .await
                .map_err(|error| ExplainError::from_error(error, DriverKind::Postgres))?,
            (Kind::MySql(conn), PreparedExplainKind::MySql(statement)) => conn
                .execute_explain(statement, params, expected)
                .await
                .map_err(|error| ExplainError::from_error(error, DriverKind::MySql))?,
            _ => {
                return Err(ExplainError::local(
                    "The prepared plan belongs to another database driver.",
                ));
            }
        };
        data.elapsed = prepared.started.elapsed();
        if let Some(source) = &mut data.source {
            source.session_epoch = self.epoch;
        }
        Ok(data)
    }

    pub(crate) async fn explain_control(
        &mut self,
        sql: &str,
    ) -> std::result::Result<(), ExplainError> {
        match &mut self.kind {
            Kind::Postgres(conn) => conn
                .execute_control(sql)
                .await
                .map_err(|error| ExplainError::from_error(error, DriverKind::Postgres))?,
            Kind::MySql(conn) => conn
                .execute_control(sql)
                .await
                .map_err(|error| ExplainError::from_error(error, DriverKind::MySql))?,
            Kind::Sqlite(conn) => conn
                .execute_control(sql)
                .await
                .map_err(|error| ExplainError::from_error(error, DriverKind::Sqlite))?,
        }
        if let Some(in_tx) = crate::backend::tx_effect(sql) {
            self.in_tx = in_tx;
        }
        Ok(())
    }

    pub(crate) async fn explain_context(
        &mut self,
    ) -> std::result::Result<ResolvedContext, ExplainError> {
        match &mut self.kind {
            Kind::Postgres(conn) => conn
                .resolved_context()
                .await
                .map_err(|error| ExplainError::from_error(error, DriverKind::Postgres)),
            Kind::MySql(conn) => conn
                .resolved_context()
                .await
                .map_err(|error| ExplainError::from_error(error, DriverKind::MySql)),
            Kind::Sqlite(conn) => Ok(conn.resolved_context()),
        }
    }

    pub async fn table_edit_metadata(&mut self, origin: &OriginTable) -> Result<TableEditMetadata> {
        match &mut self.kind {
            Kind::Postgres(c) => c
                .table_edit_metadata(origin)
                .await
                .map_err(postgres::describe),
            Kind::MySql(c) => c.table_edit_metadata(origin).await,
            Kind::Sqlite(c) => c.table_edit_metadata(origin).await,
        }
    }

    pub async fn execute_bound_update(&mut self, update: &BoundUpdate) -> Result<u64> {
        match &mut self.kind {
            Kind::Postgres(c) => c
                .execute_bound_update(update)
                .await
                .map_err(postgres::describe),
            Kind::MySql(c) => c.execute_bound_update(update).await,
            Kind::Sqlite(c) => c.execute_bound_update(update).await,
        }
    }

    pub async fn foreign_keys(
        &mut self,
        origin: &OriginTable,
        direction: RelatedDirection,
    ) -> Result<RelatedMetadata> {
        match &mut self.kind {
            Kind::Postgres(conn) => conn
                .foreign_keys(origin, direction)
                .await
                .map_err(postgres::describe),
            Kind::MySql(conn) => conn.foreign_keys(origin, direction).await,
            Kind::Sqlite(conn) => conn.foreign_keys(origin, direction).await,
        }
    }

    /// Objects, metrics and relations of one database for Database Graph,
    /// from catalogs only. MySQL uses `database` as the schema; SQLite reads `main`.
    pub async fn schema_graph(
        &mut self,
        database: &str,
        schemas: Option<&[String]>,
    ) -> Result<crate::schema_graph::SchemaGraph> {
        match &mut self.kind {
            Kind::Postgres(conn) => conn
                .schema_graph(database, schemas)
                .await
                .map_err(postgres::describe),
            Kind::MySql(conn) => conn.schema_graph(database).await,
            Kind::Sqlite(conn) => conn.schema_graph().await,
        }
    }

    /// Reads every row of a query without a page limit and sends it in
    /// batches. The bounded channel slows the read down to the consumer, so
    /// memory stays flat for results of any size. Returns the row count.
    pub async fn stream_rows(
        &mut self,
        query: &BoundQuery,
        tx: tokio::sync::mpsc::Sender<RowBatch>,
    ) -> Result<u64> {
        match &mut self.kind {
            Kind::Postgres(conn) => conn.stream_rows(query, tx).await.map_err(postgres::describe),
            Kind::MySql(conn) => conn.stream_rows(query, tx).await,
            Kind::Sqlite(conn) => conn.stream_rows(query, tx).await,
        }
    }

    /// Inserts rows of text values (`None` is NULL) and lets the database
    /// convert them to the column types. PostgreSQL loads them with COPY.
    pub async fn insert_text_rows(&mut self, target: &InsertTarget, rows: &[Vec<Option<String>>]) -> Result<()> {
        match &mut self.kind {
            Kind::Postgres(conn) => conn.copy_text_rows(target, rows).await.map_err(postgres::describe),
            Kind::MySql(conn) => conn.insert_text_rows(target, rows).await,
            Kind::Sqlite(conn) => conn.insert_text_rows(target, rows).await,
        }
    }

    pub async fn execute_bound_query(
        &mut self,
        query: &BoundQuery,
        page: Page,
        expected: Option<&ResolvedContext>,
    ) -> Result<QueryResult> {
        let started = std::time::Instant::now();
        let mut result = match &mut self.kind {
            Kind::Postgres(conn) => conn
                .execute_bound_query(query, page, expected)
                .await
                .map_err(postgres::describe),
            Kind::MySql(conn) => conn.execute_bound_query(query, page, expected).await,
            Kind::Sqlite(conn) => conn.execute_bound_query(query, page, expected).await,
        }?;
        result.elapsed = started.elapsed();
        if let Some(source) = &mut result.source {
            source.session_epoch = self.epoch;
        }
        Ok(result)
    }

    pub async fn resolved_context(&mut self) -> Result<ResolvedContext> {
        match &mut self.kind {
            Kind::Postgres(c) => c.resolved_context().await.map_err(postgres::describe),
            Kind::MySql(c) => c.resolved_context().await,
            Kind::Sqlite(c) => Ok(c.resolved_context()),
        }
    }

    pub async fn has_open_transaction(&mut self) -> Result<bool> {
        match &mut self.kind {
            Kind::Postgres(_) => Ok(self.in_tx),
            Kind::MySql(c) => c.has_open_transaction().await,
            Kind::Sqlite(c) => c.has_open_transaction().await,
        }
    }

    /// Transaction status after the last successful statement, without a
    /// round trip: MySQL reports it in every OK packet (DDL commits
    /// implicitly), SQLite in its autocommit mode. PostgreSQL ends a
    /// transaction only through explicit SQL, which is tracked.
    pub async fn transaction_state(&mut self) -> Result<bool> {
        match &mut self.kind {
            Kind::Postgres(_) => Ok(self.in_tx),
            Kind::MySql(c) => Ok(c.last_known_in_transaction()),
            Kind::Sqlite(c) => c.has_open_transaction().await,
        }
    }

    /// Explicitly reapplies even an unchanged selection, allowing Default to
    /// undo a manually typed SET/USE without reconnecting the console.
    pub async fn change_schema(&mut self, selection: &SchemaSelection) -> Result<ResolvedContext> {
        anyhow::ensure!(
            !self.has_open_transaction().await?,
            "Commit or roll back the open transaction before changing the schema."
        );
        match &mut self.kind {
            Kind::Postgres(c) => c.change_schema(selection).await.map_err(postgres::describe),
            Kind::MySql(c) => c.change_schema(selection).await,
            Kind::Sqlite(c) => c.change_schema(selection).await,
        }
    }

    /// Restores namespace only on a dedicated connection, e.g. full export.
    /// Does not reproduce temporary objects, roles, variables or transactions.
    pub async fn apply_resolved_context(&mut self, context: &ResolvedContext) -> Result<()> {
        anyhow::ensure!(
            context.temporary_schema.is_none(),
            "This result was created in a session with temporary objects. Export the fetched rows instead; a new connection cannot reproduce that namespace."
        );
        match &mut self.kind {
            Kind::Postgres(c) => c
                .apply_resolved_context(context)
                .await
                .map_err(postgres::describe),
            Kind::MySql(c) => c.apply_resolved_context(context).await,
            Kind::Sqlite(c) => check_expected_context(Some(&c.resolved_context()), Some(context)),
        }
    }

    /// Loads children of an explorer node. `node` is `None` for the data source root.
    pub async fn children(&mut self, node: Option<&DbObject>) -> Result<Vec<DbObject>> {
        let (kind, path) = match node {
            None => (None, &[][..]),
            Some(n) => (Some(n.kind), n.path.as_slice()),
        };
        match &mut self.kind {
            Kind::Postgres(c) => c.children(kind, path).await.map_err(postgres::describe),
            Kind::MySql(c) => c.children(kind, path).await,
            Kind::Sqlite(c) => c.children(kind, path).await,
        }
    }

    /// All user tables with their columns, for code completion.
    pub async fn completion_meta(&mut self) -> Result<Vec<TableMeta>> {
        match &mut self.kind {
            Kind::Postgres(c) => c.completion_meta().await.map_err(postgres::describe),
            Kind::MySql(c) => c.completion_meta().await,
            Kind::Sqlite(c) => c.completion_meta().await,
        }
    }

    pub async fn completion_metadata(&mut self) -> Result<CompletionMetadata> {
        let default_context = self.resolved_context().await?;
        let schemas = match &mut self.kind {
            Kind::Postgres(c) => c.schema_names().await.map_err(postgres::describe)?,
            Kind::MySql(c) => c.schema_names().await?,
            Kind::Sqlite(_) => vec!["main".into()],
        };
        let tables = self.completion_meta().await?;
        Ok(CompletionMetadata {
            tables,
            schemas,
            default_context,
        })
    }

    /// Primary key columns in key order; empty when the table has none.
    pub async fn primary_key(&mut self, schema: Option<&str>, table: &str) -> Result<Vec<String>> {
        match &mut self.kind {
            Kind::Postgres(c) => c
                .primary_key(schema.unwrap_or("public"), table)
                .await
                .map_err(postgres::describe),
            Kind::MySql(c) => c.primary_key(schema, table).await,
            Kind::Sqlite(c) => c.primary_key(schema.unwrap_or("main"), table).await,
        }
    }

    /// CREATE statement for an explorer object.
    pub async fn ddl(&mut self, node: &DbObject) -> Result<String> {
        match &mut self.kind {
            Kind::Postgres(c) => c.ddl(node).await.map_err(postgres::describe),
            Kind::MySql(c) => c.ddl(node).await,
            Kind::Sqlite(c) => c.ddl(node).await,
        }
    }

    /// Freshly validates the physical object type and reports definition
    /// coverage for the read-only DDL comparison, on a dedicated connection.
    pub async fn ddl_for_comparison(&mut self, node: &DbObject) -> Result<(String, Vec<String>)> {
        anyhow::ensure!(
            !self.has_open_transaction().await?,
            "DDL comparison requires a dedicated session outside an existing transaction."
        );
        match &mut self.kind {
            Kind::Postgres(conn) => conn
                .ddl_for_comparison(node)
                .await
                .map_err(postgres::describe),
            Kind::MySql(conn) => conn.ddl_for_comparison(node).await,
            Kind::Sqlite(conn) => conn.ddl_for_comparison(node).await,
        }
    }

    /// Token that can abort the running statement from another task.
    pub fn cancel_handle(&self) -> Option<CancelHandle> {
        match &self.kind {
            Kind::Postgres(c) => Some(c.cancel_handle()),
            Kind::Sqlite(c) => Some(CancelHandle::Sqlite(c.interrupt_handle())),
            Kind::MySql(c) => c.mcp_cancel_handle().map(CancelHandle::McpMySql).or_else(||c.cancel_handle().map(CancelHandle::MySql)),
        }
    }
}

fn check_expected_context(
    actual: Option<&ResolvedContext>,
    expected: Option<&ResolvedContext>,
) -> Result<()> {
    anyhow::ensure!(
        expected.is_none() || actual == expected,
        "The console schema or database has changed since this result was created. Execute the query again before reloading or changing pages."
    );
    Ok(())
}

pub enum CancelHandle {
    Postgres(tokio_postgres::CancelToken),
    PostgresTls(tokio_postgres::CancelToken, native_tls::TlsConnector),
    Sqlite(std::sync::Arc<rusqlite::InterruptHandle>),
    MySql(mysql::MyCancel),
    McpPostgres(postgres::mcp::McpCancel),
    McpMySql(mysql::mcp::McpCancel),
}

impl CancelHandle {
    pub async fn cancel(self) -> Result<()> {
        match self {
            CancelHandle::Postgres(t) => Ok(t.cancel_query(tokio_postgres::NoTls).await?),
            CancelHandle::PostgresTls(t, tls) => {
                Ok(
                    t.cancel_query(postgres_native_tls::MakeTlsConnector::new(tls))
                        .await?,
                )
            }
            CancelHandle::Sqlite(h) => {
                h.interrupt();
                Ok(())
            }
            CancelHandle::MySql(c) => c.cancel().await,
            CancelHandle::McpPostgres(c) => c.cancel().await,
            CancelHandle::McpMySql(c) => c.cancel().await,
        }
    }
}

/// Standard folder layout for a table or view node, shared by all drivers.
fn table_folders(
    path: &[String],
    columns: Vec<DbObject>,
    keys: Vec<DbObject>,
    indexes: Vec<DbObject>,
) -> Vec<DbObject> {
    let mut out = vec![DbObject::folder("columns", path, columns)];
    if !keys.is_empty() {
        out.push(DbObject::folder("keys", path, keys));
    }
    if !indexes.is_empty() {
        out.push(DbObject::folder("indexes", path, indexes));
    }
    out
}

/// Groups schema objects into folders by kind, skipping empty ones.
fn schema_folders(path: &[String], groups: Vec<(&str, Vec<DbObject>)>) -> Vec<DbObject> {
    groups
        .into_iter()
        .filter(|(_, items)| !items.is_empty())
        .map(|(name, items)| DbObject::folder(name, path, items))
        .collect()
}

fn table_kind(is_view: bool) -> ObjectKind {
    if is_view {
        ObjectKind::View
    } else {
        ObjectKind::Table
    }
}

/// Quotes an identifier with the given quote character.
pub fn quote_ident(name: &str, quote: char) -> String {
    let doubled = format!("{quote}{quote}");
    format!("{quote}{}{quote}", name.replace(quote, &doubled))
}

/// `SELECT *` for a table, as the explorer opens it on double click.
pub fn select_all(driver: DriverKind, schema: Option<&str>, table: &str) -> String {
    let q = if driver == DriverKind::MySql {
        '`'
    } else {
        '"'
    };
    match schema {
        Some(s) => format!(
            "SELECT * FROM {}.{}",
            quote_ident(s, q),
            quote_ident(table, q)
        ),
        None => format!("SELECT * FROM {}", quote_ident(table, q)),
    }
}

/// A table editor's SELECT limited on the server to one page, and the window
/// to read from its result. It asks for one row more than the page to tell
/// whether another page exists. `keys` order the rows when the user gave no
/// ORDER BY. New lines end a trailing `--` comment in the user's filter.
pub fn page_query(driver: DriverKind, sql: &str, keys: &[String], page: Page) -> (String, Page) {
    if page.limit == usize::MAX {
        return (sql.to_string(), page);
    }
    let q = if driver == DriverKind::MySql { '`' } else { '"' };
    let mut out = sql.to_string();
    if !keys.is_empty() {
        let keys: Vec<String> = keys.iter().map(|key| quote_ident(key, q)).collect();
        out.push_str(&format!("\nORDER BY {}", keys.join(", ")));
    }
    out.push_str(&format!("\nLIMIT {} OFFSET {}", page.limit + 1, page.skip));
    (out, Page::first(page.limit))
}

/// Groups (schema, table, is_view, column, type) rows ordered by table into [`TableMeta`].
fn group_meta(
    rows: impl Iterator<Item = (String, String, bool, Option<String>, Option<String>)>,
) -> Vec<TableMeta> {
    let mut out: Vec<TableMeta> = Vec::new();
    for (schema, name, is_view, col, ty) in rows {
        let same = out
            .last()
            .is_some_and(|t| t.schema == schema && t.name == name);
        if !same {
            out.push(TableMeta {
                schema,
                name,
                is_view,
                columns: Vec::new(),
            });
        }
        if let Some(col) = col {
            out.last_mut()
                .unwrap()
                .columns
                .push((col, ty.unwrap_or_default()));
        }
    }
    out
}

/// Opening transaction statement for manual transaction mode.
pub fn begin_statement(driver: DriverKind) -> &'static str {
    match driver {
        DriverKind::MySql => "START TRANSACTION",
        _ => "BEGIN",
    }
}

#[cfg(test)]
mod context_tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[tokio::test]
    #[ignore = "release benchmark against test containers: cargo test --release --lib context_tests::benchmark -- --ignored --nocapture"]
    async fn benchmark() {
        const SAMPLES: usize = 7;
        const ITERATIONS: u32 = 50;
        for driver in [DriverKind::Postgres, DriverKind::MySql] {
            let mut source = DataSource::new(driver);
            source.host = "127.0.0.1".into();
            source.port = if driver == DriverKind::Postgres {
                55432
            } else {
                53306
            };
            source.user = "dbeast".into();
            source.password = "dbeast".into();
            source.database = "dbeast".into();
            let mut conn = Connection::open(&source)
                .await
                .expect("test database must be running");
            for _ in 0..5 {
                conn.execute("select 1", Page::first(500)).await.unwrap();
                conn.execute_with_context("select 1", Page::first(500), None)
                    .await
                    .unwrap();
            }
            let mut plain = [Duration::ZERO; SAMPLES];
            let mut captured = [Duration::ZERO; SAMPLES];
            for sample in 0..SAMPLES {
                // Alternate order to reduce warm-up or scheduling bias.
                for capture in if sample % 2 == 0 {
                    [false, true]
                } else {
                    [true, false]
                } {
                    let start = Instant::now();
                    for _ in 0..ITERATIONS {
                        let result = if capture {
                            conn.execute_with_context("select 1", Page::first(500), None)
                                .await
                        } else {
                            conn.execute("select 1", Page::first(500)).await
                        }
                        .unwrap();
                        std::hint::black_box(result);
                    }
                    let elapsed = start.elapsed() / ITERATIONS;
                    if capture {
                        captured[sample] = elapsed;
                    } else {
                        plain[sample] = elapsed;
                    }
                }
            }
            plain.sort_unstable();
            captured.sort_unstable();
            println!(
                "context {} (7 warm samples x 50 SELECT 1, median per query): execute={:.3}ms with_context={:.3}ms delta={:+.3}ms",
                driver.label(),
                plain[SAMPLES / 2].as_secs_f64() * 1000.0,
                captured[SAMPLES / 2].as_secs_f64() * 1000.0,
                (captured[SAMPLES / 2].as_secs_f64() - plain[SAMPLES / 2].as_secs_f64()) * 1000.0,
            );
        }
    }
}

#[cfg(test)]
mod explain_error_tests {
    use super::*;

    #[test]
    fn sqlstate_classification_preserves_cancellation_and_uncertainty() {
        assert_eq!(pg_explain_error_kind("57014", "ERROR"), (true, false));
        assert_eq!(pg_explain_error_kind("23505", "ERROR"), (false, false));
        assert_eq!(pg_explain_error_kind("42601", "ERROR"), (false, false));
        for (code, severity) in [
            ("08006", "ERROR"),
            ("40003", "ERROR"),
            ("57P01", "FATAL"),
            ("XX000", "PANIC"),
        ] {
            assert_eq!(pg_explain_error_kind(code, severity), (false, true));
        }
        // Error text is deliberately not used as an outcome classifier.
        let error = ExplainError::from_error(
            anyhow::anyhow!("[57014] cancelled text"),
            DriverKind::Postgres,
        );
        assert!(!error.cancelled);
        assert!(error.uncertain);
        let wrapped = anyhow::Error::new(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_INTERRUPT),
            None,
        ))
        .context("while preparing");
        let error = ExplainError::from_error(wrapped, DriverKind::Sqlite);
        assert!(error.cancelled && !error.uncertain);
        let error = ExplainError::from_error(
            anyhow::anyhow!("no such column: missing_column"),
            DriverKind::Sqlite,
        );
        assert!(!error.cancelled && !error.uncertain);
    }
}

#[cfg(test)]
mod credential_tests {
    #[tokio::test]
    async fn unavailable_credentials_never_open_a_database() {
        let path = std::env::temp_dir().join(format!("dbeast-blocked-{}.sqlite", uuid::Uuid::new_v4()));
        let mut source = crate::model::DataSource::new(crate::model::DriverKind::Sqlite);
        source.file = path.to_string_lossy().into_owned();
        source.credentials_unavailable = true;
        let error = match super::Connection::open(&source).await { Ok(_) => panic!("opened despite unavailable credentials"), Err(error) => error };
        assert!(error.to_string().contains("passwords are unavailable"));
        assert!(!path.exists());
        for strict in [true, false] {
            let error = match super::Connection::open_mcp(&source, strict, None).await {
                Ok(_) => panic!("MCP opened despite unavailable credentials"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("credentials are unavailable"));
            assert!(!path.exists());
        }
    }
}

#[cfg(test)]
mod page_query_tests {
    use super::*;

    #[test]
    fn table_pages_are_limited_on_the_server_for_every_driver() {
        let page = Page { skip: 1000, limit: 500 };
        let keys = vec!["id".to_string(), "Part".to_string()];
        let cases = [
            (DriverKind::Postgres, r#"SELECT * FROM "public"."t""#, "\nORDER BY \"id\", \"Part\"\nLIMIT 501 OFFSET 1000"),
            (DriverKind::MySql, "SELECT * FROM `shop`.`t`", "\nORDER BY `id`, `Part`\nLIMIT 501 OFFSET 1000"),
            (DriverKind::Sqlite, r#"SELECT * FROM "t""#, "\nORDER BY \"id\", \"Part\"\nLIMIT 501 OFFSET 1000"),
        ];
        for (driver, base, tail) in cases {
            let (sql, window) = page_query(driver, base, &keys, page);
            assert_eq!(sql, format!("{base}{tail}"));
            // The server already skipped: read the page and the one extra row.
            assert_eq!(window, Page::first(500));
        }
        // The user's ORDER BY is kept; a trailing comment cannot swallow LIMIT.
        let (sql, _) = page_query(
            DriverKind::Postgres,
            r#"SELECT * FROM "t" WHERE a > 1 -- recent ORDER BY b DESC"#,
            &[],
            Page::first(500),
        );
        assert_eq!(sql, "SELECT * FROM \"t\" WHERE a > 1 -- recent ORDER BY b DESC\nLIMIT 501 OFFSET 0");
        assert_eq!(page_query(DriverKind::Sqlite, "SELECT 1", &[], Page::ALL), ("SELECT 1".into(), Page::ALL));
    }
}
