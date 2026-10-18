//! The working session kept between runs: open tabs with their
//! text, filters and last results, the active tab and the explorer tree. Nothing
//! connects on start; saved data is shown until a tab is run again.

use super::results::{LogLine, ResultSet};
use super::{App, ServicesView, SourceState, TabKind};
use crate::config::Config;
use crate::model::{DbObject, Page, QueryResult, SchemaSelection};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::hash::Hasher;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

/// Output lines kept per tab.
const LOG_LIMIT: usize = 500;
/// How often changes are written while the application runs.
const SAVE_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub(super) struct Session {
    tabs: Vec<SavedTab>,
    active: usize,
    expanded: Vec<(String, Vec<String>)>,
    selected: Option<(String, Vec<String>)>,
    /// Explorer tree of each data source as last loaded.
    trees: BTreeMap<String, Vec<DbObject>>,
    /// Database Graph settings and pinned objects; never data.
    graph_layouts: BTreeMap<String, super::schema_graph::SavedGraphLayout>,
}

#[derive(Serialize, Deserialize)]
struct SavedTab {
    title: String,
    ds_id: String,
    #[serde(default)]
    database: Option<String>,
    #[serde(default)]
    schema: SchemaSelection,
    kind: TabKind,
    #[serde(default)]
    bound_query: Option<std::sync::Arc<crate::model::CapturedRelatedQuery>>,
    #[serde(default)]
    transposed: bool,
    #[serde(default)]
    record_view: bool,
    #[serde(default)]
    aggregate_view: bool,
    #[serde(default = "value_editor_default")]
    show_value_editor: bool,
    #[serde(default)]
    cursor: Option<Range<usize>>,
    #[serde(default)]
    folds: Vec<Range<usize>>,
    #[serde(default)]
    results: Vec<SavedResult>,
    /// Result shown in the Services panel; `None` for the output log.
    #[serde(default)]
    view: Option<usize>,
    #[serde(default)]
    log: Vec<LogLine>,
}

#[derive(Serialize, Deserialize)]
struct SavedResult {
    #[serde(default)]
    origin: Option<super::results::ResultOrigin>,
    title: String,
    sql: String,
    page: Page,
    #[serde(default)]
    sort: Option<(usize, bool)>,
    #[serde(default)]
    transposed: bool,
    #[serde(default)]
    record_view: bool,
    #[serde(default)]
    aggregate_view: bool,
    #[serde(default = "value_editor_default")]
    show_value_editor: bool,
    data: ResultJson,
}

/// Result rows serialized once and shared by the result, session snapshots
/// and the writer thread, so a save never copies them. `id` names this
/// serialization, which lets change detection skip the rows themselves.
#[derive(Clone)]
pub(super) struct ResultJson {
    id: u64,
    raw: Arc<RawValue>,
}

static NEXT_RESULT_JSON: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// Set while hashing a session: results contribute their `id`, not their rows.
    static FINGERPRINT: Cell<bool> = const { Cell::new(false) };
}

impl ResultJson {
    pub(super) fn of(data: &QueryResult) -> Option<Self> {
        let raw = RawValue::from_string(serde_json::to_string(data).ok()?).ok()?;
        Some(Self::new(raw))
    }

    fn new(raw: Box<RawValue>) -> Self {
        Self {
            id: NEXT_RESULT_JSON.fetch_add(1, Ordering::Relaxed),
            raw: Arc::from(raw),
        }
    }
}

impl Serialize for ResultJson {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if FINGERPRINT.get() {
            serializer.serialize_u64(self.id)
        } else {
            self.raw.serialize(serializer)
        }
    }
}

impl<'de> Deserialize<'de> for ResultJson {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Box::<RawValue>::deserialize(deserializer).map(Self::new)
    }
}

