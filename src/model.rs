use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DriverKind {
    Postgres,
    MySql,
    Sqlite,
}

impl DriverKind {
    pub const ALL: [DriverKind; 3] = [DriverKind::Postgres, DriverKind::MySql, DriverKind::Sqlite];

    pub fn label(self) -> &'static str {
        match self {
            DriverKind::Postgres => "PostgreSQL",
            DriverKind::MySql => "MySQL",
            DriverKind::Sqlite => "SQLite",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            DriverKind::Postgres => 5432,
            DriverKind::MySql => 3306,
            DriverKind::Sqlite => 0,
        }
    }

    pub fn is_file_based(self) -> bool {
        matches!(self, DriverKind::Sqlite)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SshAuth {
    #[default]
    Password,
    /// Private key file, optionally protected by a passphrase.
    KeyPair,
    /// Keys held by the running ssh-agent (SSH_AUTH_SOCK).
    Agent,
}

impl SshAuth {
    pub const ALL: [SshAuth; 3] = [SshAuth::Password, SshAuth::KeyPair, SshAuth::Agent];

    pub fn label(self) -> &'static str {
        match self {
            SshAuth::Password => "Password",
            SshAuth::KeyPair => "Key pair",
            SshAuth::Agent => "OpenSSH agent",
        }
    }
}

/// SSH tunnel to reach a database that is not directly accessible.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SshConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: SshAuth,
    #[serde(default, skip_serializing)]
    pub password: String,
    pub key_file: String,
    #[serde(default, skip_serializing)]
    pub passphrase: String,
}

impl Default for SshConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: String::new(),
            port: 22,
            user: String::new(),
            auth: SshAuth::Password,
            password: String::new(),
            key_file: String::new(),
            passphrase: String::new(),
        }
    }
}

/// Which databases and schemas a data source shows in the explorer, like
/// Database and schema chooser. The default database is always shown.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Scope {
    pub all_databases: bool,
    /// Databases shown besides the default one.
    pub databases: Vec<String>,
    /// Schemas hidden per database; schemas not listed are shown.
    pub hidden_schemas: std::collections::BTreeMap<String, Vec<String>>,
}

/// Public TLS settings; identity passwords are stored separately in the vault.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TlsConfig {
    pub ca_file: String,
    /// Client certificate: a PEM certificate (chain) with `key_file`, or a
    /// PKCS#12 archive with the certificate and key when `key_file` is empty.
    pub identity_file: String,
    /// PEM private key of the client certificate (PKCS#8, PKCS#1 RSA or SEC1 EC).
    pub key_file: String,
    pub mode: TlsMode,
}

/// How the server certificate is checked (SSL modes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TlsMode {
    /// Encrypted, without checking the certificate.
    Require,
    /// The certificate must be signed by a trusted CA; the host name is not checked.
    VerifyCa,
    /// The certificate must be trusted and issued for the host name.
    #[default]
    VerifyFull,
}

impl TlsMode {
    pub const ALL: [TlsMode; 3] = [TlsMode::Require, TlsMode::VerifyCa, TlsMode::VerifyFull];

    /// What a successful connection in this mode has proved, for Test Connection.
    pub fn transport(self) -> &'static str {
        match self {
            TlsMode::Require => "TLS (certificate not verified)",
            TlsMode::VerifyCa => "TLS (CA verified, host name not checked)",
            TlsMode::VerifyFull => "TLS (verified)",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            TlsMode::Require => "Require",
            TlsMode::VerifyCa => "Verify CA",
            TlsMode::VerifyFull => "Full verification",
        }
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct DataSource {
    pub id: String,
    pub name: String,
    pub driver: DriverKind,
    pub host: String,
    pub port: u16,
    pub user: String,
    #[serde(default, skip_serializing)]
    pub password: String,
    #[serde(default, skip_serializing)]
    pub tls_identity_password: String,
    pub database: String,
    /// SQLite database file.
    pub file: String,
    pub use_tls: bool,
    #[serde(default)]
    pub tls: TlsConfig,
    pub read_only: bool,
    #[serde(default)]
    pub ssh: SshConfig,
    #[serde(default)]
    pub scope: Scope,
    #[serde(default)]
    pub password_storage: PasswordStorage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_ref: Option<String>,
    #[serde(skip)]
    pub credentials_unavailable: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PasswordStorage {
    #[default]
    Forever,
    ForSession,
}

impl fmt::Debug for DataSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DataSource")
            .field("id", &self.id)
            .field("driver", &self.driver)
            .field("credentials", &"[REDACTED]")
            .finish()
    }
}

