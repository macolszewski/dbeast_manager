use super::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;

const MAX_CLIENTS: usize = 16;
const MAX_REQUESTS: usize = 64;
const MAX_KEYS: usize = 256;
const IDLE_TTL: Duration = Duration::from_secs(20 * 60);
const SESSION_TTL: Duration = Duration::from_secs(2 * 60 * 60);
const REQUEST_TTL: Duration = Duration::from_secs(5 * 60);

#[derive(Clone)]
pub struct McpController {
    inner: Arc<Mutex<State>>,
    runtime: tokio::runtime::Handle,
    notify: Arc<dyn Fn() + Send + Sync>,
    notification_pending: Arc<AtomicBool>,
    active_operations: Arc<tokio::sync::Semaphore>,
}
#[derive(Clone)]
pub(super) struct Principal {
    pub client_id: String,
    generation: String,
}
struct Client {
    label: String,
    token_hash: [u8; 32],
    http_budget: Budget,
    session_budget: Budget,
}
struct Budget {
    since: Instant,
    count: usize,
}
impl Budget {
    fn new() -> Self {
        Self {
            since: Instant::now(),
            count: 0,
        }
    }
    fn take(&mut self, limit: usize, window: Duration) -> bool {
        if self.since.elapsed() >= window {
            self.since = Instant::now();
            self.count = 0;
        }
        if self.count >= limit {
            return false;
        }
        self.count += 1;
        true
    }
}
struct Session {
    client_id: String,
    token_hash: [u8; 32],
    label: String,
    created: Instant,
    touched: Instant,
    grants: Vec<Grant>,
    // Result eviction must never make an old write eligible for execution.
    keys: HashMap<String, ([u8; 32], String)>,
    active_operation: Arc<tokio::sync::Semaphore>,
}

