//! Activity Monitor model: PostgreSQL sessions from `pg_stat_activity`, server
//! counters, and what the dashboard derives from them (state counts, blocking
//! trees, breakdowns, a short history). Durations come from the server's own
//! clock, so they do not depend on the client's time or the polling delay.

use crate::model::{QueryResult, Value};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

/// Samples kept for the timeline: five minutes at the default 2 s interval.
pub const HISTORY: usize = 150;

/// Sessions of the whole cluster, without the monitor's own session.
/// pg_blocking_pids briefly locks the shared lock manager, so it runs only for
/// sessions that actually wait for a lock.
pub const SESSIONS_SQL: &str = "SELECT pid, datname::text, usename::text, application_name, client_addr::text, \
backend_type, state, wait_event_type, wait_event, \
EXTRACT(EPOCH FROM now() - backend_start)::float8, \
EXTRACT(EPOCH FROM now() - xact_start)::float8, \
EXTRACT(EPOCH FROM now() - query_start)::float8, \
EXTRACT(EPOCH FROM now() - state_change)::float8, \
CASE WHEN wait_event_type = 'Lock' THEN pg_blocking_pids(pid)::text END, query, backend_xid::text, backend_xmin::text, \
to_char(backend_start, 'YYYY-MM-DD HH24:MI:SS'), to_char(query_start, 'YYYY-MM-DD HH24:MI:SS') \
FROM pg_stat_activity WHERE pid <> pg_backend_pid() ORDER BY pid";

/// Server-wide counters read with every sample. Reading pg_locks copies the
/// whole lock table, so it is skipped while no visible session waits for a
/// lock; a role that cannot see others' waits always reads it.
pub const SERVER_SQL: &str = "WITH r AS (SELECT pg_has_role(current_user, 'pg_read_all_stats', 'USAGE') \
OR (SELECT rolsuper FROM pg_roles WHERE rolname = current_user) AS sees_all) \
SELECT current_setting('max_connections')::int, \
(SELECT sum(xact_commit + xact_rollback) FROM pg_stat_database)::float8, \
CASE WHEN NOT r.sees_all OR EXISTS (SELECT 1 FROM pg_stat_activity WHERE wait_event_type = 'Lock') \
THEN (SELECT count(*) FROM pg_locks WHERE NOT granted)::int ELSE 0 END, \
current_database()::text, current_setting('server_version'), r.sees_all FROM r";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum State {
    Active,
    /// Active but waiting for a lock (or another backend).
    Waiting,
    IdleInTransaction,
    IdleInTransactionAborted,
    Idle,
    FastPath,
    Disabled,
    /// Background workers, autovacuum and others without a client state.
    Background,
}

impl State {
    pub const ALL: [State; 8] = [
        State::Active,
        State::Waiting,
        State::IdleInTransaction,
        State::IdleInTransactionAborted,
        State::Idle,
        State::FastPath,
        State::Disabled,
        State::Background,
    ];

