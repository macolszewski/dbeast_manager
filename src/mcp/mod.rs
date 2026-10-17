//! Local, explicitly authorized access for MCP clients.

mod broker;
pub mod executor;
mod protocol;
mod server;

use crate::model::DataSource;
pub use broker::McpController;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub const DEFAULT_PORT: u16 = 54832;
pub const MAX_ROWS: u16 = 500;
pub const MAX_SQL_BYTES: usize = 64 * 1024;
/// Reserve space for both MCP text content and structuredContent plus JSON escaping.
pub const MAX_RESULT_BYTES: usize = 256 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
pub const MAX_SHARED_FILE_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_SHARED_TEXT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_CELL_BYTES: usize = 64 * 1024;

/// Private source snapshots never cross the MCP serialization boundary.
#[derive(Clone)]
pub struct SharedSource {
    pub alias: String,
    pub source: DataSource,
    /// Who sees this source and their standing access: `None` is every agent
    /// connection, `Some(id)` one connection. Operations outside it need approval.
    pub audiences: Vec<(Option<String>, ShareAccess)>,
}

impl SharedSource {
    /// Shared with every agent connection, without standing access.
    pub fn for_all_agents(alias: impl Into<String>, source: DataSource) -> Self {
        Self {
            alias: alias.into(),
            source,
            audiences: vec![(None, ShareAccess::default())],
        }
    }

    /// Union of the entries that apply to one connection, or `None` when the
    /// source is not shared with it at all.
    pub fn access_for(&self, client_id: &str) -> Option<ShareAccess> {
        let mut result: Option<ShareAccess> = None;
        for (_, access) in self
            .audiences
            .iter()
            .filter(|(audience, _)| audience.as_deref().is_none_or(|id| id == client_id))
        {
            let merged = result.get_or_insert_with(ShareAccess::default);
            merged.source = merged.source.max(access.source);
            for (table, level) in &access.tables {
                match merged.tables.iter_mut().find(|(t, _)| t == table) {
                    Some((_, existing)) => *existing = (*existing).max(*level),
                    None => merged.tables.push((table.clone(), *level)),
                }
            }
        }
        result
    }
}

/// Database-wide standing access for every session. Ordered from least access.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceAccess {
    /// Only listed in the catalog; every operation needs approval.
    #[default]
    Ask,
    Read,
    ReadWrite,
}

impl SourceAccess {
    pub fn public_name(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Read => "read",
            Self::ReadWrite => "read_write",
        }
    }
}

/// Standing access to one physical table. Ordered from least access.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TableAccess {
    Read,
    /// Read plus single-statement INSERT, UPDATE or DELETE that references only this table.
    ReadWrite,
}