struct ExecutionPermits {
    _global: tokio::sync::OwnedSemaphorePermit,
    _session: tokio::sync::OwnedSemaphorePermit,
}
#[derive(Clone)]
struct Grant {
    id: String,
    source_id: String,
    scope: GrantScope,
}
#[derive(Clone)]
enum GrantScope {
    ReadSource,
    ReadTable { table: TableRef, identity: String },
    FullSql,
}
struct Request {
    session_id: String,
    operation: Operation,
    alias: String,
    preview: String,
    created: Instant,
    finished: Option<Instant>,
    status: &'static str,
    result: Option<Value>,
    cancellation: CancellationToken,
    source: Option<DataSource>,
    file: Option<SharedFile>,
    grant_on_success: Approval,
    expected_table_identity: Option<String>,
    eligible_table: Option<TableRef>,
    authorizing_grant_id: Option<String>,
    /// Single target of a Full SQL write, so standing table access can be checked again.
    write_target: Option<TableRef>,
    /// The SQL was regenerated for a table write share; only then may that share alone authorize it.
    regenerated: bool,
    /// SQL shown in the activity log; document text is never copied there.
    audit_sql: Option<Arc<str>>,
}
struct State {
    snapshot: Arc<UiSnapshot>,
    generation: String,
    running: bool,
    starting: bool,
    port: u16,
    error: Option<String>,
    server_cancel: CancellationToken,
    clients: HashMap<String, Client>,
    sessions: HashMap<String, Session>,
    sources: HashMap<String, SharedSource>,
    files: HashMap<String, SharedFile>,
    requests: HashMap<String, Request>,
    audit: VecDeque<AuditView>,
    http_budget: Budget,
}
#[derive(Debug)]
pub(super) struct ApiError {
    pub code: &'static str,
    pub message: &'static str,
}
impl ApiError {
    fn new(code: &'static str, message: &'static str) -> Self {
        Self { code, message }
    }
}
fn public_execution_message(error: &ExecutionError) -> &'static str {
    match error.code {
        "invalid_input"
        | "unsupported_safe_read"
        | "resource_unavailable"
        | "permission_denied"
        | "limit_exceeded"
        | "cancelled"
        | "outcome_unknown" => error.message,
        _ => "The database operation failed. Review the connection in DBeast.",
    }
}
fn hash(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}
fn decode_hash(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}
fn secret() -> String {
    rand::random::<[u8; 32]>()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
fn valid_label(label: &str) -> bool {
    !label.trim().is_empty() && label.len() <= 128 && !label.chars().any(char::is_control)
}
fn cancel(request: &mut Request, status: &'static str) {
    request.cancellation.cancel();
    request.status = if request.operation.is_full_sql() {
        match request.status {
            "running" | "outcome_unknown" => "outcome_unknown",
            "completed" | "completed_result_revoked" => "completed_result_revoked",
            _ => status,
        }
    } else {
        status
    };
    request.result = None;
    // A running read keeps its own copy of the document until it returns.
    request.file = None;
    // Revoking an already finished request must not extend its retention.
    request.finished.get_or_insert_with(Instant::now);
}
impl State {
    /// Whether `request` may still start or release its result. Automatic
    /// authorizations must be backed by a session grant or standing access that
    /// is valid now, not only when the request was submitted.
    fn authorized_now(&self, request: &Request) -> bool {
        let Some(session) = self.sessions.get(&request.session_id) else {
            return false;
        };
        let source_id = match &request.operation {
            Operation::ReadFile { file_id, .. } => return self.files.contains_key(file_id),
            op => op.source_id().unwrap_or_default(),
        };
        let Some(shared) = self.sources.get(source_id) else {
            return false;
        };
        let Some(access) = shared.access_for(&session.client_id) else {
            return false;
        };
        if request.authorizing_grant_id.is_none() {
            // The user approved this exact request; narrowing sharing cancels it.
            return true;
        }
        matching_grant(session, &request.operation, request.eligible_table.as_ref()).is_some()
            || shared_access_grant(
                source_id,
                &shared.source,
                &access,
                &request.operation,
                request.eligible_table.as_ref(),
                request.write_target.as_ref(),
            )
            .is_some_and(|(_, table_write_only)| !table_write_only || request.regenerated)
    }
    /// Revokes a completed result whose authorization is gone, so it is never
    /// handed over. Returns whether anything changed.
    fn withhold_unauthorized(&mut self, id: &str) -> bool {
        let Some(request) = self.requests.get(id) else {
            return false;
        };
        if request.status != "completed" || self.authorized_now(request) {
            return false;
        }
        cancel(self.requests.get_mut(id).unwrap(), "revoked");
        self.record(id, "result", "revoked: access changed");
        true
    }
    fn cancel_session(&mut self, id: &str) {
        self.sessions.remove(id);
        for request in self.requests.values_mut().filter(|r| r.session_id == id) {
            cancel(request, "revoked");
        }
        self.requests.retain(|_, r| r.session_id != id);
    }
    /// Returns whether anything ended, which is when the UI needs a new snapshot.
    fn expire(&mut self) -> bool {
        let now = Instant::now();
        let before = (
            self.sessions.len(),
            self.requests.len(),
            self.requests.values().filter(|r| r.status == "pending").count(),
        );
        let expired: Vec<_> = self
            .sessions
            .iter()
            .filter(|(_, s)| {
                now.duration_since(s.created) >= SESSION_TTL
                    || now.duration_since(s.touched) >= IDLE_TTL
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            self.cancel_session(&id);
        }
        for r in self.requests.values_mut() {
            if r.status == "pending" && now.duration_since(r.created) >= REQUEST_TTL {
                cancel(r, "expired");
            }
        }
        self.requests.retain(|_, r| {
            r.finished
                .is_none_or(|at| now.duration_since(at) < REQUEST_TTL)
        });
        before
            != (
                self.sessions.len(),
                self.requests.len(),
                self.requests.values().filter(|r| r.status == "pending").count(),
            )
    }
    fn validate_principal(&self, p: &Principal) -> Result<(), ApiError> {
        if !self.running
            || self.generation != p.generation
            || !self.clients.contains_key(&p.client_id)
        {
            return Err(ApiError::new(
                "permission_denied",
                "Agent connection is unavailable.",
            ));
        }
        Ok(())
    }
    fn session_id(&mut self, p: &Principal, token: &str) -> Result<String, ApiError> {
        self.expire();
        self.validate_principal(p)?;
        let token_hash = hash(token.as_bytes());
        let (id, session) = self
            .sessions
            .iter_mut()
            .find(|(_, s)| {
                s.client_id == p.client_id && bool::from(s.token_hash.ct_eq(&token_hash))
            })
            .ok_or_else(|| {
                ApiError::new(
                    "session_expired",
                    "Session is unavailable. Begin a new session.",
                )
            })?;
        session.touched = Instant::now();
        Ok(id.clone())
    }
    fn record(&mut self, request_id: &str, action: &str, status: &str) {
        let Some(request) = self.requests.get(request_id) else {
            return;
        };
        self.audit.push_front(AuditView {
            at: chrono::Local::now().format("%H:%M:%S").to_string(),
            session_id: request.session_id.clone(),
            request_id: request_id.into(),
            alias: request.alias.clone(),
            action: action.into(),
            status: status.into(),
            sql: request.audit_sql.clone(),
            local: local_target(request),
        });
        self.audit.truncate(100);
    }
    fn rebuild_snapshot(&mut self) {
        let now = Instant::now();
        let mut clients: Vec<_> = self
            .clients
            .iter()
            .map(|(id, c)| ClientView {
                id: id.clone(),
                label: c.label.clone(),
            })
            .collect();
        clients.sort_by(|a, b| a.id.cmp(&b.id));
        let mut sessions: Vec<_> = self
            .sessions
            .iter()
            .map(|(id, s)| SessionView {
                id: id.clone(),
                client_id: s.client_id.clone(),
                label: s.label.clone(),
                expires_at: now
                    + SESSION_TTL
                        .saturating_sub(now.duration_since(s.created))
                        .min(IDLE_TTL.saturating_sub(now.duration_since(s.touched))),
                grants: s
                    .grants
                    .iter()
                    .map(|g| GrantView {
                        id: g.id.clone(),
                        alias: self
                            .sources
                            .get(&g.source_id)
                            .map(|s| s.alias.clone())
                            .unwrap_or_default(),
                        scope: match &g.scope {
                            GrantScope::ReadSource => "Read-only source".into(),
                            GrantScope::ReadTable { table, .. } => {
                                format!("Read-only table {}.{}", table.schema, table.name)
                            }
                            GrantScope::FullSql => "Full SQL (connection privileges)".into(),
                        },
                    })
                    .collect(),
            })
            .collect();
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        let mut requests: Vec<_> = self
            .requests
            .iter()
            .filter(|(_, r)| matches!(r.status, "pending" | "running"))
            .map(|(id, r)| RequestView {
                id: id.clone(),
                session_id: r.session_id.clone(),
                client_label: self
                    .sessions
                    .get(&r.session_id)
                    .map(|s| s.label.clone())
                    .unwrap_or_default(),
                alias: r.alias.clone(),
                operation: r.operation.clone(),
                eligible_table: r.eligible_table.clone(),
                preview: r.preview.clone(),
                status: r.status.into(),
                can_allow_full_sql: r.operation.is_full_sql()
                    && r.source.as_ref().is_some_and(|s| !s.read_only),
                local: local_target(r),
            })
            .collect();
        requests.sort_by(|a, b| a.id.cmp(&b.id));
        self.snapshot = Arc::new(UiSnapshot {
            running: self.running,
            starting: self.starting,
            endpoint: self
                .running
                .then(|| format!("http://127.0.0.1:{}/mcp", self.port)),
            error: self.error.clone(),
            clients,
            sessions,
            requests,
            audit: self.audit.iter().cloned().collect(),
        });
    }
}
impl McpController {
    pub fn new(runtime: tokio::runtime::Handle, notify: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            inner: Arc::new(Mutex::new(State {
                snapshot: Arc::new(UiSnapshot::default()),
                generation: id(),
                running: false,
                starting: false,
                port: DEFAULT_PORT,
                error: None,
                server_cancel: CancellationToken::new(),
                clients: HashMap::new(),
                sessions: HashMap::new(),
                sources: HashMap::new(),
                files: HashMap::new(),
                requests: HashMap::new(),
                audit: VecDeque::new(),
                http_budget: Budget::new(),
            })),
            runtime,
            notify: Arc::new(notify),
            notification_pending: Arc::new(AtomicBool::new(false)),
            active_operations: Arc::new(tokio::sync::Semaphore::new(4)),
        }
    }
    fn publish(&self, state: &mut State) {
        state.rebuild_snapshot();
        if !self.notification_pending.swap(true, Ordering::AcqRel) {
            (self.notify)();
        }
    }
    pub fn snapshot(&self) -> Arc<UiSnapshot> {
        let state = self.inner.lock().unwrap();
        self.notification_pending.store(false, Ordering::Release);
        state.snapshot.clone()
    }
    pub fn start(&self, port: u16) {
        let mut s = self.inner.lock().unwrap();
        if s.running || s.starting {
            return;
        }
        if port == 0 {
            s.error = Some("Choose a port between 1 and 65535.".into());
            self.publish(&mut s);
            return;
        }
        s.starting = true;
        s.error = None;
        s.port = port;
        s.generation = id();
        s.server_cancel = CancellationToken::new();
        let generation = s.generation.clone();
        let cancellation = s.server_cancel.clone();
        self.publish(&mut s);
        let controller = self.clone();
        self.runtime.spawn(async move {
            super::server::run(controller, port, generation, cancellation).await;
        });
    }
    pub fn stop(&self) {
        let mut s = self.inner.lock().unwrap();
        s.server_cancel.cancel();
        for r in s.requests.values_mut() {
            cancel(r, "revoked");
        }
        // Connections are remembered; the new generation invalidates every principal.
        s.sessions.clear();
        s.requests.clear();
        s.audit.clear();
        s.generation = id();
        s.running = false;
        s.starting = false;
        s.error = None;
        self.publish(&mut s);
    }
    pub(super) fn server_started(&self, generation: &str) -> bool {
        let mut s = self.inner.lock().unwrap();
        if s.generation != generation || s.server_cancel.is_cancelled() {
            return false;
        }
        s.running = true;
        s.starting = false;
        self.publish(&mut s);
        true
    }
    pub(super) fn server_ended(&self, generation: &str, error: Option<String>) {
        let mut s = self.inner.lock().unwrap();
        if s.generation != generation {
            return;
        }
        s.server_cancel.cancel();
        for r in s.requests.values_mut() {
            cancel(r, "revoked");
        }
        s.sessions.clear();
        s.requests.clear();
        s.running = false;
        s.starting = false;
        s.error = error;
        self.publish(&mut s);
    }
    pub(super) fn tick(&self) {
        let mut s = self.inner.lock().unwrap();
        if s.expire() {
            self.publish(&mut s);
        }
    }
    /// The configuration names `port`; the connection works once the server listens there.
    pub fn create_client(&self, label: String, port: u16) -> Result<ClientConnection, String> {
        let mut s = self.inner.lock().unwrap();
        if s.clients.len() >= MAX_CLIENTS {
            return Err("The limit of 16 agent connections has been reached.".into());
        }
        if !valid_label(&label) {
            return Err(
                "Use an agent label of 1 to 128 characters without control characters.".into(),
            );
        }
        let token = secret();
        let client_id = id();
        s.clients.insert(
            client_id.clone(),
            Client {
                label,
                token_hash: hash(token.as_bytes()),
                http_budget: Budget::new(),
                session_budget: Budget::new(),
            },
        );
        let configuration=serde_json::to_string_pretty(&json!({"mcpServers":{"dbeast":{
            "url":format!("http://127.0.0.1:{port}/mcp"),"headers":{"Authorization":format!("Bearer {token}")}
        }}})).expect("configuration serialization");
        self.publish(&mut s);
        Ok(ClientConnection {
            id: client_id,
            token,
            configuration,
        })
    }
    /// Connections to remember across restarts. Tokens themselves are never kept.
    pub fn stored_clients(&self) -> Vec<StoredClient> {
        let s = self.inner.lock().unwrap();
        let mut clients: Vec<_> = s
            .clients
            .iter()
            .map(|(id, client)| StoredClient {
                id: id.clone(),
                label: client.label.clone(),
                token_hash: client
                    .token_hash
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
            })
            .collect();
        clients.sort_by(|a, b| a.id.cmp(&b.id));
        clients
    }
    /// Restores remembered connections; invalid records are skipped.
    pub fn restore_clients(&self, clients: Vec<StoredClient>) {
        let mut s = self.inner.lock().unwrap();
        for client in clients {
            if s.clients.len() >= MAX_CLIENTS
                || !valid_label(&client.label)
                || uuid::Uuid::parse_str(&client.id).is_err()
                || s.clients.contains_key(&client.id)
            {
                continue;
            }
            let Some(token_hash) = decode_hash(&client.token_hash) else {
                continue;
            };
            s.clients.insert(
                client.id,
                Client {
                    label: client.label,
                    token_hash,
                    http_budget: Budget::new(),
                    session_budget: Budget::new(),
                },
            );
        }
        self.publish(&mut s);
    }
    pub fn revoke_client(&self, client_id: &str) {
        let mut s = self.inner.lock().unwrap();
        s.clients.remove(client_id);
        let sessions: Vec<_> = s
            .sessions
            .iter()
            .filter(|(_, s)| s.client_id == client_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in sessions {
            s.cancel_session(&id);
        }
        self.publish(&mut s);
    }
    pub fn end_session(&self, session_id: &str) {
        let mut s = self.inner.lock().unwrap();
        s.cancel_session(session_id);
        self.publish(&mut s);
    }
    pub fn revoke_grant(&self, session_id: &str, grant_id: &str) {
        let mut s = self.inner.lock().unwrap();
        if let Some(session) = s.sessions.get_mut(session_id) {
            session.grants.retain(|g| g.id != grant_id);
        }
        for r in s
            .requests
            .values_mut()
            .filter(|r| r.session_id == session_id)
        {
            cancel(r, "revoked");
        }
        self.publish(&mut s);
    }
    pub fn update_resources(&self, sources: Vec<SharedSource>, files: Vec<SharedFile>) {
        let mut s = self.inner.lock().unwrap();
        let mut next_sources = HashMap::new();
        for source in sources
            .into_iter()
            .take(128)
            .filter(|s| valid_label(&s.alias))
        {
            let old = s
                .sources
                .iter()
                .find(|(_, old)| old.alias == source.alias && old.source == source.source);
            next_sources.insert(old.map(|(id, _)| id.clone()).unwrap_or_else(id), source);
        }
        let mut next_files = HashMap::new();
        let mut shared_bytes = 0usize;
        let mut omitted = false;
        for file in files.into_iter().take(128) {
            if !valid_label(&file.alias)
                || file.text.len() > MAX_SHARED_FILE_BYTES
                || shared_bytes.saturating_add(file.text.len()) > MAX_SHARED_TEXT_BYTES
            {
                omitted = true;
                continue;
            }
            shared_bytes += file.text.len();
            let old = s.files.iter().find(|(_, old)| {
                old.id == file.id
                    && old.alias == file.alias
                    && old.version == file.version
                    && old.text == file.text
            });
            next_files.insert(old.map(|(id, _)| id.clone()).unwrap_or_else(id), file);
        }
        let visible = |client_id: &str, source_id: &str| {
            next_sources
                .get(source_id)
                .is_some_and(|shared: &SharedSource| shared.access_for(client_id).is_some())
        };
        // Any narrowing for a connection ends its session grants and requests on
        // that source; only widening keeps them.
        let mut narrowed_pairs = Vec::new();
        for (session_id, session) in &s.sessions {
            for (source_id, old) in &s.sources {
                if narrowed(old, next_sources.get(source_id), &session.client_id) {
                    narrowed_pairs.push((session_id.clone(), source_id.clone()));
                }
            }
        }
        for (session_id, source_id) in &narrowed_pairs {
            if let Some(session) = s.sessions.get_mut(session_id) {
                session.grants.retain(|g| &g.source_id != source_id);
            }
        }
        let mut narrowed_requests = Vec::new();
        for (request_id, r) in s.requests.iter_mut() {
            if matches!(r.status, "pending" | "running" | "completed")
                && narrowed_pairs.iter().any(|(session_id, source_id)| {
                    &r.session_id == session_id
                        && r.operation.source_id() == Some(source_id.as_str())
                })
            {
                cancel(r, "revoked");
                narrowed_requests.push(request_id.clone());
            }
        }
        for request_id in narrowed_requests {
            s.record(&request_id, "sharing", "revoked: access narrowed");
        }
        for session in s.sessions.values_mut() {
            let client_id = session.client_id.clone();
            session
                .grants
                .retain(|g| visible(&client_id, &g.source_id));
        }
        let owners: HashMap<String, String> = s
            .sessions
            .iter()
            .map(|(id, session)| (id.clone(), session.client_id.clone()))
            .collect();
        // Finished requests without a result have nothing left to revoke.
        for r in s
            .requests
            .values_mut()
            .filter(|r| r.finished.is_none() || r.result.is_some())
        {
            let valid = match &r.operation {
                Operation::ReadFile { file_id, .. } => next_files.contains_key(file_id),
                op => op.source_id().is_some_and(|id| {
                    owners
                        .get(&r.session_id)
                        .is_some_and(|client_id| visible(client_id, id))
                }),
            };
            if !valid {
                cancel(r, "resource_unavailable");
            }
        }
        s.sources = next_sources;
        s.files = next_files;
        if omitted {
            s.error=Some("Some documents were not shared: use valid aliases, at most 4 MiB per document and 16 MiB in total.".into());
        }
        self.publish(&mut s);
    }
    pub(super) fn authenticate(&self, token: &str) -> Option<Principal> {
        if token.len() != 64 {
            return None;
        }
        let s = self.inner.lock().unwrap();
        if !s.running {
            return None;
        }
        let h = hash(token.as_bytes());
        s.clients
            .iter()
            .find(|(_, c)| bool::from(c.token_hash.ct_eq(&h)))
            .map(|(id, _)| Principal {
                client_id: id.clone(),
                generation: s.generation.clone(),
            })
    }
    pub(super) fn allow_http(&self, p: &Principal) -> bool {
        let mut s = self.inner.lock().unwrap();
        if s.validate_principal(p).is_err() || !s.http_budget.take(60, Duration::from_secs(1)) {
            return false;
        }
        s.clients
            .get_mut(&p.client_id)
            .is_some_and(|c| c.http_budget.take(20, Duration::from_secs(1)))
    }
    pub(super) fn begin_session(
        &self,
        p: &Principal,
        label: Option<String>,
    ) -> Result<Value, ApiError> {
        let mut s = self.inner.lock().unwrap();
        s.validate_principal(p)?;
        let label = label.unwrap_or_else(|| s.clients[&p.client_id].label.clone());
        if !valid_label(&label) {
            return Err(ApiError::new(
                "invalid_input",
                "Use a short label without control characters.",
            ));
        }
        if !s
            .clients
            .get_mut(&p.client_id)
            .unwrap()
            .session_budget
            .take(3, Duration::from_secs(10))
        {
            return Err(ApiError::new(
                "limit_exceeded",
                "Too many new sessions. Wait ten seconds before trying again.",
            ));
        }
        let previous: Vec<_> = s
            .sessions
            .iter()
            .filter(|(_, s)| s.client_id == p.client_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in previous {
            s.cancel_session(&id);
        }
        let session_id = id();
        let token = secret();
        let now = Instant::now();
        s.sessions.insert(
            session_id.clone(),
            Session {
                client_id: p.client_id.clone(),
                token_hash: hash(token.as_bytes()),
                label,
                created: now,
                touched: now,
                grants: Vec::new(),
                keys: HashMap::new(),
                active_operation: Arc::new(tokio::sync::Semaphore::new(1)),
            },
        );
        self.publish(&mut s);
        Ok(
            json!({"session_id":session_id,"session_token":token,"idle_timeout_seconds":IDLE_TTL.as_secs(),"max_lifetime_seconds":SESSION_TTL.as_secs(),
            "limits":{"max_rows":MAX_ROWS,"max_sql_bytes":MAX_SQL_BYTES,"max_result_bytes":MAX_RESULT_BYTES,"max_response_bytes":MAX_RESPONSE_BYTES,
                "max_cell_bytes":MAX_CELL_BYTES,"request_keys":MAX_KEYS,"max_pending_requests":4,"max_running_requests":1,
                "approval_timeout_seconds":REQUEST_TTL.as_secs(),"execution_timeout_seconds":30,"poll_interval_ms":1000},"permissions":[]}),
        )
    }
    pub(super) fn catalog(&self, p: &Principal, token: &str) -> Result<Value, ApiError> {
        let mut s = self.inner.lock().unwrap();
        s.session_id(p, token)?;
        let sources:Vec<_>=s.sources.iter().filter_map(|(id,s)|{
            let shared=s.access_for(&p.client_id)?;
            let read_only=s.source.read_only;
            let access=match shared.source{SourceAccess::ReadWrite if read_only=>SourceAccess::Read,access=>access};
            let tables:Vec<_>=shared.tables.iter().map(|(table,access)|json!({"schema":table.schema,"name":table.name,
                "access":if read_only{"read"}else{access.public_name()}})).collect();
            Some(json!({"source_id":id,"alias":s.alias,"driver":format!("{:?}",s.source.driver),"full_sql_available":!read_only,
                "shared_access":access.public_name(),"shared_tables":tables}))
        }).collect();
        let files:Vec<_>=s.files.iter().map(|(id,f)|json!({"file_id":id,"alias":f.alias,"version":f.version,"kind":"open_text"})).collect();
        Ok(json!({"sources":sources,"files":files}))
    }
}

impl McpController {
    pub(super) fn submit(
        &self,
        p: &Principal,
        token: &str,
        key: &str,
        mut operation: Operation,
    ) -> Result<Value, ApiError> {
        if uuid::Uuid::parse_str(key).is_err() {
            return Err(ApiError::new(
                "invalid_input",
                "request_key must be a UUID unique to this operation.",
            ));
        }
        let digest = hash(&serde_json::to_vec(&operation).expect("operation serialization"));
        let mut s = self.inner.lock().unwrap();
        let session_id = s.session_id(p, token)?;
        if let Some((previous, request_id)) = s.sessions[&session_id].keys.get(key) {
            if previous != &digest {
                return Err(ApiError::new(
                    "request_conflict",
                    "The request key was already used with different parameters.",
                ));
            }
            let request_id = request_id.clone();
            return Ok(self.release(&mut s, &request_id));
        }
        if s.sessions[&session_id].keys.len() >= MAX_KEYS || s.requests.len() >= MAX_REQUESTS {
            return Err(ApiError::new(
                "limit_exceeded",
                "Request storage limit reached. Wait for results to expire or begin a new session.",
            ));
        }
        let pending = s
            .requests
            .values()
            .filter(|r| r.status == "pending")
            .count();
        let own = s
            .requests
            .values()
            .filter(|r| r.status == "pending" && r.session_id == session_id)
            .count();
        if pending >= 32 || own >= 4 {
            return Err(ApiError::new(
                "limit_exceeded",
                "Too many requests awaiting approval.",
            ));
        }
        let (source, file, alias) = match &operation {
            Operation::ReadFile {
                file_id,
                version,
                start,
                max_chars,
            } => {
                if *max_chars == 0 || *max_chars > 64 * 1024 || *start > MAX_SHARED_FILE_BYTES {
                    return Err(ApiError::new("invalid_input", "Invalid document range."));
                }
                let file = s
                    .files
                    .get(file_id)
                    .filter(|f| f.version == *version)
                    .ok_or_else(|| {
                        ApiError::new("resource_unavailable", "Shared resource is unavailable.")
                    })?
                    .clone();
                (None, Some(file.clone()), file.alias.clone())
            }
            op => {
                let source = s
                    .sources
                    .get(op.source_id().unwrap_or_default())
                    .filter(|shared| shared.access_for(&p.client_id).is_some())
                    .ok_or_else(|| {
                        ApiError::new("resource_unavailable", "Shared resource is unavailable.")
                    })?
                    .clone();
                if op.is_full_sql() && source.source.read_only {
                    return Err(ApiError::new(
                        "permission_denied",
                        "This source allows read-only operations only.",
                    ));
                }
                if let Operation::Sql { sql, .. } | Operation::ReadSql { sql, .. } = op
                    && (sql.is_empty() || sql.len() > MAX_SQL_BYTES)
                {
                    return Err(ApiError::new(
                        "invalid_input",
                        "SQL must contain 1 to 65536 bytes.",
                    ));
                }
                (Some(source.source), None, source.alias)
            }
        };
        // Parsing and snapshot slicing can be more expensive than a state lookup.
        // Do them outside the mutex also used by the application's controls.
        drop(s);
        let mut preview = if let Operation::ReadFile {
            start, max_chars, ..
        } = &operation
        {
            file.as_ref()
                .expect("document snapshot")
                .text
                .chars()
                .skip(*start)
                .take(*max_chars)
                .collect()
        } else {
            executor::preview(source.as_ref().expect("source snapshot"), &operation)
                .map_err(|error| ApiError::new(error.code, public_execution_message(&error)))?
        };
        // Only a write that provably touches one table can match a table write share.
        let write = match &operation {
            Operation::Sql { sql, .. } => {
                executor::table_write_target(source.as_ref().expect("SQL source"), sql).ok()
            }
            _ => None,
        };
        let mut audit_sql = (!matches!(operation, Operation::ReadFile { .. }))
            .then(|| Arc::<str>::from(preview.as_str()));
        let eligible_table = if let Operation::ReadSql { sql, .. } = &operation {
            let tables = executor::read_sql_tables(source.as_ref().expect("SQL source"), sql)
                .map_err(|_| {
                    ApiError::new(
                        "unsupported_safe_read",
                        "SQL contains unsupported read-only constructs.",
                    )
                })?;
            (tables.len() == 1).then(|| tables[0].clone())
        } else {
            operation.table().cloned()
        };
        let mut s = self.inner.lock().unwrap();
        if s.session_id(p, token)? != session_id {
            return Err(ApiError::new("session_expired", "Begin a new session."));
        }
        let resource_current = match &operation {
            Operation::ReadFile { file_id, .. } => s.files.contains_key(file_id),
            op => op.source_id().is_some_and(|id| {
                s.sources
                    .get(id)
                    .is_some_and(|shared| shared.access_for(&p.client_id).is_some())
            }),
        };
        if !resource_current {
            return Err(ApiError::new(
                "resource_unavailable",
                "Shared resource changed while preparing the request.",
            ));
        }
        if let Some((previous, request_id)) = s.sessions[&session_id].keys.get(key) {
            if previous != &digest {
                return Err(ApiError::new(
                    "request_conflict",
                    "The request key was already used with different parameters.",
                ));
            }
            let request_id = request_id.clone();
            return Ok(self.release(&mut s, &request_id));
        }
        if s.sessions[&session_id].keys.len() >= MAX_KEYS
            || s.requests.len() >= MAX_REQUESTS
            || s.requests
                .values()
                .filter(|r| r.status == "pending")
                .count()
                >= 32
            || s.requests
                .values()
                .filter(|r| r.status == "pending" && r.session_id == session_id)
                .count()
                >= 4
        {
            return Err(ApiError::new(
                "limit_exceeded",
                "Request capacity was reached while preparing this request.",
            ));
        }
        let grant = matching_grant(
            &s.sessions[&session_id],
            &operation,
            eligible_table.as_ref(),
        )
        .map(|grant| (grant, false))
        .or_else(|| {
            let id = operation.source_id()?;
            let shared = s.sources.get(id)?;
            shared_access_grant(
                id,
                &shared.source,
                &shared.access_for(&p.client_id)?,
                &operation,
                eligible_table.as_ref(),
                write.as_ref().map(|(table, _)| table),
            )
        });
        // A table write share runs the SQL regenerated from the checked AST, not
        // the submitted text, and the audit shows exactly that SQL.
        let mut regenerated = false;
        let grant = grant.map(|(grant, table_write_only)| {
            regenerated = table_write_only;
            if table_write_only
                && let (Some((_, normalized)), Operation::Sql { sql, .. }) =
                    (&write, &mut operation)
            {
                sql.clone_from(normalized);
                preview.clone_from(normalized);
                audit_sql = Some(Arc::from(normalized.as_str()));
            }
            grant
        });
        let permits = if grant.is_some() {
            Some(self.reserve_execution(&s, &session_id).map_err(|_| ApiError::new(
                "limit_exceeded",
                "Wait for the running operation and its cancellation cleanup before submitting another request.",
            ))?)
        } else {
            None
        };
        let expected = grant.as_ref().and_then(|g| match &g.scope {
            GrantScope::ReadTable { identity, .. } => Some(identity.clone()),
            _ => None,
        });
        let request_id = id();
        s.sessions
            .get_mut(&session_id)
            .unwrap()
            .keys
            .insert(key.into(), (digest, request_id.clone()));
        s.requests.insert(
            request_id.clone(),
            Request {
                session_id: session_id.clone(),
                operation,
                alias: alias.clone(),
                preview,
                created: Instant::now(),
                finished: None,
                status: "pending",
                result: None,
                cancellation: CancellationToken::new(),
                source,
                file,
                grant_on_success: Approval::Once,
                expected_table_identity: expected,
                eligible_table,
                authorizing_grant_id: grant.as_ref().map(|g| g.id.clone()),
                write_target: write.map(|(table, _)| table),
                regenerated,
                audit_sql,
            },
        );
        s.record(&request_id, "request", "pending");
        self.publish(&mut s);
        drop(s);
        if grant.is_some() {
            let _ = self.approve_reserved(&request_id, Approval::Once, permits);
        }
        let mut s = self.inner.lock().unwrap();
        Ok(self.release(&mut s, &request_id))
    }

    /// Status for the agent, checked against the authorization valid now.
    fn release(&self, s: &mut State, request_id: &str) -> Value {
        if s.withhold_unauthorized(request_id) {
            self.publish(s);
        }
        request_status(s, request_id)
    }

    pub fn approve(&self, request_id: &str, decision: Approval) -> Result<(), String> {
        self.approve_reserved(request_id, decision, None)
    }

    fn reserve_execution(&self, state: &State, session_id: &str) -> Result<ExecutionPermits, ()> {
        let global = self
            .active_operations
            .clone()
            .try_acquire_owned()
            .map_err(|_| ())?;
        let session = state
            .sessions
            .get(session_id)
            .ok_or(())?
            .active_operation
            .clone()
            .try_acquire_owned()
            .map_err(|_| ())?;
        Ok(ExecutionPermits {
            _global: global,
            _session: session,
        })
    }

    fn approve_reserved(
        &self,
        request_id: &str,
        decision: Approval,
        permits: Option<ExecutionPermits>,
    ) -> Result<(), String> {
        let mut s = self.inner.lock().unwrap();
        s.expire();
        let request = s
            .requests
            .get(request_id)
            .ok_or("The request has expired.")?;
        if request.status != "pending" {
            return Err("The request is no longer awaiting approval.".into());
        }
        if decision == Approval::Deny {
            let r = s.requests.get_mut(request_id).unwrap();
            cancel(r, "denied");
            s.record(request_id, "approval", "denied");
            self.publish(&mut s);
            return Ok(());
        }
        if !s.sessions.contains_key(&request.session_id) {
            return Err("The session has expired.".into());
        }
        if !s.authorized_now(request) {
            let r = s.requests.get_mut(request_id).unwrap();
            cancel(r, "revoked");
            s.record(request_id, "approval", "revoked: access changed");
            self.publish(&mut s);
            return Err("Access to this resource changed. The request was revoked.".into());
        }
        if decision == Approval::ReadTableForSession && request.eligible_table.is_none()
            || decision == Approval::ReadSourceForSession
                && (request.operation.source_id().is_none() || request.operation.is_full_sql())
            || decision == Approval::FullSqlForSession
                && (!request.operation.is_full_sql()
                    || request.source.as_ref().is_none_or(|s| s.read_only))
        {
            return Err("This permission does not apply to the requested operation.".into());
        }
        let permits = match permits {
            Some(permits) => permits,
            None => self.reserve_execution(&s, &request.session_id).map_err(|_| "An operation is running or finishing cancellation. Approve this request after it finishes.".to_owned())?,
        };
        let generation = s.generation.clone();
        let permits = Arc::new(permits);
        let r = s.requests.get_mut(request_id).unwrap();
        r.status = "running";
        r.grant_on_success = decision;
        let execution = r.source.clone().map(|source| ExecutionRequest {
            source,
            operation: r.operation.clone(),
            expected_table_identity: r.expected_table_identity.clone(),
            cancellation: r.cancellation.clone(),
            execution_lease: Some(permits.clone()),
        });
        let file_input = r
            .file
            .clone()
            .map(|file| (file, r.operation.clone(), r.preview.clone()));
        let cancellation = r.cancellation.clone();
        let action = if r.authorizing_grant_id.as_deref() == Some(SHARED_ACCESS_GRANT) {
            "shared access"
        } else if r.authorizing_grant_id.is_some() {
            "session permission"
        } else {
            "approval"
        };
        s.record(request_id, action, "running");
        self.publish(&mut s);
        drop(s);
        let controller = self.clone();
        let request_id = request_id.to_owned();
        self.runtime.spawn(async move{
            // Revocation changes the UI state immediately, but capacity belongs
            // to this task until the executor has finished closing its connection.
            let _permits = permits;
            let output=if let Some(execution)=execution{
                let full=execution.operation.is_full_sql();
                match tokio::time::timeout(Duration::from_secs(35),executor::execute(execution)).await{
                    Ok(result)=>result,
                    Err(_)=>{cancellation.cancel();Err(ExecutionError{code:"limit_exceeded",message:"Operation timed out.",outcome_unknown:full})}
                }
            }else{
                let value=file_input.map(|(file,operation,text)|{
                    let start=match operation{Operation::ReadFile{start,..}=>start,_=>0};
                    let returned=text.chars().count();
                    json!({"text":text,"version":file.version,"start":start,"returned_chars":returned,"has_more":file.text.chars().count()>start+returned})
                }).unwrap_or(Value::Null);
                Ok(ExecutionOutput{value,table_identity:None})
            };
            controller.complete(&generation,&request_id,output);
        });
        Ok(())
    }

    fn complete(
        &self,
        generation: &str,
        request_id: &str,
        output: Result<ExecutionOutput, ExecutionError>,
    ) {
        let mut s = self.inner.lock().unwrap();
        s.expire();
        if s.generation != generation {
            return;
        }
        let Some(r) = s.requests.get_mut(request_id) else {
            return;
        };
        if r.status != "running" || r.cancellation.is_cancelled() {
            return;
        }
        if !s.authorized_now(&s.requests[request_id]) {
            cancel(s.requests.get_mut(request_id).unwrap(), "revoked");
            s.record(request_id, "result", "revoked: access changed");
            self.publish(&mut s);
            return;
        }
        let Some(r) = s.requests.get_mut(request_id) else {
            return;
        };
        let sid = r.session_id.clone();
        let operation = r.operation.clone();
        let decision = r.grant_on_success;
        let eligible_table = r.eligible_table.clone();
        let mut identity = None;
        let mut invalid_grant = None;
        let mut terminal_note = None;
        match output {
            Ok(output)
                if serde_json::to_vec(&output.value)
                    .is_ok_and(|bytes| bytes.len() <= MAX_RESULT_BYTES) =>
            {
                r.status = "completed";
                r.result = Some(output.value);
                identity = output.table_identity;
            }
            Ok(_) => {
                let message = if operation.is_full_sql() {
                    "Result exceeds the response limit. SQL effects may already have committed; do not retry a write."
                } else {
                    "Result exceeds the response limit."
                };
                terminal_note = Some(message);
                r.status = if operation.is_full_sql() {
                    "outcome_unknown"
                } else {
                    "failed"
                };
                r.result = Some(json!({"code":"limit_exceeded","message":message}));
            }
            Err(error) => {
                terminal_note = Some(public_execution_message(&error));
                if error.code == "resource_unavailable" && r.expected_table_identity.is_some() {
                    invalid_grant = r.authorizing_grant_id.clone();
                }
                r.status = if error.outcome_unknown {
                    "outcome_unknown"
                } else {
                    "failed"
                };
                // Only statically authored policy text may cross this boundary.
                // Driver text can contain endpoints and private certificate paths.
                r.result =
                    Some(json!({"code":error.code,"message":public_execution_message(&error)}));
            }
        }
        r.finished = Some(Instant::now());
        r.file = None;
        let status = r.status;
        if let Some(grant_id) = invalid_grant
            && let Some(session) = s.sessions.get_mut(&sid)
        {
            session.grants.retain(|g| g.id != grant_id);
        }
        if status == "completed"
            && let Some(source_id) = operation.source_id()
        {
            let scope = match decision {
                Approval::ReadSourceForSession => Some(GrantScope::ReadSource),
                Approval::FullSqlForSession => Some(GrantScope::FullSql),
                Approval::ReadTableForSession => eligible_table
                    .zip(identity)
                    .map(|(table, identity)| GrantScope::ReadTable { table, identity }),
                _ => None,
            };
            if let Some(scope) = scope
                && let Some(session) = s.sessions.get_mut(&sid)
            {
                session
                    .grants
                    .retain(|g| g.source_id != source_id || !same_scope(&g.scope, &scope));
                if session.grants.len() < 128 {
                    session.grants.push(Grant {
                        id: id(),
                        source_id: source_id.into(),
                        scope,
                    });
                }
            }
        }
        let audit_status =
            terminal_note.map_or_else(|| status.to_owned(), |note| format!("{status}: {note}"));
        s.record(request_id, "result", &audit_status);
        self.publish(&mut s);
    }

    pub(super) fn get_request(
        &self,
        p: &Principal,
        token: &str,
        request_id: &str,
    ) -> Result<Value, ApiError> {
        let mut s = self.inner.lock().unwrap();
        let sid = s.session_id(p, token)?;
        if !s.sessions[&sid]
            .keys
            .values()
            .any(|(_, id)| id == request_id)
        {
            return Err(ApiError::new(
                "resource_unavailable",
                "Request is unavailable.",
            ));
        }
        Ok(self.release(&mut s, request_id))
    }
    pub(super) fn cancel_request(
        &self,
        p: &Principal,
        token: &str,
        request_id: &str,
    ) -> Result<Value, ApiError> {
        let mut s = self.inner.lock().unwrap();
        let sid = s.session_id(p, token)?;
        let r = s
            .requests
            .get_mut(request_id)
            .filter(|r| r.session_id == sid)
            .ok_or_else(|| ApiError::new("resource_unavailable", "Request is unavailable."))?;
        cancel(r, "cancelled");
        self.publish(&mut s);
        Ok(request_status(&s, request_id))
    }
    pub(super) fn close_session(&self, p: &Principal, token: &str) -> Result<Value, ApiError> {
        let mut s = self.inner.lock().unwrap();
        let sid = s.session_id(p, token)?;
        s.cancel_session(&sid);
        self.publish(&mut s);
        Ok(json!({"status":"closed"}))
    }
}
fn local_target(request: &Request) -> Option<LocalTarget> {
    request.source.as_ref().map(|source| LocalTarget {
        source_id: source.id.clone(),
        database: source.database.clone(),
    })
}

const SHARED_ACCESS_GRANT: &str = "shared-access";

/// Standing access chosen in DBeast before the session began. It never widens
/// a read-only source and never authorizes document reads. The flag is set
/// when only a table write share authorizes a Full SQL write.
fn shared_access_grant(
    source_id: &str,
    source: &DataSource,
    access: &ShareAccess,
    operation: &Operation,
    eligible_table: Option<&TableRef>,
    write_target: Option<&TableRef>,
) -> Option<(Grant, bool)> {
    let full = operation.is_full_sql();
    if full && source.read_only {
        return None;
    }
    let source_allows = match access.source {
        SourceAccess::ReadWrite => true,
        SourceAccess::Read => !full,
        SourceAccess::Ask => false,
    };
    let table_allows = access.tables.iter().any(|(table, access)| {
        if full {
            *access == TableAccess::ReadWrite && write_target == Some(table)
        } else {
            eligible_table == Some(table)
        }
    });
    (source_allows || table_allows).then(|| {
        (
            Grant {
                id: SHARED_ACCESS_GRANT.into(),
                source_id: source_id.into(),
                scope: if full {
                    GrantScope::FullSql
                } else {
                    GrantScope::ReadSource
                },
            },
            full && !source_allows,
        )
    })
}

/// Whether `client_id` lost anything it had on `old`: the source itself, or
/// for any audience entry that applied to it (all agents or this connection)
/// the entry, its database level, or any of its tables or their levels.
fn narrowed(old: &SharedSource, new: Option<&SharedSource>, client_id: &str) -> bool {
    let applies = |audience: &Option<String>| audience.as_deref().is_none_or(|id| id == client_id);
    old.audiences
        .iter()
        .filter(|(audience, _)| applies(audience))
        .any(|(audience, before)| {
            let after: Vec<&ShareAccess> = new
                .into_iter()
                .flat_map(|new| &new.audiences)
                .filter(|(a, _)| a == audience)
                .map(|(_, access)| access)
                .collect();
            let source = after.iter().map(|a| a.source).max();
            source.is_none_or(|source| source < before.source)
                || before.tables.iter().any(|(table, level)| {
                    !after
                        .iter()
                        .flat_map(|a| &a.tables)
                        .any(|(t, l)| t == table && l >= level)
                })
        })
}

fn matching_grant(
    session: &Session,
    operation: &Operation,
    eligible_table: Option<&TableRef>,
) -> Option<Grant> {
    session
        .grants
        .iter()
        .filter(|g| {
            Some(g.source_id.as_str()) == operation.source_id()
                && match &g.scope {
                    GrantScope::FullSql => true,
                    GrantScope::ReadSource => !operation.is_full_sql(),
                    GrantScope::ReadTable { table, .. } => {
                        eligible_table == Some(table) && !operation.is_full_sql()
                    }
                }
        })
        .max_by_key(|g| match g.scope {
            GrantScope::FullSql => 3,
            GrantScope::ReadSource => 2,
            GrantScope::ReadTable { .. } => 1,
        })
        .cloned()
}
fn same_scope(a: &GrantScope, b: &GrantScope) -> bool {
    match (a, b) {
        (GrantScope::ReadSource, GrantScope::ReadSource)
        | (GrantScope::FullSql, GrantScope::FullSql) => true,
        (GrantScope::ReadTable { table: a, .. }, GrantScope::ReadTable { table: b, .. }) => a == b,
        _ => false,
    }
}
fn request_status(state: &State, id: &str) -> Value {
    match state.requests.get(id) {
        Some(r) => {
            json!({"request_id":id,"status":r.status,"result":r.result,"poll_after_ms":1000})
        }
        None => {
            json!({"request_id":id,"status":"result_expired","message":"The result expired. This request will not execute again. SQL effects may already have committed; do not retry a write with a new key."})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DriverKind;

    fn setup() -> (McpController, Principal, String, String) {
        let c = McpController::new(tokio::runtime::Handle::current(), || {});
        c.inner.lock().unwrap().running = true;
        let connection = c.create_client("test agent".into(), DEFAULT_PORT).unwrap();
        let principal = c.authenticate(&connection.token).unwrap();
        let session = c.begin_session(&principal, None).unwrap();
        (
            c,
            principal,
            session["session_token"].as_str().unwrap().into(),
            session["session_id"].as_str().unwrap().into(),
        )
    }
    fn document(c: &McpController) -> Operation {
        c.update_resources(
            Vec::new(),
            vec![SharedFile {
                id: "/private/document.sql".into(),
                alias: "Open document 1".into(),
                version: 1,
                text: Arc::from("SELECT 42;"),
            }],
        );
        let id = c.inner.lock().unwrap().files.keys().next().unwrap().clone();
        Operation::ReadFile {
            file_id: id,
            version: 1,
            start: 0,
            max_chars: 64,
        }
    }
    fn source(c: &McpController, read_only: bool) -> String {
        let mut ds = DataSource::new(DriverKind::Sqlite);
        ds.id = "/private/source.db".into();
        ds.name = "private-user@private-host".into();
        ds.host = "private-host".into();
        ds.user = "private-user".into();
        ds.password = "synthetic-credential".into();
        ds.file = "/private/source.db".into();
        ds.read_only = read_only;
        c.update_resources(
            vec![SharedSource::for_all_agents("SQLite source 1", ds)],
            Vec::new(),
        );
        c.inner
            .lock()
            .unwrap()
            .sources
            .keys()
            .next()
            .unwrap()
            .clone()
    }

    #[tokio::test]
    async fn session_secret_is_bound_to_client_and_display_id_has_no_authority() {
        let (c, p, token, session_id) = setup();
        let other = c.create_client("second agent".into(), DEFAULT_PORT).unwrap();
        let other = c.authenticate(&other.token).unwrap();
        assert!(c.catalog(&other, &token).is_err());
        assert!(c.catalog(&p, &session_id).is_err());
        assert!(c.catalog(&p, &token).is_ok());
        assert!(!format!("{:?}", c.snapshot()).contains(&token));
    }
    #[tokio::test]
    async fn begin_session_replaces_every_permission_and_old_token() {
        let (c, p, token, sid) = setup();
        let source_id = source(&c, false);
        c.inner
            .lock()
            .unwrap()
            .sessions
            .get_mut(&sid)
            .unwrap()
            .grants
            .push(Grant {
                id: id(),
                source_id,
                scope: GrantScope::FullSql,
            });
        let new = c
            .begin_session(&p, Some("new conversation".into()))
            .unwrap();
        assert_ne!(new["session_id"], sid);
        assert!(c.catalog(&p, &token).is_err());
        assert!(c.snapshot().sessions[0].grants.is_empty());
    }
    #[tokio::test]
    async fn catalog_uses_opaque_ids_and_separate_aliases() {
        let (c, p, token, _) = setup();
        let source_id = source(&c, false);
        assert_ne!(source_id, "/private/source.db");
        let catalog = c.catalog(&p, &token).unwrap().to_string();
        for secret in [
            "private-host",
            "private-user",
            "synthetic-credential",
            "/private",
            "password",
            "credential_ref",
        ] {
            assert!(!catalog.contains(secret));
        }
        assert!(catalog.contains("SQLite source 1"));
    }
    #[tokio::test]
    async fn duplicate_request_never_reexecutes_after_result_eviction() {
        let (c, p, token, _) = setup();
        let op = document(&c);
        let key = id();
        let first = c.submit(&p, &token, &key, op.clone()).unwrap();
        let rid = first["request_id"].as_str().unwrap();
        assert_eq!(first, c.submit(&p, &token, &key, op.clone()).unwrap());
        c.approve(rid, Approval::Deny).unwrap();
        c.inner
            .lock()
            .unwrap()
            .requests
            .get_mut(rid)
            .unwrap()
            .finished = Some(Instant::now() - REQUEST_TTL - Duration::from_secs(1));
        c.tick();
        assert_eq!(
            c.submit(&p, &token, &key, op).unwrap()["status"],
            "result_expired"
        );
        assert!(c.inner.lock().unwrap().requests.is_empty());
    }
    #[tokio::test]
    async fn idempotency_conflict_and_foreign_request_do_not_reveal_content() {
        let (c, p, token, _) = setup();
        let mut op = document(&c);
        let key = id();
        let first = c.submit(&p, &token, &key, op.clone()).unwrap();
        if let Operation::ReadFile { start, .. } = &mut op {
            *start = 1;
        }
        assert_eq!(
            c.submit(&p, &token, &key, op).unwrap_err().code,
            "request_conflict"
        );
        let other = c.create_client("other".into(), DEFAULT_PORT).unwrap();
        let other = c.authenticate(&other.token).unwrap();
        let session = c.begin_session(&other, None).unwrap();
        assert!(
            c.get_request(
                &other,
                session["session_token"].as_str().unwrap(),
                first["request_id"].as_str().unwrap()
            )
            .is_err()
        );
    }
    #[tokio::test]
    async fn once_is_consumed_and_revoke_discards_late_completion() {
        let (c, p, token, _) = setup();
        let op = document(&c);
        let result = c.submit(&p, &token, &id(), op).unwrap();
        let rid = result["request_id"].as_str().unwrap();
        c.approve(rid, Approval::Once).unwrap();
        assert!(c.approve(rid, Approval::Once).is_err());
        c.revoke_client(&p.client_id);
        tokio::task::yield_now().await;
        assert!(c.get_request(&p, &token, rid).is_err());
        assert!(c.inner.lock().unwrap().requests.is_empty());
    }
    #[tokio::test]
    async fn changing_shared_snapshot_revokes_the_approved_request() {
        let (c, p, token, _) = setup();
        let op = document(&c);
        let result = c.submit(&p, &token, &id(), op).unwrap();
        let rid = result["request_id"].as_str().unwrap();
        c.update_resources(
            Vec::new(),
            vec![SharedFile {
                id: "/private/document.sql".into(),
                alias: "Open document 1".into(),
                version: 2,
                text: Arc::from("new private contents"),
            }],
        );
        assert_eq!(
            c.get_request(&p, &token, rid).unwrap()["status"],
            "resource_unavailable"
        );
        assert!(c.approve(rid, Approval::Once).is_err());
    }
    #[tokio::test]
    async fn finished_document_reads_release_old_snapshots_without_renewing_retention() {
        let (c, p, token, _) = setup();
        let share = |version: u64, text: &Arc<str>| {
            c.update_resources(
                Vec::new(),
                vec![SharedFile {
                    id: "/private/document.sql".into(),
                    alias: "Open document 1".into(),
                    version,
                    text: Arc::clone(text),
                }],
            );
            let file_id = c.inner.lock().unwrap().files.keys().next().unwrap().clone();
            Operation::ReadFile {
                file_id,
                version,
                start: 0,
                max_chars: 64,
            }
        };
        let mut old_versions = Vec::new();
        let mut completed = None;
        for version in 1..=4u64 {
            let text: Arc<str> = Arc::from(format!("-- v{version}\n{}", "x".repeat(1 << 20)));
            old_versions.push(Arc::downgrade(&text));
            let op = share(version, &text);
            drop(text);
            let rid = c.submit(&p, &token, &id(), op).unwrap()["request_id"]
                .as_str()
                .unwrap()
                .to_owned();
            if version % 2 == 0 {
                c.cancel_request(&p, &token, &rid).unwrap();
            } else {
                c.approve(&rid, Approval::Once).unwrap();
                for _ in 0..100 {
                    if c.get_request(&p, &token, &rid).unwrap()["status"] != "running" {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                assert_eq!(c.get_request(&p, &token, &rid).unwrap()["status"], "completed");
                completed.get_or_insert(rid);
            }
        }
        // Only the version still shared is alive; finished and cancelled
        // requests no longer own the documents they read.
        let latest = c.inner.lock().unwrap().files.values().next().unwrap().text.clone();
        assert!(old_versions[3].upgrade().is_some_and(|t| Arc::ptr_eq(&t, &latest)));
        for old in &old_versions[..3] {
            assert!(old.upgrade().is_none(), "an old document version is retained");
        }
        // Later sharing changes revoke the result once and keep its finish time.
        let rid = completed.unwrap();
        let first_finish = c.inner.lock().unwrap().requests[&rid].finished;
        tokio::time::sleep(Duration::from_millis(5)).await;
        share(5, &Arc::from("v5"));
        share(6, &Arc::from("v6"));
        let state = c.inner.lock().unwrap();
        let request = &state.requests[&rid];
        assert_eq!(request.finished, first_finish);
        assert!(request.result.is_none());
    }
    #[tokio::test]
    async fn readonly_source_cannot_request_full_sql() {
        let (c, p, token, _) = setup();
        let source_id = source(&c, true);
        assert_eq!(
            c.submit(
                &p,
                &token,
                &id(),
                Operation::Sql {
                    source_id,
                    sql: "SELECT 1".into()
                }
            )
            .unwrap_err()
            .code,
            "permission_denied"
        );
        assert!(c.snapshot().requests.is_empty());
    }
    #[tokio::test]
    async fn policy_reasons_are_precise_and_driver_diagnostics_stay_private() {
        let (controller, principal, token, _) = setup();
        let source_id = source(&controller, false);
        let rejected = controller
            .submit(
                &principal,
                &token,
                &id(),
                Operation::ReadSql {
                    source_id: source_id.clone(),
                    sql: "SELECT id FROM items".into(),
                },
            )
            .unwrap_err();
        assert!(rejected.message.contains("schema.table"));
        let request = controller
            .submit(
                &principal,
                &token,
                &id(),
                Operation::Sql {
                    source_id,
                    sql: "SELECT 1".into(),
                },
            )
            .unwrap();
        let request_id = request["request_id"].as_str().unwrap();
        let generation = {
            let mut state = controller.inner.lock().unwrap();
            state.requests.get_mut(request_id).unwrap().status = "running";
            state.generation.clone()
        };
        controller.complete(&generation,request_id,Err(ExecutionError{code:"execution_failed",message:"synthetic-private-host synthetic-private-password /synthetic/private/path",outcome_unknown:false}));
        let result = controller
            .get_request(&principal, &token, request_id)
            .unwrap()
            .to_string();
        assert!(!result.contains("synthetic-private"));
        assert!(!result.contains("/synthetic"));
        let activity = &controller.snapshot().audit[0].status;
        assert!(activity.contains("Review the connection"));
        assert!(!activity.contains("synthetic-private"));
    }
    #[tokio::test]
    async fn table_and_read_source_grants_never_match_full_sql() {
        let (c, _, _, sid) = setup();
        let source_id = source(&c, false);
        let table = TableRef {
            schema: "main".into(),
            name: "items".into(),
        };
        let mut state = c.inner.lock().unwrap();
        let session = state.sessions.get_mut(&sid).unwrap();
        session.grants.push(Grant {
            id: id(),
            source_id: source_id.clone(),
            scope: GrantScope::ReadTable {
                table: table.clone(),
                identity: "one".into(),
            },
        });
        session.grants.push(Grant {
            id: id(),
            source_id: source_id.clone(),
            scope: GrantScope::ReadSource,
        });
        assert!(
            matching_grant(
                session,
                &Operation::Sql {
                    source_id: source_id.clone(),
                    sql: "SELECT 1".into()
                },
                Some(&table)
            )
            .is_none()
        );
        assert!(
            matching_grant(
                session,
                &Operation::ReadSql {
                    source_id,
                    sql: "SELECT id FROM main.items".into()
                },
                Some(&table)
            )
            .is_some()
        );
    }
    #[test]
    fn shared_access_never_widens_read_only_sources_or_read_shares() {
        let table = TableRef {
            schema: "main".into(),
            name: "items".into(),
        };
        let mut source = DataSource::new(DriverKind::Sqlite);
        let mut access = ShareAccess {
            source: SourceAccess::Read,
            tables: vec![(table.clone(), TableAccess::ReadWrite)],
        };
        let write = Operation::Sql {
            source_id: "s".into(),
            sql: "DELETE FROM main.items".into(),
        };
        let read = Operation::ReadSql {
            source_id: "s".into(),
            sql: "SELECT id FROM main.other".into(),
        };
        let grant = |source: &DataSource, access: &ShareAccess, op: &Operation, target| {
            shared_access_grant("s", source, access, op, None, target)
        };
        assert!(grant(&source, &access, &read, None).is_some());
        assert!(grant(&source, &access, &write, None).is_none());
        let (full, table_write_only) = grant(&source, &access, &write, Some(&table)).unwrap();
        assert!(matches!(full.scope, GrantScope::FullSql));
        assert!(table_write_only);
        source.read_only = true;
        assert!(grant(&source, &access, &write, Some(&table)).is_none());
        access.source = SourceAccess::ReadWrite;
        assert!(grant(&source, &access, &write, None).is_none());
        source.read_only = false;
        access = ShareAccess::default();
        assert!(grant(&source, &access, &read, None).is_none());
        assert!(grant(&source, &access, &document_operation(), None).is_none());
    }
    #[test]
    fn audiences_limit_visibility_and_merge_per_connection() {
        let table = TableRef {
            schema: "main".into(),
            name: "items".into(),
        };
        let shared = SharedSource {
            alias: "Shared".into(),
            source: DataSource::new(DriverKind::Sqlite),
            audiences: vec![
                (
                    Some("claude".into()),
                    ShareAccess {
                        source: SourceAccess::Ask,
                        tables: vec![(table.clone(), TableAccess::ReadWrite)],
                    },
                ),
                (
                    Some("codex".into()),
                    ShareAccess {
                        source: SourceAccess::Read,
                        tables: vec![(table.clone(), TableAccess::Read)],
                    },
                ),
            ],
        };
        assert!(shared.access_for("other").is_none());
        let claude = shared.access_for("claude").unwrap();
        assert_eq!(claude.source, SourceAccess::Ask);
        assert_eq!(claude.tables, vec![(table.clone(), TableAccess::ReadWrite)]);
        let mut everyone = shared.clone();
        everyone.audiences.push((None, ShareAccess::default()));
        let other = everyone.access_for("other").unwrap();
        assert_eq!(other, ShareAccess::default());
        let codex = everyone.access_for("codex").unwrap();
        assert_eq!(codex.source, SourceAccess::Read);
    }
    #[tokio::test]
    async fn table_write_share_runs_and_audits_regenerated_sql() {
        let (c, p, token, _) = setup();
        let mut shared = SharedSource::for_all_agents("Writable table", DataSource::new(DriverKind::Sqlite));
        shared.source.file = "/private/writable.db".into();
        let table = TableRef {
            schema: "main".into(),
            name: "items".into(),
        };
        shared.audiences = vec![(
            None,
            ShareAccess {
                source: SourceAccess::Ask,
                tables: vec![(table, TableAccess::ReadWrite)],
            },
        )];
        c.update_resources(vec![shared], Vec::new());
        let source_id = c.inner.lock().unwrap().sources.keys().next().unwrap().clone();
        c.submit(
            &p,
            &token,
            &id(),
            Operation::Sql {
                source_id: source_id.clone(),
                sql: "DELETE FROM main.items /* why */ WHERE id = 1 -- trailing".into(),
            },
        )
        .unwrap();
        let normalized = "DELETE FROM main.items WHERE id = 1";
        let s = c.inner.lock().unwrap();
        let request = s.requests.values().next().unwrap();
        assert_eq!(
            request.operation,
            Operation::Sql {
                source_id,
                sql: normalized.into()
            }
        );
        assert_eq!(request.preview, normalized);
        assert_eq!(request.audit_sql.as_deref(), Some(normalized));
    }

    #[tokio::test]
    async fn source_shared_with_one_connection_is_invisible_to_others() {
        let (c, p, token, _) = setup();
        let other = c.create_client("other agent".into(), DEFAULT_PORT).unwrap();
        let other_p = c.authenticate(&other.token).unwrap();
        let other_session = c.begin_session(&other_p, None).unwrap();
        let other_token = other_session["session_token"].as_str().unwrap();
        let mut shared = SharedSource::for_all_agents("Only mine", DataSource::new(DriverKind::Sqlite));
        shared.source.file = "/private/only-mine.db".into();
        shared.audiences = vec![(
            Some(p.client_id.clone()),
            ShareAccess {
                source: SourceAccess::Read,
                tables: Vec::new(),
            },
        )];
        c.update_resources(vec![shared], Vec::new());
        let mine = c.catalog(&p, &token).unwrap();
        assert_eq!(mine["sources"][0]["shared_access"], "read");
        let source_id = mine["sources"][0]["source_id"].as_str().unwrap().to_owned();
        let theirs = c.catalog(&other_p, other_token).unwrap();
        assert_eq!(theirs["sources"].as_array().unwrap().len(), 0);
        let denied = c
            .submit(
                &other_p,
                other_token,
                &id(),
                Operation::ListTables { source_id },
            )
            .unwrap_err();
        assert_eq!(denied.code, "resource_unavailable");
    }
    #[tokio::test]
    async fn connections_survive_stop_and_restore_without_tokens() {
        let (c, _, _, _) = setup();
        let connection = c.create_client("remembered".into(), DEFAULT_PORT).unwrap();
        let stored = c.stored_clients();
        assert!(stored.iter().all(|client| !client.token_hash.contains(&connection.token)));
        c.stop();
        c.inner.lock().unwrap().running = true;
        assert!(c.authenticate(&connection.token).is_some());
        let restored = McpController::new(tokio::runtime::Handle::current(), || {});
        restored.inner.lock().unwrap().running = true;
        restored.restore_clients(stored);
        let principal = restored.authenticate(&connection.token).unwrap();
        assert_eq!(principal.client_id, connection.id);
        restored.revoke_client(&connection.id);
        assert!(restored.authenticate(&connection.token).is_none());
    }
    fn document_operation() -> Operation {
        Operation::ReadFile {
            file_id: "f".into(),
            version: 1,
            start: 0,
            max_chars: 10,
        }
    }
    #[tokio::test]
    async fn limits_expire_without_new_client_requests_and_stop_invalidates_auth() {
        let (c, p, token, sid) = setup();
        let op = document(&c);
        c.submit(&p, &token, &id(), op).unwrap();
        c.inner
            .lock()
            .unwrap()
            .sessions
            .get_mut(&sid)
            .unwrap()
            .created = Instant::now() - SESSION_TTL - Duration::from_secs(1);
        c.tick();
        assert!(c.snapshot().sessions.is_empty());
        assert!(c.catalog(&p, &token).is_err());
        let client = c.create_client("surviving client".into(), DEFAULT_PORT).unwrap();
        c.stop();
        assert!(c.authenticate(&client.token).is_none());
        assert!(!c.snapshot().running);
    }
    #[tokio::test]
    async fn idle_expiry_cancels_running_work_before_removing_its_result() {
        let (c, p, token, sid) = setup();
        let operation = document(&c);
        let result = c.submit(&p, &token, &id(), operation).unwrap();
        let rid = result["request_id"].as_str().unwrap();
        let cancellation = {
            let mut state = c.inner.lock().unwrap();
            let request = state.requests.get_mut(rid).unwrap();
            request.status = "running";
            request.cancellation.clone()
        };
        c.inner
            .lock()
            .unwrap()
            .sessions
            .get_mut(&sid)
            .unwrap()
            .touched = Instant::now() - IDLE_TTL - Duration::from_secs(1);
        c.tick();
        assert!(cancellation.is_cancelled());
        assert!(c.snapshot().requests.is_empty());
        assert!(c.get_request(&p, &token, rid).is_err());
    }
    #[tokio::test]
    async fn key_capacity_never_discards_a_live_sessions_tombstones() {
        let (c, p, token, sid) = setup();
        let operation = document(&c);
        let key = id();
        let first = c.submit(&p, &token, &key, operation.clone()).unwrap();
        let rid = first["request_id"].as_str().unwrap();
        {
            let mut state = c.inner.lock().unwrap();
            state.requests.remove(rid);
            let session = state.sessions.get_mut(&sid).unwrap();
            while session.keys.len() < MAX_KEYS {
                session.keys.insert(id(), ([0; 32], id()));
            }
        }
        assert_eq!(
            c.submit(&p, &token, &id(), operation.clone())
                .unwrap_err()
                .code,
            "limit_exceeded"
        );
        assert_eq!(
            c.submit(&p, &token, &key, operation).unwrap()["status"],
            "result_expired"
        );
        assert_eq!(c.inner.lock().unwrap().sessions[&sid].keys.len(), MAX_KEYS);
    }
    #[tokio::test]
    async fn oversized_full_sql_result_keeps_the_possibility_of_committed_effects() {
        let (c, p, token, _) = setup();
        let source_id = source(&c, false);
        let result = c
            .submit(
                &p,
                &token,
                &id(),
                Operation::Sql {
                    source_id,
                    sql: "SELECT 1".into(),
                },
            )
            .unwrap();
        let rid = result["request_id"].as_str().unwrap();
        let generation = {
            let mut state = c.inner.lock().unwrap();
            state.requests.get_mut(rid).unwrap().status = "running";
            state.generation.clone()
        };
        c.complete(
            &generation,
            rid,
            Ok(ExecutionOutput {
                value: json!({"text":"x".repeat(MAX_RESULT_BYTES)}),
                table_identity: None,
            }),
        );
        assert_eq!(
            c.get_request(&p, &token, rid).unwrap()["status"],
            "outcome_unknown"
        );
    }
    #[tokio::test]
    async fn queued_document_cannot_gain_session_wide_or_sql_permission() {
        let (c, p, token, _) = setup();
        let op = document(&c);
        let result = c.submit(&p, &token, &id(), op).unwrap();
        let rid = result["request_id"].as_str().unwrap();
        for grant in [
            Approval::ReadTableForSession,
            Approval::ReadSourceForSession,
            Approval::FullSqlForSession,
        ] {
            assert!(c.approve(rid, grant).is_err());
        }
    }
    #[tokio::test]
    async fn repeated_revocation_preserves_unknown_or_completed_write_outcomes() {
        let (c, p, token, sid) = setup();
        let source_id = source(&c, false);
        let result = c
            .submit(
                &p,
                &token,
                &id(),
                Operation::Sql {
                    source_id,
                    sql: "SELECT 1".into(),
                },
            )
            .unwrap();
        let rid = result["request_id"].as_str().unwrap();
        c.inner
            .lock()
            .unwrap()
            .requests
            .get_mut(rid)
            .unwrap()
            .status = "running";
        assert_eq!(
            c.cancel_request(&p, &token, rid).unwrap()["status"],
            "outcome_unknown"
        );
        assert_eq!(
            c.cancel_request(&p, &token, rid).unwrap()["status"],
            "outcome_unknown"
        );
        c.revoke_grant(&sid, "any-grant");
        assert_eq!(
            c.get_request(&p, &token, rid).unwrap()["status"],
            "outcome_unknown"
        );
        c.inner
            .lock()
            .unwrap()
            .requests
            .get_mut(rid)
            .unwrap()
            .status = "completed";
        assert_eq!(
            c.cancel_request(&p, &token, rid).unwrap()["status"],
            "completed_result_revoked"
        );
        assert_eq!(
            c.cancel_request(&p, &token, rid).unwrap()["status"],
            "completed_result_revoked"
        );
    }
    #[tokio::test]
    async fn grants_are_deduplicated_and_changed_identity_requires_new_consent() {
        let (c, p, token, sid) = setup();
        let source_id = source(&c, false);
        let table = TableRef {
            schema: "main".into(),
            name: "items".into(),
        };
        let operation = Operation::DescribeTable {
            source_id: source_id.clone(),
            table: table.clone(),
        };
        let first = c.submit(&p, &token, &id(), operation.clone()).unwrap();
        let second = c.submit(&p, &token, &id(), operation.clone()).unwrap();
        let generation = c.inner.lock().unwrap().generation.clone();
        for request in [&first, &second] {
            let rid = request["request_id"].as_str().unwrap();
            {
                let mut state = c.inner.lock().unwrap();
                let r = state.requests.get_mut(rid).unwrap();
                r.status = "running";
                r.grant_on_success = Approval::ReadTableForSession;
            }
            c.complete(
                &generation,
                rid,
                Ok(ExecutionOutput {
                    value: json!({}),
                    table_identity: Some("table-one".into()),
                }),
            );
        }
        assert_eq!(c.snapshot().sessions[0].grants.len(), 1);
        let rid = second["request_id"].as_str().unwrap();
        {
            let mut state = c.inner.lock().unwrap();
            let grant_id = state.sessions[&sid].grants[0].id.clone();
            let r = state.requests.get_mut(rid).unwrap();
            r.status = "running";
            r.expected_table_identity = Some("table-one".into());
            r.authorizing_grant_id = Some(grant_id);
        }
        c.complete(
            &generation,
            rid,
            Err(ExecutionError {
                code: "resource_unavailable",
                message: "changed",
                outcome_unknown: false,
            }),
        );
        assert!(c.snapshot().sessions[0].grants.is_empty());
        assert_eq!(
            c.submit(&p, &token, &id(), operation).unwrap()["status"],
            "pending"
        );
    }
    #[tokio::test]
    async fn floods_are_rate_limited_and_ui_notifications_are_coalesced() {
        use std::sync::atomic::AtomicUsize;
        let notifications = Arc::new(AtomicUsize::new(0));
        let count = notifications.clone();
        let c = McpController::new(tokio::runtime::Handle::current(), move || {
            count.fetch_add(1, Ordering::Relaxed);
        });
        c.inner.lock().unwrap().running = true;
        let client = c.create_client("test".into(), DEFAULT_PORT).unwrap();
        let p = c.authenticate(&client.token).unwrap();
        assert_eq!((0..1000).filter(|_| c.allow_http(&p)).count(), 20);
        let session = c.begin_session(&p, None).unwrap();
        let token = session["session_token"].as_str().unwrap();
        let operation = document(&c);
        let request = c.submit(&p, token, &id(), operation).unwrap();
        let rid = request["request_id"].as_str().unwrap();
        for _ in 0..300 {
            c.cancel_request(&p, token, rid).unwrap();
        }
        assert_eq!(notifications.load(Ordering::Relaxed), 1);
        c.snapshot();
        let started = Instant::now();
        c.stop();
        assert!(started.elapsed() < Duration::from_millis(100));
        assert_eq!(notifications.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn cancelled_work_holds_session_capacity_until_its_cleanup_finishes() {
        let (controller, principal, token, session_id) = setup();
        let operation = document(&controller);
        let first = controller
            .submit(&principal, &token, &id(), operation.clone())
            .unwrap();
        let first_id = first["request_id"].as_str().unwrap();
        let permits = {
            let mut state = controller.inner.lock().unwrap();
            state.requests.get_mut(first_id).unwrap().status = "running";
            controller.reserve_execution(&state, &session_id).unwrap()
        };
        let (finish_cleanup, cleanup) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let _permits = permits;
            let _ = cleanup.await;
        });
        controller
            .cancel_request(&principal, &token, first_id)
            .unwrap();
        let second = controller
            .submit(&principal, &token, &id(), operation)
            .unwrap();
        let second_id = second["request_id"].as_str().unwrap();
        assert!(controller.approve(second_id, Approval::Once).is_err());
        assert_eq!(controller.active_operations.available_permits(), 3);
        finish_cleanup.send(()).unwrap();
        task.await.unwrap();
        assert!(controller.approve(second_id, Approval::Once).is_ok());
    }

    #[tokio::test]
    async fn restarting_sessions_or_server_does_not_reset_global_cleanup_capacity() {
        let (controller, _, _, _) = setup();
        let mut held = Vec::new();
        for index in 0..4 {
            let connection = controller
                .create_client(format!("cleanup {index}"), DEFAULT_PORT)
                .unwrap();
            let principal = controller.authenticate(&connection.token).unwrap();
            let session = controller.begin_session(&principal, None).unwrap();
            held.push(
                controller
                    .reserve_execution(
                        &controller.inner.lock().unwrap(),
                        session["session_id"].as_str().unwrap(),
                    )
                    .unwrap(),
            );
        }
        controller.stop();
        controller.inner.lock().unwrap().running = true;
        let connection = controller.create_client("new generation".into(), DEFAULT_PORT).unwrap();
        let principal = controller.authenticate(&connection.token).unwrap();
        let session = controller.begin_session(&principal, None).unwrap();
        let token = session["session_token"].as_str().unwrap();
        let operation = document(&controller);
        let request = controller
            .submit(&principal, token, &id(), operation)
            .unwrap();
        let request_id = request["request_id"].as_str().unwrap();
        assert_eq!(controller.active_operations.available_permits(), 0);
        assert!(controller.approve(request_id, Approval::Once).is_err());
        held.pop();
        assert!(controller.approve(request_id, Approval::Once).is_ok());
    }

    #[tokio::test]
    async fn automatic_grant_cannot_enqueue_more_work_during_cancel_cleanup() {
        let (controller, principal, token, session_id) = setup();
        let source_id = source(&controller, false);
        let held = {
            let mut state = controller.inner.lock().unwrap();
            state
                .sessions
                .get_mut(&session_id)
                .unwrap()
                .grants
                .push(Grant {
                    id: id(),
                    source_id: source_id.clone(),
                    scope: GrantScope::ReadSource,
                });
            controller.reserve_execution(&state, &session_id).unwrap()
        };
        let operation = Operation::ReadSql {
            source_id,
            sql: "SELECT id FROM main.items".into(),
        };
        assert_eq!(
            controller
                .submit(&principal, &token, &id(), operation)
                .unwrap_err()
                .code,
            "limit_exceeded"
        );
        assert!(controller.snapshot().requests.is_empty());
        drop(held);
    }

    fn items() -> TableRef {
        TableRef {
            schema: "main".into(),
            name: "items".into(),
        }
    }
    fn shared(audiences: Vec<(Option<String>, ShareAccess)>) -> SharedSource {
        let mut source = DataSource::new(DriverKind::Sqlite);
        source.id = "narrowing-source".into();
        source.file = "/private/narrowing.db".into();
        SharedSource {
            alias: "Narrowing source".into(),
            source,
            audiences,
        }
    }
    fn access(source: SourceAccess, tables: Vec<(TableRef, TableAccess)>) -> ShareAccess {
        ShareAccess { source, tables }
    }
    /// Shares a source and adds a session grant as if the user had approved it.
    fn share_with_grant(
        c: &McpController,
        sid: &str,
        audiences: Vec<(Option<String>, ShareAccess)>,
        scope: GrantScope,
    ) -> String {
        c.update_resources(vec![shared(audiences)], Vec::new());
        let mut state = c.inner.lock().unwrap();
        let source_id = state.sources.keys().next().unwrap().clone();
        state.sessions.get_mut(sid).unwrap().grants.push(Grant {
            id: id(),
            source_id: source_id.clone(),
            scope,
        });
        source_id
    }
    fn table_grant() -> GrantScope {
        GrantScope::ReadTable {
            table: items(),
            identity: "items-identity".into(),
        }
    }

    #[tokio::test]
    async fn unsharing_a_table_revokes_its_session_grant() {
        let (c, p, token, sid) = setup();
        let source_id = share_with_grant(
            &c,
            &sid,
            vec![(
                None,
                access(SourceAccess::Ask, vec![(items(), TableAccess::Read)]),
            )],
            table_grant(),
        );
        c.update_resources(
            vec![shared(vec![(None, access(SourceAccess::Ask, Vec::new()))])],
            Vec::new(),
        );
        assert!(c.snapshot().sessions[0].grants.is_empty());
        let request = c
            .submit(
                &p,
                &token,
                &id(),
                Operation::DescribeTable {
                    source_id,
                    table: items(),
                },
            )
            .unwrap();
        assert_eq!(request["status"], "pending");
    }

    #[tokio::test]
    async fn lowering_database_access_revokes_full_sql_session_grant() {
        let (c, p, token, sid) = setup();
        let source_id = share_with_grant(
            &c,
            &sid,
            vec![(None, access(SourceAccess::ReadWrite, Vec::new()))],
            GrantScope::FullSql,
        );
        c.update_resources(
            vec![shared(vec![(None, access(SourceAccess::Read, Vec::new()))])],
            Vec::new(),
        );
        assert!(c.snapshot().sessions[0].grants.is_empty());
        let request = c
            .submit(
                &p,
                &token,
                &id(),
                Operation::Sql {
                    source_id,
                    sql: "DELETE FROM main.items".into(),
                },
            )
            .unwrap();
        assert_eq!(request["status"], "pending");
    }

    #[tokio::test]
    async fn removing_a_specific_recipient_revokes_grants_despite_all_agents_entry() {
        let (c, p, _, sid) = setup();
        share_with_grant(
            &c,
            &sid,
            vec![
                (
                    Some(p.client_id.clone()),
                    access(SourceAccess::Ask, Vec::new()),
                ),
                (None, access(SourceAccess::Ask, Vec::new())),
            ],
            GrantScope::ReadSource,
        );
        c.update_resources(
            vec![shared(vec![(None, access(SourceAccess::Ask, Vec::new()))])],
            Vec::new(),
        );
        assert!(c.snapshot().sessions[0].grants.is_empty());
    }

    #[tokio::test]
    async fn widening_or_unrelated_refresh_keeps_grants_and_results() {
        let (c, p, token, sid) = setup();
        let source_id = share_with_grant(
            &c,
            &sid,
            vec![(None, access(SourceAccess::Ask, Vec::new()))],
            table_grant(),
        );
        let rid = completed_request(&c, &p, &token, &source_id, Some(SHARED_ACCESS_GRANT));
        c.update_resources(
            vec![shared(vec![(
                None,
                access(SourceAccess::Read, vec![(items(), TableAccess::ReadWrite)]),
            )])],
            Vec::new(),
        );
        assert_eq!(c.snapshot().sessions[0].grants.len(), 1);
        // The request was authorized by standing access that exists only now;
        // what matters is that widening did not cancel it.
        assert_eq!(c.get_request(&p, &token, &rid).unwrap()["status"], "completed");
    }

    /// A finished read on `source_id` authorized by `grant`, with a result to release.
    fn completed_request(
        c: &McpController,
        p: &Principal,
        token: &str,
        source_id: &str,
        grant: Option<&str>,
    ) -> String {
        let request = c
            .submit(
                p,
                token,
                &id(),
                Operation::ListTables {
                    source_id: source_id.into(),
                },
            )
            .unwrap();
        let rid = request["request_id"].as_str().unwrap().to_owned();
        let mut state = c.inner.lock().unwrap();
        let r = state.requests.get_mut(&rid).unwrap();
        r.status = "completed";
        r.result = Some(json!({"rows":[["private row"]]}));
        r.finished = Some(Instant::now());
        r.authorizing_grant_id = grant.map(str::to_owned);
        rid
    }

    #[tokio::test]
    async fn narrowing_standing_access_withholds_completed_results() {
        let (c, p, token, _) = setup();
        c.update_resources(
            vec![shared(vec![(None, access(SourceAccess::Read, Vec::new()))])],
            Vec::new(),
        );
        let source_id = c.inner.lock().unwrap().sources.keys().next().unwrap().clone();
        let rid = completed_request(&c, &p, &token, &source_id, Some(SHARED_ACCESS_GRANT));
        c.update_resources(
            vec![shared(vec![(None, access(SourceAccess::Ask, Vec::new()))])],
            Vec::new(),
        );
        let status = c.get_request(&p, &token, &rid).unwrap();
        assert_ne!(status["status"], "completed");
        assert!(status["result"].is_null());
    }

    #[tokio::test]
    async fn results_are_released_only_while_their_authorization_is_valid() {
        let (c, p, token, sid) = setup();
        let source_id = share_with_grant(
            &c,
            &sid,
            vec![(None, access(SourceAccess::Ask, Vec::new()))],
            GrantScope::ReadSource,
        );
        let grant_id = c.inner.lock().unwrap().sessions[&sid].grants[0].id.clone();
        let rid = completed_request(&c, &p, &token, &source_id, Some(&grant_id));
        assert_eq!(c.get_request(&p, &token, &rid).unwrap()["status"], "completed");
        // Removed without going through a revocation path that cancels requests.
        c.inner
            .lock()
            .unwrap()
            .sessions
            .get_mut(&sid)
            .unwrap()
            .grants
            .clear();
        let status = c.get_request(&p, &token, &rid).unwrap();
        assert_eq!(status["status"], "revoked");
        assert!(status["result"].is_null());
    }

    #[tokio::test]
    async fn automatic_execution_rechecks_its_authorization_before_starting() {
        let (c, p, token, _) = setup();
        c.update_resources(
            vec![shared(vec![(None, access(SourceAccess::Ask, Vec::new()))])],
            Vec::new(),
        );
        let source_id = c.inner.lock().unwrap().sources.keys().next().unwrap().clone();
        let request = c
            .submit(&p, &token, &id(), Operation::ListTables { source_id })
            .unwrap();
        let rid = request["request_id"].as_str().unwrap();
        // As if standing access had authorized it and was narrowed before start.
        c.inner
            .lock()
            .unwrap()
            .requests
            .get_mut(rid)
            .unwrap()
            .authorizing_grant_id = Some(SHARED_ACCESS_GRANT.into());
        assert!(c.approve(rid, Approval::Once).is_err());
        assert_ne!(c.get_request(&p, &token, rid).unwrap()["status"], "running");
    }

    #[tokio::test]
    #[ignore = "manual controller cost measurement; no database or user configuration"]
    async fn benchmark_controller_snapshots() {
        fn cached_snapshot_us(controller: &McpController) -> f64 {
            for _ in 0..1_000 {
                std::hint::black_box(controller.snapshot());
            }
            let mut trials = Vec::new();
            for _ in 0..7 {
                let start = Instant::now();
                for _ in 0..10_000 {
                    std::hint::black_box(controller.snapshot());
                }
                trials.push(start.elapsed().as_secs_f64() * 1_000_000.0 / 10_000.0);
            }
            trials.sort_by(f64::total_cmp);
            trials[3]
        }
        let controller = McpController::new(tokio::runtime::Handle::current(), || {});
        let disabled = cached_snapshot_us(&controller);
        controller.inner.lock().unwrap().running = true;
        let source_id = source(&controller, false);
        let enabled_idle = cached_snapshot_us(&controller);
        let operation = Operation::Sql {
            source_id,
            sql: format!("SELECT '{}'", "x".repeat(MAX_SQL_BYTES - 10)),
        };
        for index in 0..8 {
            let connection = controller
                .create_client(format!("Synthetic client {index}"), DEFAULT_PORT)
                .unwrap();
            let principal = controller.authenticate(&connection.token).unwrap();
            let session = controller.begin_session(&principal, None).unwrap();
            let token = session["session_token"].as_str().unwrap();
            for _ in 0..4 {
                controller
                    .submit(&principal, token, &id(), operation.clone())
                    .unwrap();
            }
        }
        assert_eq!(controller.snapshot().requests.len(), 32);
        let pending = cached_snapshot_us(&controller);
        let mut times = Vec::new();
        for _ in 0..50 {
            let start = Instant::now();
            controller.tick();
            times.push(start.elapsed().as_secs_f64() * 1_000.0);
        }
        times.sort_by(f64::total_cmp);
        println!(
            "MCP controller snapshot: disabled={disabled:.3} us; enabled_idle={enabled_idle:.3} us; 32_pending={pending:.3} us. Background publication with 32 maximum-size SQL previews: median={:.3} ms; max={:.3} ms. CPU only, no UI, network or database.",
            times[25], times[49]
        );
        controller.stop();
    }
}