/// Hash of everything a save would write, with each result's rows replaced by
/// its identity. Equal hashes mean nothing needs serializing or writing.
fn fingerprint(session: &Session) -> Option<u64> {
    struct HashWriter(std::collections::hash_map::DefaultHasher);
    impl std::io::Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.write(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = HashWriter(Default::default());
    FINGERPRINT.set(true);
    let result = serde_json::to_writer(&mut writer, session);
    FINGERPRINT.set(false);
    result.ok()?;
    Some(writer.0.finish())
}

fn value_editor_default() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DataSource, DriverKind, ExecutionContext, RelatedDirection, Value};
    use std::sync::Arc;

    #[test]
    fn related_session_restores_typed_parameters_but_never_runtime_authority() {
        let (mut data, relation, identity, context) = crate::related::tests::fixture();
        data.rows[0] = vec![
            Value::Decimal("18446744073709551615".into()),
            Value::Bytes(vec![0, 255]),
        ];
        let query = Arc::new(
            crate::related::capture(
                &data,
                0,
                &relation,
                RelatedDirection::Referenced,
                identity,
                context,
            )
            .unwrap(),
        );
        let mut ds = DataSource::new(DriverKind::Sqlite);
        ds.file = query.source_identity.file.clone();
        ds.read_only = true;
        let ctx = egui::Context::default();
        let mut app = super::super::actions::tests::isolated_app(&ctx);
        app.config.data_sources.push(ds.clone());
        let index = app.add_tab(
            "Related: main.accounts".into(),
            ds.id.clone(),
            TabKind::Console {
                sql: "stale preview text".into(),
            },
        );
        app.tabs[index].bound_query = Some(query.clone());
        let mut result = ResultSet::new(
            "accounts".into(),
            crate::sqlgen::related_query(DriverKind::Sqlite, &query)
                .unwrap()
                .sql,
            data,
            Page { skip: 5, limit: 5 },
        );
        result.origin = Some(super::super::results::ResultOrigin {
            ds_id: ds.id.clone(),
            source: Some(ds.identity()),
            context: ExecutionContext::default(),
            bound_query: Some(query.clone()),
        });
        app.tabs[index].results.push(result);
        app.tabs[index].view = ServicesView::Result(0);
        let serialized = serde_json::to_string(&app.session()).unwrap();
        assert!(!serialized.contains("session_epoch"));
        let mut restored = super::super::actions::tests::isolated_app(&ctx);
        restored.config.data_sources.push(ds);
        restored.restore_session(serde_json::from_str(&serialized).unwrap());
        let tab = &restored.tabs[0];
        assert_eq!(tab.bound_query.as_ref().unwrap().values, query.values);
        let TabKind::Console { sql } = &tab.kind else {
            panic!("related console")
        };
        assert_eq!(
            *sql,
            crate::sqlgen::related_query(DriverKind::Sqlite, &query)
                .unwrap()
                .sql
        );
        let result = &tab.results[0];
        assert_eq!(result.page.skip, 5);
        assert_eq!(
            result
                .origin
                .as_ref()
                .unwrap()
                .bound_query
                .as_ref()
                .unwrap()
                .values,
            query.values
        );
        assert!(result.data.source.is_none());
        assert!(result.edit_metadata().is_none());
        assert!(restored.result_replay_blocked_reason(0, 0).is_none());
        assert!(tab.running.is_none());
    }

    /// App with `tabs` consoles, each with one 500 x 20 result.
    fn app_with_results(ctx: &egui::Context, tabs: usize) -> App {
        let mut app = super::super::actions::tests::isolated_app(ctx);
        let ds = DataSource::new(DriverKind::Sqlite);
        app.config.data_sources.push(ds.clone());
        for tab in 0..tabs {
            let index = app.add_tab(
                format!("console {tab}"),
                ds.id.clone(),
                TabKind::Console {
                    sql: "select * from synthetic".into(),
                },
            );
            let data = QueryResult {
                columns: (0..20)
                    .map(|c| crate::model::Column {
                        name: format!("column_{c}"),
                        type_name: "text".into(),
                    })
                    .collect(),
                rows: (0..500)
                    .map(|r| {
                        (0..20)
                            .map(|c| match c % 5 {
                                0 => Value::Int((tab * 500 + r) as i64),
                                1 => Value::Float(r as f64 / 7.0),
                                2 => Value::Decimal(format!("{r}.{c:04}")),
                                3 => Value::Bytes(vec![c as u8; 16]),
                                _ => Value::Text(format!(
                                    "user{r}@example.com synthetic note {tab}-{c}: a longer free text value as found in comment or description columns, repeated {r}"
                                )),
                            })
                            .collect()
                    })
                    .collect(),
                affected: None,
                truncated: false,
                elapsed: Duration::from_millis(1),
                context: None,
                source: None,
            };
            app.tabs[index].results.push(ResultSet::new(
                "synthetic".into(),
                "select * from synthetic".into(),
                data,
                Page::first(500),
            ));
            app.tabs[index].view = ServicesView::Result(0);
        }
        app
    }

    #[test]
    fn unchanged_session_is_detected_without_rows_and_writes_stay_ordered() {
        let ctx = egui::Context::default();
        let mut app = app_with_results(&ctx, 2);
        let first = fingerprint(&app.session()).unwrap();
        assert_eq!(fingerprint(&app.session()), Some(first));
        // Results are shared with snapshots, not copied per save.
        let json = app.tabs[0].results[0].data_json().unwrap().raw.clone();
        assert!(Arc::ptr_eq(&json, &app.session().tabs[0].results[0].data.raw));
        app.tabs[0].cursor = Some(1..2);
        let moved = fingerprint(&app.session()).unwrap();
        assert_ne!(moved, first);
        // New rows with identical metadata still count as a change.
        let data = (*app.tabs[1].results[0].data).clone();
        app.tabs[1].results[0] = ResultSet::new("synthetic".into(), "select * from synthetic".into(), data, Page::first(500));
        assert_ne!(fingerprint(&app.session()), Some(moved));

        let dir = std::env::temp_dir().join(format!("dbeast-session-writer-{}", uuid::Uuid::new_v4()));
        let path = dir.join("session.json");
        let mut writer = SessionWriter::default();
        for text in ["first", "second", "last"] {
            app.tabs[0].kind = TabKind::Console { sql: text.into() };
            writer.send(Job::Write(path.clone(), Box::new(app.session())));
        }
        let (done, finished) = mpsc::channel();
        writer.worker.as_ref().unwrap().send(Job::Flush(done)).unwrap();
        finished.recv().unwrap();
        let restored: Session = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(matches!(&restored.tabs[0].kind, TabKind::Console { sql } if sql == "last"));
        assert_eq!(restored.tabs[1].results[0].data.raw.get(), app.tabs[1].results[0].data_json().unwrap().raw.get());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(!writer.failed.load(Ordering::Relaxed));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    #[ignore = "release measurement of session saves; run alone, writes only to a temporary directory"]
    fn benchmark_session_save() {
        let dir = std::env::temp_dir().join(format!("dbeast-session-bench-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: run alone with --exact; nothing else in this process reads the variable.
        unsafe { std::env::set_var("DBEAST_CONFIG", dir.join("config.json")) };
        let ctx = egui::Context::default();
        for tabs in [10, 30] {
            let mut app = app_with_results(&ctx, tabs);
            app.save_session(true);
            app.finish_session();
            let size = std::fs::metadata(dir.join("session.json")).unwrap().len();
            let median = |times: &mut Vec<f64>| {
                times.sort_by(f64::total_cmp);
                times[times.len() / 2]
            };
            let (mut unchanged, mut changed) = (Vec::new(), Vec::new());
            for round in 0..21 {
                let start = Instant::now();
                app.save_session(true);
                unchanged.push(start.elapsed().as_secs_f64() * 1000.0);
                app.tabs[0].kind = TabKind::Console {
                    sql: format!("select {round}"),
                };
                let start = Instant::now();
                app.save_session(true);
                changed.push(start.elapsed().as_secs_f64() * 1000.0);
            }
            let start = Instant::now();
            app.finish_session();
            let flush = start.elapsed().as_secs_f64() * 1000.0;
            println!(
                "{tabs} tabs x 500 rows x 20 cols, session {:.1} MB: UI thread unchanged={:.3} ms, changed={:.3} ms (median of 21); final flush={flush:.1} ms",
                size as f64 / 1e6,
                median(&mut unchanged),
                median(&mut changed),
            );
        }
        std::fs::remove_dir_all(dir).ok();
    }
}

/// When the session was last saved and what was saved. Serializing and
/// writing happen on one background thread, in the order the saves were made.
#[derive(Default)]
pub(super) struct SessionWriter {
    /// Fingerprint of the last snapshot handed to the writer.
    saved: Option<u64>,
    at: Option<Instant>,
    worker: Option<mpsc::Sender<Job>>,
    /// Set by the writer when a write failed, so the next save tries again.
    failed: Arc<AtomicBool>,
}

enum Job {
    Write(PathBuf, Box<Session>),
    /// Acknowledged once every earlier write has finished.
    Flush(mpsc::Sender<()>),
}

impl SessionWriter {
    fn send(&mut self, job: Job) {
        let failed = self.failed.clone();
        let worker = self.worker.get_or_insert_with(|| {
            let (sender, receiver) = mpsc::channel();
            let spawned = std::thread::Builder::new()
                .name("session-writer".into())
                .spawn(move || run_writer(&receiver, &failed));
            if let Err(e) = spawned {
                eprintln!("session writer not started: {e}");
            }
            sender
        });
        if let Err(mpsc::SendError(job)) = worker.send(job) {
            // Without a writer thread the session is still saved, synchronously.
            self.worker = None;
            if let Job::Write(path, session) = job {
                save_to(&path, &session, &self.failed);
            }
        }
    }
}

fn run_writer(receiver: &mpsc::Receiver<Job>, failed: &AtomicBool) {
    let mut pending = None;
    while let Ok(mut job) = receiver.recv() {
        loop {
            match job {
                Job::Write(path, session) => pending = Some((path, session)),
                Job::Flush(done) => {
                    if let Some((path, session)) = pending.take() {
                        save_to(&path, &session, failed);
                    }
                    let _ = done.send(());
                }
            }
            // Snapshots queued behind a slow write supersede each other.
            match receiver.try_recv() {
                Ok(next) => job = next,
                Err(_) => break,
            }
        }
        if let Some((path, session)) = pending.take() {
            save_to(&path, &session, failed);
        }
    }
}

fn save_to(path: &std::path::Path, session: &Session, failed: &AtomicBool) {
    let result = serde_json::to_string(session)
        .map_err(std::io::Error::other)
        .and_then(|text| write(path, &text));
    if let Err(e) = result {
        eprintln!("session save failed: {e}");
        failed.store(true, Ordering::Relaxed);
    }
}

fn path() -> PathBuf {
    Config::data_dir().join("session.json")
}

/// The saved session, or `None` before the first one is written.
pub(super) fn load() -> Option<Session> {
    let text = std::fs::read_to_string(path()).ok()?;
    match serde_json::from_str(&text) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("session not restored: {e}");
            None
        }
    }
}