impl TableAccess {
    pub fn public_name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::ReadWrite => "read_write",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShareAccess {
    pub source: SourceAccess,
    pub tables: Vec<(TableRef, TableAccess)>,
}

impl std::fmt::Debug for SharedSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedSource")
            .field("alias", &self.alias)
            .field("driver", &self.source.driver)
            .field("audiences", &self.audiences)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct SharedFile {
    pub id: String,
    pub alias: String,
    pub version: u64,
    pub text: Arc<str>,
}

impl std::fmt::Debug for SharedFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedFile")
            .field("alias", &self.alias)
            .field("version", &self.version)
            .field("bytes", &self.text.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TableRef {
    pub schema: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum Operation {
    ListTables {
        source_id: String,
    },
    DescribeTable {
        source_id: String,
        table: TableRef,
    },
    ReadTable {
        source_id: String,
        table: TableRef,
        columns: Vec<String>,
        limit: u16,
        offset: u32,
    },
    Sql {
        source_id: String,
        sql: String,
    },
    ReadSql {
        source_id: String,
        sql: String,
    },
    ReadFile {
        file_id: String,
        version: u64,
        start: usize,
        max_chars: usize,
    },
}

impl Operation {
    pub fn source_id(&self) -> Option<&str> {
        match self {
            Self::ListTables { source_id }
            | Self::DescribeTable { source_id, .. }
            | Self::ReadTable { source_id, .. }
            | Self::Sql { source_id, .. }
            | Self::ReadSql { source_id, .. } => Some(source_id),
            Self::ReadFile { .. } => None,
        }
    }
    pub fn table(&self) -> Option<&TableRef> {
        match self {
            Self::DescribeTable { table, .. } | Self::ReadTable { table, .. } => Some(table),
            _ => None,
        }
    }
    pub fn is_full_sql(&self) -> bool {
        matches!(self, Self::Sql { .. })
    }
}

pub struct ExecutionRequest {
    pub source: DataSource,
    pub operation: Operation,
    pub expected_table_identity: Option<String>,
    pub cancellation: CancellationToken,
    /// Capacity follows detached native work until its real resources close.
    /// This opaque guard is never serialized or exposed through MCP.
    pub execution_lease: Option<Arc<dyn Send + Sync>>,
}

pub struct ExecutionOutput {
    pub value: serde_json::Value,
    pub table_identity: Option<String>,
}

#[derive(Debug)]
pub struct ExecutionError {
    pub code: &'static str,
    /// Authored policy text only. Native diagnostics must be discarded by the
    /// driver adapter; dynamic messages cannot enter this public error type.
    pub message: &'static str,
    pub outcome_unknown: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Approval {
    Deny,
    Once,
    ReadTableForSession,
    ReadSourceForSession,
    FullSqlForSession,
}

#[derive(Clone, Debug, Default)]
pub struct UiSnapshot {
    pub running: bool,
    pub starting: bool,
    pub endpoint: Option<String>,
    pub error: Option<String>,
    pub clients: Vec<ClientView>,
    pub sessions: Vec<SessionView>,
    pub requests: Vec<RequestView>,
    pub audit: Vec<AuditView>,
}

#[derive(Clone, Debug)]
pub struct ClientView {
    pub id: String,
    pub label: String,
}

#[derive(Clone, Debug)]
pub struct SessionView {
    pub id: String,
    pub client_id: String,
    pub label: String,
    /// When the session ends; the UI counts down, so the broker does not
    /// publish a new snapshot every second.
    pub expires_at: std::time::Instant,
    pub grants: Vec<GrantView>,
}

#[derive(Clone, Debug)]
pub struct GrantView {
    pub id: String,
    pub alias: String,
    pub scope: String,
}

/// Local data source behind a request, used only by the DBeast UI (for example to
/// open the SQL in a query console). Never serialized to MCP clients.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalTarget {
    pub source_id: String,
    pub database: String,
}

#[derive(Clone, Debug)]
pub struct RequestView {
    pub id: String,
    pub session_id: String,
    pub client_label: String,
    pub alias: String,
    pub operation: Operation,
    pub eligible_table: Option<TableRef>,
    /// Exact SQL or immutable document text shown before approval.
    pub preview: String,
    pub status: String,
    pub can_allow_full_sql: bool,
    pub local: Option<LocalTarget>,
}

#[derive(Clone, Debug)]
pub struct AuditView {
    pub at: String,
    pub session_id: String,
    pub request_id: String,
    pub alias: String,
    pub action: String,
    pub status: String,
    /// Exact SQL or generated read statement; absent for document reads.
    pub sql: Option<Arc<str>>,
    pub local: Option<LocalTarget>,
}

/// A connection that survives restarts. Only the SHA-256 of its token is kept.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredClient {
    pub id: String,
    pub label: String,
    /// Hex SHA-256 of the bearer token.
    pub token_hash: String,
}

impl std::fmt::Debug for StoredClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredClient")
            .field("id", &self.id)
            .field("label", &self.label)
            .finish_non_exhaustive()
    }
}

/// One table entry of a remembered share.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredTable {
    pub schema: String,
    pub name: String,
    pub access: TableAccess,
}

/// Standing access of one audience: `client_id: None` is every agent connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredAudience {
    pub client_id: Option<String>,
    pub access: SourceAccess,
    pub tables: Vec<StoredTable>,
}

/// A database shared with agents, by local data source ID and optional database.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredShare {
    pub source_id: String,
    pub database: Option<String>,
    pub alias: String,
    pub audiences: Vec<StoredAudience>,
}

/// Agent connections and sharing kept in the Keychain. Integrity matters as much
/// as secrecy: an agent with file access must not be able to grant itself access.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpState {
    pub clients: Vec<StoredClient>,
    pub shares: Vec<StoredShare>,
}

/// Intentionally lacks Debug and Serialize: only show after a user's copy action.
pub struct ClientConnection {
    pub id: String,
    pub token: String,
    pub configuration: String,
}