impl fmt::Debug for SshConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SshConfig")
            .field("enabled", &self.enabled)
            .field("auth", &self.auth)
            .field("credentials", &"[REDACTED]")
            .finish()
    }
}

/// Connection target and behavior captured by a result, without credentials.
/// A source's display name and explorer scope do not change its identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceIdentity {
    pub driver: DriverKind,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub database: String,
    pub file: String,
    pub use_tls: bool,
    #[serde(default)]
    pub tls: TlsConfig,
    pub read_only: bool,
    pub ssh: Option<SshIdentity>,
}

/// The effective SSH peer; authentication material is deliberately excluded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshIdentity {
    pub host: String,
    pub port: u16,
    pub user: String,
}

impl DataSource {
    pub fn new(driver: DriverKind) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: String::new(),
            driver,
            host: "localhost".into(),
            port: driver.default_port(),
            user: String::new(),
            password: String::new(),
            database: String::new(),
            file: String::new(),
            use_tls: false,
            tls: TlsConfig::default(),
            tls_identity_password: String::new(),
            read_only: false,
            ssh: SshConfig::default(),
            scope: Scope::default(),
            password_storage: if cfg!(target_os = "macos") {
                PasswordStorage::Forever
            } else {
                PasswordStorage::ForSession
            },
            credential_ref: None,
            credentials_unavailable: false,
        }
    }

    /// The same data source connected to another database of the server.
    pub fn with_database(&self, database: &str) -> DataSource {
        let mut ds = self.clone();
        ds.database = database.to_string();
        ds
    }

    /// Capture after applying a console's physical database override.
    pub fn identity(&self) -> SourceIdentity {
        SourceIdentity {
            driver: self.driver,
            host: self.host.clone(),
            port: self.port,
            user: self.user.clone(),
            database: self.database.clone(),
            file: self.file.clone(),
            use_tls: self.use_tls,
            tls: self.tls.clone(),
            read_only: self.read_only,
            ssh: self.uses_ssh().then(|| SshIdentity {
                host: self.ssh.host.clone(),
                port: self.ssh.port,
                user: self.ssh.user.clone(),
            }),
        }
    }

    /// Whether a database (PostgreSQL) or schema (MySQL) at the top of the tree is shown.
    pub fn shows_database(&self, name: &str, is_default: bool) -> bool {
        is_default || self.scope.all_databases || self.scope.databases.iter().any(|d| d == name)
    }

    pub fn shows_schema(&self, database: &str, schema: &str) -> bool {
        !self
            .scope
            .hidden_schemas
            .get(database)
            .is_some_and(|h| h.iter().any(|s| s == schema))
    }

    /// Whether connections go through an SSH tunnel.
    pub fn uses_ssh(&self) -> bool {
        self.ssh.enabled && !self.driver.is_file_based()
    }

    pub fn display_name(&self) -> String {
        if !self.name.is_empty() {
            return self.name.clone();
        }
        if self.driver.is_file_based() {
            return self
                .file
                .rsplit('/')
                .next()
                .unwrap_or(&self.file)
                .to_string();
        }
        format!("{}@{}", self.database, self.host)
    }

    pub fn url(&self) -> String {
        match self.driver {
            DriverKind::Postgres => {
                format!("postgresql://{}:{}/{}", self.host, self.port, self.database)
            }
            DriverKind::MySql => format!("mysql://{}:{}/{}", self.host, self.port, self.database),
            DriverKind::Sqlite => format!("sqlite:{}", self.file),
        }
    }

    /// Where the tunnel goes, e.g. "deploy@bastion:22"; empty without SSH.
    pub fn ssh_label(&self) -> String {
        if !self.uses_ssh() {
            return String::new();
        }
        format!("{}@{}:{}", self.ssh.user, self.ssh.host, self.ssh.port)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(#[serde(with = "float_text")] f64),
    /// Exact numeric kept as text to avoid rounding.
    Decimal(String),
    Text(String),
    Bytes(Vec<u8>),
}

/// Floats as text, so NaN and infinities survive a JSON round trip.
mod float_text {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&v.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// Decoded size of a row: the values and the text or bytes they own.
pub fn row_bytes(row: &[Value]) -> usize {
    row.iter()
        .map(|value| {
            std::mem::size_of::<Value>()
                + match value {
                    Value::Decimal(text) | Value::Text(text) => text.capacity(),
                    Value::Bytes(bytes) => bytes.capacity(),
                    _ => 0,
                }
        })
        .sum::<usize>()
        + std::mem::size_of::<Vec<Value>>()
}

impl Value {
    pub fn is_numeric(&self) -> bool {
        matches!(self, Value::Int(_) | Value::Float(_) | Value::Decimal(_))
    }

    /// Single-line representation used in the grid and in CSV/TSV copy.
    pub fn to_cell(&self) -> String {
        match self {
            Value::Null => "<null>".into(),
            Value::Bytes(b) => {
                let hex: String = b.iter().take(32).map(|x| format!("{x:02X}")).collect();
                if b.len() > 32 {
                    format!("0x{hex}…")
                } else {
                    format!("0x{hex}")
                }
            }
            Value::Text(s) => s.replace('\n', "↵"),
            other => other.to_string(),
        }
    }

    /// `to_cell` cut to at most `max_chars` characters before any copy is made,
    /// so drawing a multi-megabyte cell costs the same as drawing a short one.
    pub fn cell_preview(&self, max_chars: usize) -> String {
        match self {
            Value::Text(s) | Value::Decimal(s) => cell_preview(s, max_chars),
            Value::Null | Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::Bytes(_) => {
                cell_preview(&self.to_cell(), max_chars)
            }
        }
    }
}

/// Single-line prefix of `text` with at most `max_chars` characters; newlines
/// become "↵" and a cut is marked with "…".
pub fn cell_preview(text: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max_chars.saturating_mul(4)) + 3);
    let mut chars = text.chars();
    for c in chars.by_ref().take(max_chars) {
        out.push(if c == '\n' { '↵' } else { c });
    }
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "NULL"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Float(x) => write!(f, "{x}"),
            Value::Decimal(d) => write!(f, "{d}"),
            Value::Text(s) => write!(f, "{s}"),
            Value::Bytes(b) => write!(f, "<{} bytes>", b.len()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    pub type_name: String,
}

/// A table identity reported by the result protocol, not inferred from SQL.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum OriginTable {
    Postgres(u32),
    Named { schema: String, table: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OriginColumn {
    Postgres(i16),
    Named(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnOrigin {
    pub table: OriginTable,
    pub column: OriginColumn,
}

/// Session-local provenance is deliberately not restored from saved results.
#[derive(Clone, Debug)]
pub struct ResultSource {
    pub session_epoch: u64,
    pub columns: Vec<Option<ColumnOrigin>>,
    pub exact_columns: Vec<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhysicalTable {
    pub schema: String,
    pub name: String,
    pub oid: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhysicalColumn {
    pub name: String,
    pub ordinal: u32,
    pub nullable: bool,
    pub generated: bool,
    pub update_default: bool,
    pub comparable: bool,
}

/// Identity used for a SQLite rowid table without a usable primary key.
pub const SQLITE_ROWID: &str = "rowid";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableEditMetadata {
    pub table: PhysicalTable,
    pub columns: Vec<PhysicalColumn>,
    pub primary_key: Vec<String>,
    pub transactional: bool,
}

/// Validated mapping from output positions to a single table's physical names.
#[derive(Clone, Debug)]
pub struct ResultEditMetadata {
    pub table: PhysicalTable,
    pub physical_columns: Vec<Option<String>>,
    pub writable: Vec<bool>,
    /// The old value can be compared in SQL, so a concurrent change to it is
    /// detected. Writable columns without equality (json, arrays, geometry)
    /// are identified by the primary key only.
    pub comparable: Vec<bool>,
    pub update_default: Vec<bool>,
    pub primary_key: Vec<usize>,
    pub session_epoch: u64,
}

/// SQL contains only generated identifiers and placeholders; values stay bound.
#[derive(Clone, Debug)]
pub struct BoundUpdate {
    pub sql: String,
    pub params: Vec<Value>,
    pub source_row: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RelatedDirection {
    Referenced,
    Referencing,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignKeyColumn {
    pub source_name: String,
    pub source_origin: OriginColumn,
    pub source_type: String,
    pub target_name: String,
    pub target_origin: OriginColumn,
    pub target_type: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignKey {
    pub name: String,
    /// Catalog identity is scoped by the source table, not globally by name.
    pub catalog_id: String,
    pub source: PhysicalTable,
    pub target: PhysicalTable,
    pub columns: Vec<ForeignKeyColumn>,
}

#[derive(Clone, Debug)]
pub struct RelatedMetadata {
    pub table: PhysicalTable,
    pub keys: Vec<ForeignKey>,
}

/// A saved query shape, never arbitrary SQL or persisted edit authority.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CapturedRelatedQuery {
    pub relation: ForeignKey,
    pub direction: RelatedDirection,
    pub values: Vec<Value>,
    pub source_identity: SourceIdentity,
    pub context: ResolvedContext,
}

#[derive(Clone, Debug)]
pub struct BoundQuery {
    pub sql: String,
    pub params: Vec<Value>,
}

#[derive(Clone, Debug)]
pub struct MutationReport {
    pub changed_rows: usize,
}

#[derive(Clone, Debug)]
pub struct MutationFailure {
    pub message: String,
    pub source_row: Option<usize>,
    /// A connection failure during commit must never trigger a blind retry.
    pub outcome_unknown: bool,
}

impl MutationFailure {
    /// Nothing of the batch was committed; the reason is all there is to show.
    pub fn stopped(message: &str) -> Self {
        Self {
            message: message.into(),
            source_row: None,
            outcome_unknown: false,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct QueryResult {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Value>>,
    /// Rows changed by DML; `None` for statements returning a result set.
    pub affected: Option<u64>,
    /// More rows were available than `max_rows`.
    pub truncated: bool,
    pub elapsed: Duration,
    /// Server namespace immediately before execution, for safe replay/export.
    /// This is not a snapshot of transaction data or arbitrary session settings.
    #[serde(default)]
    pub context: Option<ResolvedContext>,
    #[serde(skip)]
    pub source: Option<ResultSource>,
}

impl QueryResult {
    pub fn has_result_set(&self) -> bool {
        !self.columns.is_empty()
    }
}

/// Window of rows to fetch: drivers skip `skip` rows and keep at most `limit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page {
    pub skip: usize,
    pub limit: usize,
}

/// Decoded bytes a paged read keeps (see [`Page::is_full`]). Each driver
/// decodes a whole row before it is counted, so one row can exceed it.
pub const PAGE_BYTES: usize = 64 << 20;

impl Page {
    pub fn first(limit: usize) -> Self {
        Self { skip: 0, limit }
    }

    /// Whether a page holding `rows` rows of `bytes` (see [`row_bytes`]) takes
    /// no more rows. Besides the row limit a paged read stops at [`PAGE_BYTES`],
    /// keeping at least one row; the next row then marks the result truncated
    /// and the next page starts after the rows kept. [`Page::ALL`] reads,
    /// which internal callers rely on to be complete, have no byte limit.
    pub fn is_full(&self, rows: usize, bytes: usize) -> bool {
        rows >= self.limit || (self.limit != usize::MAX && rows > 0 && bytes >= PAGE_BYTES)
    }

    pub const ALL: Page = Page {
        skip: 0,
        limit: usize::MAX,
    };
}

/// Table and its columns, used for code completion.
#[derive(Clone, Debug, PartialEq)]
pub struct TableMeta {
    pub schema: String,
    pub name: String,
    pub is_view: bool,
    /// Column name and type.
    pub columns: Vec<(String, String)>,
}

/// A console's configured default namespace; Default preserves server defaults.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SchemaSelection {
    #[default]
    Default,
    Named(String),
}

/// Persisted console configuration. Database selects a physical PostgreSQL DB;
/// schema selects a PostgreSQL schema or a MySQL database on the same session.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionContext {
    #[serde(default)]
    pub database: Option<String>,
    #[serde(default)]
    pub schema: SchemaSelection,
}

/// Actual namespace before a statement. PostgreSQL retains both the raw path
/// (needed to restore `$user` and quoting) and the resolved ordered schema list.
/// SQLite reports main; attached databases and temporary state remain local.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedContext {
    pub database: Option<String>,
    pub search_path: Option<String>,
    pub schemas: Vec<String>,
    /// PostgreSQL's implicit temporary namespace cannot be recreated on a new
    /// connection. Keep it separate from the ordinary completion search path.
    #[serde(default)]
    pub temporary_schema: Option<String>,
}

/// Metadata belongs to a physical PostgreSQL database, not merely a source ID.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MetadataKey {
    pub ds_id: String,
    pub database: Option<String>,
}

impl MetadataKey {
    pub fn for_source(ds: &DataSource) -> Self {
        Self {
            ds_id: ds.id.clone(),
            database: (ds.driver == DriverKind::Postgres).then(|| ds.database.clone()),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct CompletionMetadata {
    pub tables: Vec<TableMeta>,
    /// Includes empty schemas, which cannot be derived from the table list.
    pub schemas: Vec<String>,
    /// Defaults of this introspection connection, not the console's live state.
    pub default_context: ResolvedContext,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ObjectKind {
    Database,
    Schema,
    Table,
    View,
    Routine,
    Sequence,
    Column,
    Index,
    Key,
    Folder,
}

/// Node in the Database Explorer tree, loaded lazily level by level.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DbObject {
    pub kind: ObjectKind,
    pub name: String,
    /// Grey hint shown next to the name (column type, row estimate, ...).
    pub detail: String,
    /// Path of names from the data source root, used to load children.
    pub path: Vec<String>,
    pub children: Option<Vec<DbObject>>,
    pub expandable: bool,
}

impl DbObject {
    pub fn new(kind: ObjectKind, name: impl Into<String>, parent: &[String]) -> Self {
        let name = name.into();
        let mut path = parent.to_vec();
        path.push(name.clone());
        let expandable = !matches!(
            kind,
            ObjectKind::Column
                | ObjectKind::Index
                | ObjectKind::Key
                | ObjectKind::Routine
                | ObjectKind::Sequence
        );
        Self {
            kind,
            name,
            detail: String::new(),
            path,
            children: None,
            expandable,
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    pub fn folder(name: &str, parent: &[String], children: Vec<DbObject>) -> Self {
        let mut f = DbObject::new(ObjectKind::Folder, name, parent);
        // Prefix keeps folder paths distinct from objects with the same name.
        *f.path.last_mut().unwrap() = format!("\0{name}");
        f.detail = children.len().to_string();
        f.expandable = !children.is_empty();
        f.children = Some(children);
        f
    }

    /// Folders are not part of their children's paths, so this is a full
    /// depth-first search rather than a prefix walk.
    pub fn find_mut(&mut self, path: &[String]) -> Option<&mut DbObject> {
        if self.path == path {
            return Some(self);
        }
        self.children
            .as_mut()?
            .iter_mut()
            .find_map(|c| c.find_mut(path))
    }
}

#[cfg(test)]
mod identity_tests {
    use super::*;

    #[test]
    fn connection_target_and_mode_changes_change_identity() {
        let original = DataSource::new(DriverKind::Postgres);
        let mutations: [fn(&mut DataSource); 10] = [
            |ds| ds.driver = DriverKind::MySql,
            |ds| ds.host = "another-host".into(),
            |ds| ds.port = 55432,
            |ds| ds.user = "another-user".into(),
            |ds| ds.database = "another-database".into(),
            |ds| ds.file = "another-file.db".into(),
            |ds| ds.use_tls = true,
            |ds| ds.tls.ca_file = "ca.pem".into(),
            |ds| ds.tls.identity_file = "client.p12".into(),
            |ds| ds.read_only = true,
        ];
        for mutate in mutations {
            let mut changed = original.clone();
            mutate(&mut changed);
            assert_ne!(original.identity(), changed.identity());
        }
        assert_ne!(
            original.identity(),
            original.with_database("other").identity()
        );
    }

    #[test]
    fn old_source_identity_without_tls_details_still_restores() {
        let source = DataSource::new(DriverKind::Postgres);
        let mut value = serde_json::to_value(source.identity()).unwrap();
        value.as_object_mut().unwrap().remove("tls");
        assert_eq!(serde_json::from_value::<SourceIdentity>(value).unwrap(), source.identity());
    }

    #[test]
    fn credentials_and_display_settings_are_not_captured() {
        let mut original = DataSource::new(DriverKind::Postgres);
        original.ssh.enabled = true;
        let expected = original.identity();
        let mut changed = original;
        changed.id = "different-config-id".into();
        changed.name = "renamed".into();
        changed.scope.all_databases = true;
        changed.password = "test-only-db-password".into();
        changed.tls_identity_password = "test-only-tls-password".into();
        changed.ssh.auth = SshAuth::KeyPair;
        changed.ssh.password = "test-only-ssh-password".into();
        changed.ssh.key_file = "test-only-key-path".into();
        changed.ssh.passphrase = "test-only-key-passphrase".into();
        assert_eq!(expected, changed.identity());

        let encoded = serde_json::to_string(&changed.identity()).unwrap();
        assert!(!encoded.contains("test-only"));
        assert_eq!(
            serde_json::from_str::<SourceIdentity>(&encoded).unwrap(),
            expected
        );
    }

    #[test]
    fn only_effective_ssh_peer_changes_identity() {
        let mut source = DataSource::new(DriverKind::Postgres);
        let direct = source.identity();
        source.ssh.host = "bastion".into();
        source.ssh.user = "operator".into();
        source.ssh.port = 2222;
        assert_eq!(source.identity(), direct);
        source.ssh.enabled = true;
        let tunneled = source.identity();
        assert_ne!(tunneled, direct);
        assert_eq!(tunneled.ssh.as_ref().unwrap().host, "bastion");
        for mutate in [
            (|ds: &mut DataSource| ds.ssh.host = "other".into()) as fn(&mut DataSource),
            |ds| ds.ssh.user = "other".into(),
            |ds| ds.ssh.port = 2223,
        ] {
            let mut changed = source.clone();
            mutate(&mut changed);
            assert_ne!(tunneled, changed.identity());
        }
        source.driver = DriverKind::Sqlite;
        assert!(source.identity().ssh.is_none());
    }
}

#[cfg(test)]
mod preview_tests {
    use super::*;

    #[test]
    fn cell_preview_cuts_on_char_boundaries_before_copying() {
        assert_eq!(cell_preview("a\nb", 10), "a↵b");
        assert_eq!(cell_preview("Łódź\nżółw", 6), "Łódź↵ż…");
        assert_eq!(cell_preview("😀😀😀", 2), "😀😀…");
        assert_eq!(cell_preview("", 4), "");
        let decimal = Value::Decimal(format!("1.{}", "0".repeat(5000)));
        assert_eq!(decimal.cell_preview(8), "1.000000…");
        assert_eq!(Value::Int(42).cell_preview(8), "42");
        let bytes = Value::Bytes(vec![0xAB; 100]);
        assert_eq!(bytes.cell_preview(1024), bytes.to_cell());
    }

    #[test]
    fn cell_preview_size_does_not_grow_with_the_value() {
        let small = Value::Text("x\n".repeat(512)).cell_preview(1024);
        let large = Value::Text("x\n".repeat(25 << 20)).cell_preview(1024);
        assert_eq!(small.chars().count(), 1024);
        assert_eq!(large.chars().count(), 1025);
        assert!(large.capacity() <= 1024 * 4 + 3);
    }

    #[test]
    #[ignore = "benchmark: cargo test --release --lib cell_preview_cost -- --ignored --nocapture"]
    fn cell_preview_cost_benchmark() {
        for mib in [1usize, 10, 50] {
            let value = Value::Text("line of text\n".repeat((mib << 20) / 13));
            let start = std::time::Instant::now();
            let full = value.to_cell();
            let before = start.elapsed();
            let start = std::time::Instant::now();
            let preview = value.cell_preview(1024);
            let after = start.elapsed();
            println!("{mib} MiB cell: full copy {before:?} ({} B), preview {after:?} ({} B)", full.len(), preview.len());
        }
    }
}