/// Session data is local to this machine and may hold query results, so the
/// file is readable only by the owner, like the config.
fn write(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    // Renaming keeps the previous session intact if writing is interrupted.
    std::fs::rename(&tmp, path)
}

/// Opened files are temporary data sources and are not restored.
fn kept(ds_id: &str) -> bool {
    !ds_id.starts_with("file:")
}

impl App {
    fn session(&self) -> Session {
        let tabs = self
            .tabs
            .iter()
            .filter(|t| kept(&t.ds_id))
            .map(|t| SavedTab {
                title: t.title.clone(),
                ds_id: t.ds_id.clone(),
                database: t.database.clone(),
                schema: t.schema.clone(),
                kind: t.kind.clone(),
                bound_query: t.bound_query.clone(),
                transposed: t.transposed,
                record_view: t.record_view,
                aggregate_view: t.aggregate_view,
                show_value_editor: t.show_value_editor,
                cursor: t.cursor.clone(),
                folds: t.folds.clone(),
                results: t
                    .results
                    .iter()
                    .filter_map(|rs| {
                        Some(SavedResult {
                            origin: rs.origin.clone(),
                            title: rs.title.clone(),
                            sql: rs.sql.clone(),
                            page: rs.page,
                            sort: rs.sort,
                            transposed: rs.transposed,
                            record_view: rs.record_view,
                            aggregate_view: rs.aggregate_view,
                            show_value_editor: rs.show_value_editor,
                            data: rs.data_json()?.clone(),
                        })
                    })
                    .collect(),
                view: match t.view {
                    ServicesView::Result(i) => Some(i),
                    ServicesView::Output | ServicesView::Plan => None,
                },
                log: t.log.recent(LOG_LIMIT),
            })
            .collect();
        let active = self
            .tabs
            .iter()
            .take(self.active)
            .filter(|t| kept(&t.ds_id))
            .count();
        let trees = self
            .sources
            .iter()
            .filter(|(id, _)| kept(id))
            .filter_map(|(id, st)| Some((id.clone(), st.roots.clone()?)))
            .collect();
        Session {
            tabs,
            active,
            expanded: self
                .expanded
                .iter()
                .filter(|(id, _)| kept(id))
                .cloned()
                .collect(),
            selected: self.selected.clone().filter(|(id, _)| kept(id)),
            trees,
            graph_layouts: self
                .graph_layouts
                .iter()
                .filter(|(key, _)| kept(key))
                .map(|(key, layout)| (key.clone(), layout.clone()))
                .collect(),
        }
    }