    pub fn label(self) -> &'static str {
        match self {
            State::Active => "active",
            State::Waiting => "waiting",
            State::IdleInTransaction => "idle in transaction",
            State::IdleInTransactionAborted => "idle in transaction (aborted)",
            State::Idle => "idle",
            State::FastPath => "fastpath function call",
            State::Disabled => "disabled",
            State::Background => "background",
        }
    }

    fn parse(state: Option<&str>, wait_type: Option<&str>) -> Self {
        match state {
            // Lock waits make an active session effectively stuck; I/O and
            // client waits are part of normal work.
            Some("active") if matches!(wait_type, Some("Lock")) => State::Waiting,
            Some("active") => State::Active,
            Some("idle in transaction") => State::IdleInTransaction,
            Some("idle in transaction (aborted)") => State::IdleInTransactionAborted,
            Some("idle") => State::Idle,
            Some("fastpath function call") => State::FastPath,
            Some("disabled") => State::Disabled,
            _ => State::Background,
        }
    }

    pub fn in_transaction(self) -> bool {
        matches!(self, State::IdleInTransaction | State::IdleInTransactionAborted)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Session {
    pub pid: i32,
    pub database: String,
    pub user: String,
    pub application: String,
    pub client: String,
    pub backend_type: String,
    pub state: State,
    pub wait_type: String,
    pub wait_event: String,
    /// Seconds since the backend, transaction and query started, and since the
    /// state last changed, by the server's clock.
    pub backend_age: Option<f64>,
    pub xact_age: Option<f64>,
    pub query_age: Option<f64>,
    pub state_age: Option<f64>,
    pub blocked_by: Vec<i32>,
    pub query: String,
    pub xid: String,
    pub xmin: String,
    pub backend_start: String,
    pub query_start: String,
    /// Derived once per sample for the table and sorting: "type: event" and
    /// the query collapsed to one shortened line.
    pub wait: String,
    pub query_line: String,
}

impl Session {
    /// Fills the derived fields.
    pub fn derive(mut self) -> Self {
        self.wait = match (self.wait_type.is_empty(), self.wait_event.is_empty()) {
            (true, _) => String::new(),
            (false, true) => self.wait_type.clone(),
            (false, false) => format!("{}: {}", self.wait_type, self.wait_event),
        };
        self.query_line = first_line(&self.query);
        self
    }

    pub fn is_client(&self) -> bool {
        self.backend_type == "client backend"
    }

    /// The server hides other users' queries without pg_read_all_stats.
    pub fn query_hidden(&self) -> bool {
        self.query == "<insufficient privilege>"
    }

    /// How long the current state has lasted where that is what matters:
    /// the query for active sessions, the transaction for idle-in-transaction.
    pub fn duration(&self) -> Option<f64> {
        match self.state {
            State::Active | State::Waiting | State::FastPath => self.query_age,
            State::IdleInTransaction | State::IdleInTransactionAborted => self.xact_age,
            State::Idle => self.state_age,
            State::Disabled | State::Background => self.backend_age,
        }
    }

    pub fn wait(&self) -> &str {
        &self.wait
    }

    /// Lower-case text the dashboard filter matches against.
    pub fn matches(&self, needle: &str) -> bool {
        needle.is_empty()
            || [&self.user, &self.database, &self.application, &self.client, &self.query, &self.backend_type, &self.wait_event]
                .iter()
                .any(|field| field.to_lowercase().contains(needle))
            || self.pid.to_string() == needle
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Server {
    pub max_connections: i64,
    /// Committed plus rolled back transactions of all databases so far.
    pub transactions: f64,
    pub waiting_locks: i64,
    pub database: String,
    pub version: String,
    /// The monitor's role can see every session's query.
    pub sees_all: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub sessions: Vec<Session>,
    pub server: Server,
    /// When the sample was taken, by the client, for rates and staleness.
    pub at: Option<std::time::Instant>,
}

/// The query on one line, cut to 300 bytes.
fn first_line(query: &str) -> String {
    let mut line: String = query.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.len() > 300 {
        let cut = (0..=300).rev().find(|&i| line.is_char_boundary(i)).unwrap_or(0);
        line.truncate(cut);
        line.push('…');
    }
    line
}

fn text(row: &[Value], i: usize) -> String {
    match row.get(i) {
        Some(Value::Null) | None => String::new(),
        Some(Value::Text(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

fn number(row: &[Value], i: usize) -> Option<f64> {
    match row.get(i)? {
        Value::Float(f) => Some(*f),
        Value::Int(i) => Some(*i as f64),
        Value::Decimal(d) | Value::Text(d) => d.parse().ok(),
        _ => None,
    }
}

/// `{123,456}` from `pg_blocking_pids(pid)::text`.
fn pids(text: &str) -> Vec<i32> {
    text.trim_matches(|c| c == '{' || c == '}')
        .split(',')
        .filter_map(|p| p.trim().parse().ok())
        .collect()
}

pub fn parse_sessions(data: &QueryResult) -> Vec<Session> {
    data.rows
        .iter()
        .map(|row| {
            let state = text(row, 6);
            let wait_type = text(row, 7);
            Session {
                pid: number(row, 0).unwrap_or_default() as i32,
                database: text(row, 1),
                user: text(row, 2),
                application: text(row, 3),
                client: text(row, 4),
                backend_type: text(row, 5),
                state: State::parse((!state.is_empty()).then_some(state.as_str()), (!wait_type.is_empty()).then_some(wait_type.as_str())),
                wait_type,
                wait_event: text(row, 8),
                backend_age: number(row, 9),
                xact_age: number(row, 10),
                query_age: number(row, 11),
                state_age: number(row, 12),
                blocked_by: pids(&text(row, 13)),
                query: text(row, 14),
                xid: text(row, 15),
                xmin: text(row, 16),
                backend_start: text(row, 17),
                query_start: text(row, 18),
                wait: String::new(),
                query_line: String::new(),
            }
            .derive()
        })
        .collect()
}

pub fn parse_server(data: &QueryResult) -> Server {
    let Some(row) = data.rows.first() else {
        return Server::default();
    };
    Server {
        max_connections: number(row, 0).unwrap_or_default() as i64,
        transactions: number(row, 1).unwrap_or_default(),
        waiting_locks: number(row, 2).unwrap_or_default() as i64,
        database: text(row, 3),
        version: text(row, 4),
        sees_all: matches!(row.get(5), Some(Value::Bool(true))),
    }
}

/// Counts shown on the cards, for the sessions the filters let through.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Summary {
    pub total: usize,
    /// Client connections, which count against max_connections.
    pub clients: usize,
    pub by_state: BTreeMap<State, usize>,
    pub blocked: usize,
    pub longest_query: Option<(i32, f64)>,
    pub oldest_transaction: Option<(i32, f64)>,
    /// Idle-in-transaction sessions older than a minute hold locks and bloat.
    pub stale_transactions: usize,
}

impl Summary {
    pub fn of<'a>(sessions: impl Iterator<Item = &'a Session>) -> Self {
        let mut summary = Summary::default();
        for s in sessions {
            summary.total += 1;
            summary.clients += usize::from(s.is_client());
            *summary.by_state.entry(s.state).or_default() += 1;
            summary.blocked += usize::from(!s.blocked_by.is_empty());
            if matches!(s.state, State::Active | State::Waiting)
                && s.is_client()
                && let Some(age) = s.query_age
                && summary.longest_query.is_none_or(|(_, longest)| age > longest)
            {
                summary.longest_query = Some((s.pid, age));
            }
            if let Some(age) = s.xact_age
                && summary.oldest_transaction.is_none_or(|(_, oldest)| age > oldest)
            {
                summary.oldest_transaction = Some((s.pid, age));
            }
            if s.state.in_transaction() && s.xact_age.is_some_and(|age| age > 60.0) {
                summary.stale_transactions += 1;
            }
        }
        summary
    }

    pub fn count(&self, state: State) -> usize {
        self.by_state.get(&state).copied().unwrap_or(0)
    }
}

/// One point of the timeline: sessions per state group.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Sample {
    pub active: usize,
    pub waiting: usize,
    pub idle_in_transaction: usize,
    pub idle: usize,
    /// Transactions per second since the previous sample.
    pub tps: Option<f64>,
}

#[derive(Default)]
pub struct History {
    pub samples: VecDeque<Sample>,
    last: Option<(std::time::Instant, f64)>,
}

impl History {
    pub fn push(&mut self, snapshot: &Snapshot) {
        let summary = Summary::of(snapshot.sessions.iter().filter(|s| s.is_client()));
        let tps = match (self.last, snapshot.at) {
            (Some((then, before)), Some(now)) => {
                let seconds = now.duration_since(then).as_secs_f64();
                // A counter reset (stats reset, restart) is not negative traffic.
                (seconds > 0.0 && snapshot.server.transactions >= before)
                    .then(|| (snapshot.server.transactions - before) / seconds)
            }
            _ => None,
        };
        if let Some(at) = snapshot.at {
            self.last = Some((at, snapshot.server.transactions));
        }
        self.samples.push_back(Sample {
            active: summary.count(State::Active) + summary.count(State::FastPath),
            waiting: summary.count(State::Waiting),
            idle_in_transaction: summary.count(State::IdleInTransaction) + summary.count(State::IdleInTransactionAborted),
            idle: summary.count(State::Idle),
            tps,
        });
        while self.samples.len() > HISTORY {
            self.samples.pop_front();
        }
    }

    pub fn tps(&self) -> Option<f64> {
        self.samples.back().and_then(|s| s.tps)
    }
}

/// A session and the sessions it blocks, for the blocking tree.
#[derive(Clone, Debug, PartialEq)]
pub struct BlockNode {
    pub pid: i32,
    pub children: Vec<BlockNode>,
}

impl BlockNode {
    pub fn count(&self) -> usize {
        self.children.iter().map(|c| 1 + c.count()).sum()
    }
}

/// Trees rooted at sessions that block others without being blocked
/// themselves; cycles (deadlocks before detection) get a root as well.
pub fn blocking_trees(sessions: &[Session]) -> Vec<BlockNode> {
    let mut blocks: HashMap<i32, Vec<i32>> = HashMap::new();
    let mut blocked: HashSet<i32> = HashSet::new();
    for s in sessions {
        for &blocker in &s.blocked_by {
            blocks.entry(blocker).or_default().push(s.pid);
            blocked.insert(s.pid);
        }
    }
    for children in blocks.values_mut() {
        children.sort_unstable();
        children.dedup();
    }
    fn build(pid: i32, blocks: &HashMap<i32, Vec<i32>>, seen: &mut HashSet<i32>) -> BlockNode {
        seen.insert(pid);
        let children = blocks
            .get(&pid)
            .into_iter()
            .flatten()
            .filter(|child| !seen.contains(child))
            .copied()
            .collect::<Vec<_>>()
            .into_iter()
            .map(|child| build(child, blocks, seen))
            .collect();
        BlockNode { pid, children }
    }
    let mut roots: Vec<i32> = blocks.keys().copied().filter(|pid| !blocked.contains(pid)).collect();
    roots.sort_unstable();
    let mut seen = HashSet::new();
    let mut trees: Vec<BlockNode> = roots.into_iter().map(|pid| build(pid, &blocks, &mut seen)).collect();
    let mut cyclic: Vec<i32> = blocks.keys().copied().filter(|pid| !seen.contains(pid)).collect();
    cyclic.sort_unstable();
    for pid in cyclic {
        if !seen.contains(&pid) {
            trees.push(build(pid, &blocks, &mut seen));
        }
    }
    trees.sort_by_key(|tree| std::cmp::Reverse(tree.count()));
    trees
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Facet {
    State,
    User,
    Application,
    Database,
    Wait,
}

impl Facet {
    pub const ALL: [Facet; 5] = [Facet::State, Facet::User, Facet::Application, Facet::Database, Facet::Wait];

    pub fn label(self) -> &'static str {
        match self {
            Facet::State => "State",
            Facet::User => "User",
            Facet::Application => "Application",
            Facet::Database => "Database",
            Facet::Wait => "Wait event",
        }
    }

    pub fn value(self, s: &Session) -> String {
        let value = match self {
            Facet::State => s.state.label().to_string(),
            Facet::User => s.user.clone(),
            Facet::Application => s.application.clone(),
            Facet::Database => s.database.clone(),
            Facet::Wait => s.wait.clone(),
        };
        if value.is_empty() { "(none)".into() } else { value }
    }
}

/// Values of a facet with their session counts, most frequent first.
pub fn breakdown<'a>(facet: Facet, sessions: impl Iterator<Item = &'a Session>) -> Vec<(String, usize)> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for s in sessions {
        *counts.entry(facet.value(s)).or_default() += 1;
    }
    let mut list: Vec<(String, usize)> = counts.into_iter().collect();
    list.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    list
}

/// "850 ms", "12.4 s", "3 min 05 s", "2 h 10 min", "4 d 3 h".
pub fn duration_text(seconds: f64) -> String {
    let s = seconds.max(0.0);
    if s < 1.0 {
        format!("{:.0} ms", s * 1000.0)
    } else if s < 60.0 {
        format!("{s:.1} s")
    } else if s < 3600.0 {
        format!("{} min {:02} s", (s / 60.0) as u64, (s % 60.0) as u64)
    } else if s < 86_400.0 {
        format!("{} h {:02} min", (s / 3600.0) as u64, ((s % 3600.0) / 60.0) as u64)
    } else {
        format!("{} d {} h", (s / 86_400.0) as u64, ((s % 86_400.0) / 3600.0) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(pid: i32, state: State, blocked_by: &[i32]) -> Session {
        Session {
            pid,
            database: "shop".into(),
            user: "app".into(),
            application: "api".into(),
            client: "10.0.0.1".into(),
            backend_type: "client backend".into(),
            state,
            wait_type: String::new(),
            wait_event: String::new(),
            backend_age: Some(100.0),
            xact_age: state.in_transaction().then_some(90.0),
            query_age: Some(5.0 + pid as f64),
            state_age: Some(1.0),
            blocked_by: blocked_by.to_vec(),
            query: "select 1".into(),
            xid: String::new(),
            xmin: String::new(),
            backend_start: String::new(),
            query_start: String::new(),
            wait: String::new(),
            query_line: String::new(),
        }
        .derive()
    }

    #[test]
    fn rows_parse_states_waits_and_blockers() {
        let row = |state: &str, wait: &str, blockers: &str| {
            let mut row = vec![Value::Null; 19];
            row[0] = Value::Int(42);
            row[5] = Value::Text("client backend".into());
            row[6] = Value::Text(state.into());
            row[7] = if wait.is_empty() { Value::Null } else { Value::Text(wait.into()) };
            row[11] = Value::Float(2.5);
            row[13] = Value::Text(blockers.into());
            row
        };
        let data = QueryResult {
            rows: vec![row("active", "Lock", "{7,9}"), row("active", "IO", "{}"), row("idle in transaction", "", "{}"), row("", "", "{}")],
            ..QueryResult::default()
        };
        let sessions = parse_sessions(&data);
        assert_eq!(sessions[0].state, State::Waiting);
        assert_eq!(sessions[0].blocked_by, [7, 9]);
        assert_eq!(sessions[0].query_age, Some(2.5));
        assert_eq!(sessions[1].state, State::Active, "I/O waits are normal work");
        assert_eq!(sessions[2].state, State::IdleInTransaction);
        assert_eq!(sessions[3].state, State::Background);
    }

    #[test]
    fn summary_counts_and_extremes() {
        let sessions = [
            session(1, State::Active, &[]),
            session(2, State::Waiting, &[3]),
            session(3, State::IdleInTransaction, &[]),
            session(4, State::Idle, &[]),
        ];
        let summary = Summary::of(sessions.iter());
        assert_eq!(summary.total, 4);
        assert_eq!(summary.blocked, 1);
        assert_eq!(summary.count(State::Waiting), 1);
        assert_eq!(summary.longest_query, Some((2, 7.0)));
        assert_eq!(summary.oldest_transaction, Some((3, 90.0)));
        assert_eq!(summary.stale_transactions, 1);
    }

    #[test]
    fn blocking_trees_nest_and_survive_cycles() {
        let sessions = vec![
            session(1, State::IdleInTransaction, &[]),
            session(2, State::Waiting, &[1]),
            session(3, State::Waiting, &[2]),
            session(4, State::Waiting, &[1]),
            session(10, State::Waiting, &[11]),
            session(11, State::Waiting, &[10]),
        ];
        let trees = blocking_trees(&sessions);
        assert_eq!(trees[0].pid, 1);
        assert_eq!(trees[0].count(), 3);
        assert_eq!(trees[0].children[0].children[0].pid, 3);
        assert_eq!(trees.len(), 2, "a cycle gets one root");
        assert_eq!(trees[1].count(), 1);
    }

    #[test]
    fn history_rates_ignore_counter_resets() {
        let mut history = History::default();
        let start = std::time::Instant::now();
        let snapshot = |transactions: f64, secs: u64| Snapshot {
            sessions: vec![session(1, State::Active, &[])],
            server: Server { transactions, ..Server::default() },
            at: Some(start + std::time::Duration::from_secs(secs)),
        };
        history.push(&snapshot(1000.0, 0));
        history.push(&snapshot(1100.0, 2));
        assert_eq!(history.tps(), Some(50.0));
        history.push(&snapshot(10.0, 4));
        assert_eq!(history.tps(), None);
        assert_eq!(history.samples.back().unwrap().active, 1);
    }

    #[test]
    fn breakdowns_and_durations() {
        let sessions = [session(1, State::Active, &[]), session(2, State::Idle, &[]), session(3, State::Idle, &[])];
        assert_eq!(breakdown(Facet::State, sessions.iter())[0], ("idle".to_string(), 2));
        assert_eq!(duration_text(0.25), "250 ms");
        assert_eq!(duration_text(12.44), "12.4 s");
        assert_eq!(duration_text(185.0), "3 min 05 s");
        assert_eq!(duration_text(7800.0), "2 h 10 min");
        assert_eq!(duration_text(360_000.0), "4 d 4 h");
    }
}