    pub(super) fn restore_session(&mut self, session: Session) {
        for (id, roots) in session.trees {
            if self.source(&id).is_some() {
                self.sources.insert(
                    id,
                    SourceState {
                        roots: Some(roots),
                        ..Default::default()
                    },
                );
            }
        }
        self.expanded = session
            .expanded
            .into_iter()
            .filter(|(id, _)| self.sources.contains_key(id))
            .collect();
        self.selected = session.selected;
        self.graph_layouts = session.graph_layouts;
        for saved in session.tabs {
            let i = self.add_tab(saved.title, saved.ds_id, saved.kind);
            self.tabs[i].database = saved.database;
            self.tabs[i].schema = saved.schema;
            self.tabs[i].bound_query = saved.bound_query;
            if let Some(query) = &self.tabs[i].bound_query
                && let Ok(generated) =
                    crate::sqlgen::related_query(query.source_identity.driver, query)
            {
                self.tabs[i].kind = TabKind::Console { sql: generated.sql };
            }
            self.update_metadata_key(i);
            let tab = &mut self.tabs[i];
            tab.refresh_on_activate = matches!(tab.kind, TabKind::Table { .. });
            tab.transposed = saved.transposed;
            tab.record_view = saved.record_view && !saved.aggregate_view;
            tab.aggregate_view = saved.aggregate_view;
            tab.show_value_editor = saved.show_value_editor;
            tab.cursor = saved.cursor;
            tab.folds = saved.folds;
            tab.log = super::results::OutputLog::from_lines(saved.log);
            for r in saved.results {
                let Ok(data) = serde_json::from_str::<QueryResult>(r.data.raw.get()) else {
                    continue;
                };
                let mut rs = ResultSet::new(r.title, r.sql, data, r.page);
                // The rows were just read from this text; the next save reuses it.
                rs.set_data_json(r.data);
                rs.origin = r.origin;
                rs.transposed = r.transposed;
                rs.record_view = r.record_view && !r.aggregate_view;
                rs.aggregate_view = r.aggregate_view;
                rs.show_value_editor = r.show_value_editor;
                rs.set_sort(r.sort);
                tab.results.push(rs);
            }
            tab.view = match saved.view {
                Some(v) if v < tab.results.len() => ServicesView::Result(v),
                _ => ServicesView::Output,
            };
        }
        self.active = session.active.min(self.tabs.len().saturating_sub(1));
    }

    /// Writes the session when it changed, at most every [`SAVE_INTERVAL`]
    /// unless `now` is set.
    pub(super) fn save_session(&mut self, now: bool) {
        if !self.config_loaded {
            return;
        }
        if !now
            && self
                .session_writer
                .at
                .is_some_and(|at| at.elapsed() < SAVE_INTERVAL)
        {
            return;
        }
        self.session_writer.at = Some(Instant::now());
        if self.session_writer.failed.swap(false, Ordering::Relaxed) {
            self.session_writer.saved = None;
        }
        let session = self.session();
        let Some(fingerprint) = fingerprint(&session) else {
            return;
        };
        if self.session_writer.saved == Some(fingerprint) {
            return;
        }
        self.session_writer.saved = Some(fingerprint);
        self.session_writer
            .send(Job::Write(path(), Box::new(session)));
    }

    /// Saves the session and waits until it is on disk; used when the
    /// application exits.
    pub(super) fn finish_session(&mut self) {
        self.save_session(true);
        let Some(worker) = &self.session_writer.worker else {
            return;
        };
        let (done, finished) = mpsc::channel();
        if worker.send(Job::Flush(done)).is_ok() {
            let _ = finished.recv();
        }
    }
}
