//! The working session kept between runs: open tabs with their
//! text, filters and last results, the active tab and the explorer tree. Nothing
//! connects on start; saved data is shown until a tab is run again.

use super::results::{LogLine, ResultSet};
use super::{App, ServicesView, SourceState, TabKind};
use crate::config::Config;
use crate::model::{DbObject, Page, QueryResult, SchemaSelection};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::collections::{BTreeMap, HashSet};
use std::hash::Hasher;
use std::io::Write as _;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

/// Output lines kept per tab.
const LOG_LIMIT: usize = 500;
/// Decoded result rows kept in memory across all tabs; see
/// [`App::release_results_over`].
pub(super) const RESULTS_MEMORY: usize = 1 << 30;
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

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct SavedResult {
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
    /// Rows written inline by sessions before results had their own files.
    #[serde(default, skip_serializing)]
    data: Option<Box<RawValue>>,
    /// File in the results folder holding the rows, written once per result.
    #[serde(default)]
    file: Option<String>,
    /// Rows to write or just read; never part of session.json.
    #[serde(skip)]
    rows: Option<Arc<QueryResult>>,
}

/// Hash of everything a save would write to session.json. Rows are in their own
/// files, named once per result, so equal hashes mean nothing needs writing.
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
    serde_json::to_writer(&mut writer, session).ok()?;
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
        let snapshot = app.session();
        let serialized = serde_json::to_string(&snapshot).unwrap();
        let rows = snapshot.tabs[0].results[0].rows.clone().unwrap();
        assert!(!serialized.contains("session_epoch"));
        assert!(!serde_json::to_string(&*rows).unwrap().contains("session_epoch"));
        let mut parsed: Session = serde_json::from_str(&serialized).unwrap();
        // As `load` reads them back from their file.
        parsed.tabs[0].results[0].rows =
            Some(Arc::new(serde_json::from_str(&serde_json::to_string(&*rows).unwrap()).unwrap()));
        let mut restored = super::super::actions::tests::isolated_app(&ctx);
        restored.config.data_sources.push(ds);
        restored.restore_session(parsed);
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
        // Results are shared with snapshots, not copied or serialized per save.
        let snapshot = app.session();
        assert!(Arc::ptr_eq(
            &app.tabs[0].results[0].data,
            snapshot.tabs[0].results[0].rows.as_ref().unwrap()
        ));
        drop(snapshot);
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
        let flush = |writer: &SessionWriter| {
            let (done, finished) = mpsc::channel();
            writer.worker.as_ref().unwrap().send(Job::Flush(done)).unwrap();
            finished.recv().unwrap();
        };
        for text in ["first", "second", "last"] {
            app.tabs[0].kind = TabKind::Console { sql: text.into() };
            writer.send(Job::Write(path.clone(), Box::new(app.session())));
        }
        flush(&writer);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.len() < 20_000, "session.json holds no rows: {} bytes", text.len());
        let restored = load_from(&path).unwrap();
        assert!(matches!(&restored.tabs[0].kind, TabKind::Console { sql } if sql == "last"));
        let rows = |result: &SavedResult| {
            let rows = read_result_file(&results_dir(&path), result.file.as_deref().unwrap());
            serde_json::to_string(&rows.unwrap()).unwrap()
        };
        assert_eq!(rows(&restored.tabs[1].results[0]), serde_json::to_string(&*app.tabs[1].results[0].data).unwrap());
        assert_eq!(restored.tabs[1].results[0].file.as_ref(), Some(&app.tabs[1].results[0].session_file));
        let results = results_dir(&path);
        let file = results.join(&app.tabs[0].results[0].session_file);
        let written = std::fs::metadata(&file).unwrap().modified().unwrap();

        // A changed session keeps the file of unchanged rows and drops replaced ones.
        let replaced = app.tabs[1].results[0].session_file.clone();
        let data = (*app.tabs[1].results[0].data).clone();
        app.tabs[1].results[0] = ResultSet::new("synthetic".into(), "select 2".into(), data, Page::first(500));
        writer.send(Job::Write(path.clone(), Box::new(app.session())));
        flush(&writer);
        assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), written);
        assert!(!results.join(replaced).exists());
        assert!(results.join(&app.tabs[1].results[0].session_file).exists());
        assert_eq!(std::fs::read_dir(&results).unwrap().count(), 2);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for file in [path.clone(), file.clone()] {
                let mode = std::fs::metadata(&file).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o600);
            }
        }
        assert!(!writer.failed.load(Ordering::Relaxed));
        std::fs::remove_dir_all(dir).ok();
    }

    fn wait_for_stored(app: &mut App, index: usize) {
        for _ in 0..500 {
            app.load_stored_results();
            if app.tabs[index].stored.is_none() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("stored results of tab {index} were not read");
    }

    #[test]
    fn restored_results_are_read_when_their_tab_is_shown_and_keep_their_files() {
        let ctx = egui::Context::default();
        let dir = std::env::temp_dir().join(format!("dbeast-session-lazy-{}", uuid::Uuid::new_v4()));
        let path = dir.join("session.json");
        let mut app = app_with_results(&ctx, 3);
        app.config_loaded = true;
        app.session_writer.path = Some(path.clone());
        app.save_session(true);
        app.finish_session();
        let files: Vec<String> = app.tabs.iter().map(|t| t.results[0].session_file.clone()).collect();

        let mut session = load_from(&path).unwrap();
        assert!(session.tabs.iter().all(|t| t.results[0].rows.is_none()), "only metadata is read");
        session.active = 1;
        let mut restored = app_with_results(&ctx, 0);
        restored.config_loaded = true;
        restored.session_writer.path = Some(path.clone());
        restored.restore_session(session);
        assert!(restored.tabs.iter().all(|t| t.results.is_empty() && t.stored.is_some()));
        wait_for_stored(&mut restored, 1);
        assert_eq!(restored.tabs[1].results.len(), 1);
        assert!(matches!(restored.tabs[1].view, ServicesView::Result(0)));
        assert_eq!(
            serde_json::to_string(&*restored.tabs[1].results[0].data).unwrap(),
            serde_json::to_string(&*app.tabs[1].results[0].data).unwrap()
        );
        assert!(restored.tabs[0].stored.as_ref().is_some_and(|s| !s.is_loading()));
        assert!(restored.tabs[2].stored.as_ref().is_some_and(|s| !s.is_loading()));

        // Saving keeps the files of results that were never read.
        restored.tabs[0].cursor = Some(3..4);
        restored.save_session(true);
        restored.finish_session();
        for file in &files {
            assert!(results_dir(&path).join(file).exists(), "{file} kept");
        }
        let saved = load_from(&path).unwrap();
        assert_eq!(saved.tabs[2].results[0].file.as_ref(), Some(&files[2]));

        // Showing another tab reads its rows.
        restored.active = 2;
        wait_for_stored(&mut restored, 2);
        assert_eq!(restored.tabs[2].results[0].data.rows.len(), 500);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn results_over_the_memory_budget_go_back_to_their_files() {
        let ctx = egui::Context::default();
        let dir = std::env::temp_dir().join(format!("dbeast-session-budget-{}", uuid::Uuid::new_v4()));
        let path = dir.join("session.json");
        let mut app = app_with_results(&ctx, 4);
        app.config_loaded = true;
        app.session_writer.path = Some(path.clone());
        let one_tab = app.tabs[0].results[0].data_bytes;
        // Not written yet: nothing can be released.
        app.release_results_over(one_tab);
        assert!(app.tabs.iter().all(|t| t.stored.is_none()));
        app.save_session(true);
        app.finish_session();
        app.active = 3;
        app.tabs[1].results[0].edits.changed.insert((0, 4), crate::sqlgen::CellEdit::Null);
        let expected = serde_json::to_string(&*app.tabs[0].results[0].data).unwrap();
        let weak = Arc::downgrade(&app.tabs[0].results[0].data);
        app.tabs[0].last_active = Instant::now() - Duration::from_secs(60);
        let deadline = Instant::now() + Duration::from_secs(3);
        while app.tabs[0].stored.is_none() && Instant::now() < deadline {
            app.release_results_over(2 * one_tab);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(app.tabs[0].stored.is_some(), "the longest unused tab is released first");
        assert_eq!(weak.strong_count(), 0, "its rows are freed");
        assert!(app.tabs[1].stored.is_none(), "pending edits stay in memory");
        assert!(app.tabs[3].stored.is_none(), "the active tab stays in memory");
        let in_memory: usize = app.tabs.iter().flat_map(|t| &t.results).map(|rs| rs.data_bytes).sum();
        assert!(in_memory <= 2 * one_tab + one_tab, "{in_memory}");
        app.active = 0;
        wait_for_stored(&mut app, 0);
        assert_eq!(serde_json::to_string(&*app.tabs[0].results[0].data).unwrap(), expected);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn sessions_with_inline_rows_still_restore() {
        let ctx = egui::Context::default();
        let app = app_with_results(&ctx, 1);
        let dir = std::env::temp_dir().join(format!("dbeast-session-compat-{}", uuid::Uuid::new_v4()));
        let path = dir.join("session.json");
        let mut session = serde_json::to_value(app.session()).unwrap();
        let result = &mut session["tabs"][0]["results"][0];
        result.as_object_mut().unwrap().remove("file");
        result["data"] = serde_json::to_value(&*app.tabs[0].results[0].data).unwrap();
        // A damaged name never reaches outside the results folder.
        session["tabs"][0]["results"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"title": "bad", "sql": "", "page": app.tabs[0].results[0].page, "file": "../config.json"}));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, session.to_string()).unwrap();
        let restored = load_from(&path).unwrap();
        let results = &restored.tabs[0].results;
        assert!(results[0].file.is_none(), "rows get a new file on the next save");
        assert_eq!(
            serde_json::to_string(results[0].rows.as_deref().unwrap()).unwrap(),
            serde_json::to_string(&*app.tabs[0].results[0].data).unwrap()
        );
        assert!(results[1].rows.is_none());
        let mut restored_app = app_with_results(&ctx, 0);
        restored_app.restore_session(restored);
        assert_eq!(restored_app.tabs[0].results.len(), 1);
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
            let start = Instant::now();
            app.save_session(true);
            let first = start.elapsed().as_secs_f64() * 1000.0;
            app.finish_session();
            // Reading happens on a background thread; restoring on the UI thread.
            let start = Instant::now();
            let saved = load().unwrap();
            let read = start.elapsed().as_secs_f64() * 1000.0;
            let start = Instant::now();
            let mut restored = app_with_results(&ctx, 0);
            restored.restore_session(saved);
            let restore = start.elapsed().as_secs_f64() * 1000.0;
            assert_eq!(restored.tabs.len(), tabs);
            // The active tab's rows are read on a worker, then shown.
            let start = Instant::now();
            let active = restored.active;
            wait_for_stored(&mut restored, active);
            let active_ready = start.elapsed().as_secs_f64() * 1000.0;
            let size = std::fs::metadata(dir.join("session.json")).unwrap().len()
                + std::fs::read_dir(dir.join("session-results"))
                    .unwrap()
                    .flatten()
                    .map(|file| file.metadata().unwrap().len())
                    .sum::<u64>();
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
                "{tabs} tabs x 500 rows x 20 cols, session {:.1} MB: UI thread first save={first:.1} ms, unchanged={:.3} ms, changed={:.3} ms (median of 21); final flush={flush:.1} ms; background read={read:.1} ms, UI restore={restore:.1} ms, active tab rows ready after {active_ready:.1} ms",
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
    /// session.json of this application; tests point it at a temporary folder.
    path: Option<PathBuf>,
    release_check: Option<mpsc::Receiver<HashSet<String>>>,
    release_check_at: Option<Instant>,
}

impl SessionWriter {
    fn path(&self) -> PathBuf {
        self.path.clone().unwrap_or_else(path)
    }
}

enum Job {
    Write(PathBuf, Box<Session>),
    CheckResultFiles(PathBuf, Vec<String>, mpsc::Sender<HashSet<String>>, egui::Context),
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
                Job::CheckResultFiles(path, files, done, ctx) => {
                    // A check queued after a save must see the completed write.
                    if let Some((path, session)) = pending.take() {
                        save_to(&path, &session, failed);
                    }
                    let present = files.into_iter().filter(|name| result_file_exists(&path, name)).collect();
                    let _ = done.send(present);
                    ctx.request_repaint();
                }
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

/// Writes rows of results not yet on disk, then session.json, then removes
/// result files the new session no longer names. session.json is replaced
/// last, so it never names a file that is missing.
fn save_to(path: &Path, session: &Session, failed: &AtomicBool) {
    let result = (|| {
        let dir = results_dir(path);
        for result in session.tabs.iter().flat_map(|tab| &tab.results) {
            if let (Some(file), Some(rows)) = (&result.file, &result.rows) {
                let target = dir.join(file);
                if !target.exists() {
                    write_json(&target, rows.as_ref())?;
                }
            }
        }
        write_json(path, session)?;
        remove_unreferenced(&dir, session);
        Ok::<_, std::io::Error>(())
    })();
    if let Err(e) = result {
        eprintln!("session save failed: {e}");
        failed.store(true, Ordering::Relaxed);
    }
}

fn path() -> PathBuf {
    Config::data_dir().join("session.json")
}

fn results_dir(session: &Path) -> PathBuf {
    session.with_file_name("session-results")
}

/// A result file name written by this module; anything else in session.json
/// is ignored, so a damaged file cannot point outside the results folder.
fn result_file_name(name: &str) -> bool {
    name.strip_prefix("r-")
        .and_then(|rest| rest.strip_suffix(".json"))
        .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
}

pub(super) fn new_result_file() -> String {
    format!("r-{}.json", uuid::Uuid::new_v4())
}

fn remove_unreferenced(dir: &Path, session: &Session) {
    let kept: HashSet<&str> = session
        .tabs
        .iter()
        .flat_map(|tab| &tab.results)
        .filter_map(|result| result.file.as_deref())
        .collect();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let stale_temp = name
            .strip_suffix(".tmp")
            .is_some_and(result_file_name);
        if (result_file_name(name) && !kept.contains(name)) || stale_temp {
            std::fs::remove_file(entry.path()).ok();
        }
    }
}

/// The saved session with the rows of its results, or `None` before the first
/// one is written. Reads files as streams; meant for a background thread.
pub(super) fn exists() -> bool {
    path().exists()
}

pub(super) fn load() -> Option<Session> {
    load_from(&path())
}

fn load_from(path: &Path) -> Option<Session> {
    let file = std::fs::File::open(path).ok()?;
    let mut session: Session = match serde_json::from_reader(std::io::BufReader::new(file)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("session not restored: {e}");
            return None;
        }
    };
    for result in session.tabs.iter_mut().flat_map(|tab| &mut tab.results) {
        // An older session holds the rows inline; they get their own file on
        // the next save. Rows in files are read when their tab is shown.
        if let Some(raw) = result.data.take() {
            result.file = None;
            result.rows = serde_json::from_str::<QueryResult>(raw.get()).ok().map(Arc::new);
        }
    }
    Some(session)
}

/// Reads the session on a background thread; the result arrives on the receiver.
pub(super) fn load_in_background(ctx: egui::Context) -> mpsc::Receiver<Option<Session>> {
    let (sender, receiver) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("session-reader".into())
        .spawn({
            let sender = sender.clone();
            move || {
                let _ = sender.send(load());
                ctx.request_repaint();
            }
        });
    if spawned.is_err() {
        let _ = sender.send(load());
    }
    receiver
}

/// Session data is local to this machine and may hold query results, so the
/// files are readable only by the owner, like the config. Written to a
/// temporary file and renamed, so an interrupted write keeps the previous one.
fn write_json<T: Serialize + ?Sized>(path: &Path, value: &T) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut out = std::io::BufWriter::new(options.open(&tmp)?);
    serde_json::to_writer(&mut out, value).map_err(std::io::Error::other)?;
    out.flush()?;
    drop(out);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)
}

/// Opened files are temporary data sources and are not restored.
fn kept(ds_id: &str) -> bool {
    !ds_id.starts_with("file:")
}

/// Results of a tab kept in their session files instead of memory: restored
/// and not shown yet, or released to stay within the results memory budget.
/// They are read back when the tab is activated.
pub(super) struct StoredResults {
    results: Vec<SavedResult>,
    view: Option<usize>,
    loading: Option<mpsc::Receiver<Vec<Option<QueryResult>>>>,
}

impl StoredResults {
    pub(super) fn is_loading(&self) -> bool {
        self.loading.is_some()
    }
}

fn read_result_file(dir: &Path, name: &str) -> Option<QueryResult> {
    if !result_file_name(name) {
        return None;
    }
    let file = std::fs::File::open(dir.join(name)).ok()?;
    serde_json::from_reader(std::io::BufReader::new(file)).ok()
}

fn result_file_exists(session: &Path, name: &str) -> bool {
    result_file_name(name) && results_dir(session).join(name).exists()
}

/// A result rebuilt from its saved settings and rows.
fn saved_result(r: SavedResult, data: QueryResult) -> ResultSet {
    let mut rs = ResultSet::new(r.title, r.sql, data, r.page);
    // These rows are already in this file; the next save keeps it.
    if let Some(file) = r.file {
        rs.session_file = file;
    }
    rs.origin = r.origin;
    rs.transposed = r.transposed;
    rs.record_view = r.record_view && !r.aggregate_view;
    rs.aggregate_view = r.aggregate_view;
    rs.show_value_editor = r.show_value_editor;
    rs.set_sort(r.sort);
    rs
}

/// The settings of a result whose rows stay in its session file.
fn stored_result(rs: &ResultSet) -> SavedResult {
    SavedResult {
        origin: rs.origin.clone(),
        title: rs.title.clone(),
        sql: rs.sql.clone(),
        page: rs.page,
        sort: rs.sort,
        transposed: rs.transposed,
        record_view: rs.record_view,
        aggregate_view: rs.aggregate_view,
        show_value_editor: rs.show_value_editor,
        data: None,
        file: Some(rs.session_file.clone()),
        rows: None,
    }
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
                results: match &t.stored {
                    // Not read back yet: keep naming their files.
                    Some(stored) => stored.results.clone(),
                    None => t
                    .results
                    .iter()
                    .map(|rs| SavedResult {
                            origin: rs.origin.clone(),
                            title: rs.title.clone(),
                            sql: rs.sql.clone(),
                            page: rs.page,
                            sort: rs.sort,
                            transposed: rs.transposed,
                            record_view: rs.record_view,
                            aggregate_view: rs.aggregate_view,
                            show_value_editor: rs.show_value_editor,
                            data: None,
                            file: Some(rs.session_file.clone()),
                            rows: Some(rs.data.clone()),
                    })
                    .collect(),
                },
                view: match (&t.stored, t.view) {
                    (Some(stored), _) => stored.view,
                    (None, ServicesView::Result(i)) => Some(i),
                    (None, ServicesView::Output | ServicesView::Plan) => None,
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
            if saved.results.iter().any(|r| r.rows.is_some()) {
                // An older session with rows inline; results without rows are left out.
                for mut r in saved.results {
                    let Some(rows) = r.rows.take() else {
                        continue;
                    };
                    let data = Arc::try_unwrap(rows).unwrap_or_else(|rows| (*rows).clone());
                    tab.results.push(saved_result(r, data));
                }
                tab.view = match saved.view {
                    Some(v) if v < tab.results.len() => ServicesView::Result(v),
                    _ => ServicesView::Output,
                };
            } else {
                // Rows in their own files are read when the tab is shown.
                let results: Vec<SavedResult> = saved
                    .results
                    .into_iter()
                    .filter(|r| r.file.as_deref().is_some_and(result_file_name))
                    .collect();
                tab.view = ServicesView::Output;
                if !results.is_empty() {
                    tab.stored = Some(StoredResults {
                        view: saved.view.filter(|&v| v < results.len()),
                        results,
                        loading: None,
                    });
                }
            }
        }
        self.active = session.active.min(self.tabs.len().saturating_sub(1));
    }

    /// Reads back the stored results of the active tab, on a worker, and
    /// shows the results of any tab whose rows have been read.
    pub(super) fn load_stored_results(&mut self) {
        let active = self.active;
        let ctx = self.backend.ctx().clone();
        let session_path = self.session_writer.path();
        for (index, tab) in self.tabs.iter_mut().enumerate() {
            if index == active {
                tab.last_active = Instant::now();
            }
            let Some(stored) = &mut tab.stored else {
                continue;
            };
            if index == active && stored.loading.is_none() {
                let files: Vec<String> = stored
                    .results
                    .iter()
                    .map(|r| r.file.clone().unwrap_or_default())
                    .collect();
                let (sender, receiver) = mpsc::channel();
                let dir = results_dir(&session_path);
                let ctx = ctx.clone();
                let spawned = std::thread::Builder::new()
                    .name("session-results".into())
                    .spawn(move || {
                        let rows = files.iter().map(|name| read_result_file(&dir, name)).collect();
                        // The tab may have been closed or rerun meanwhile.
                        if sender.send(rows).is_ok() {
                            ctx.request_repaint();
                        }
                    });
                if spawned.is_ok() {
                    stored.loading = Some(receiver);
                }
            }
            let Some(loading) = &stored.loading else {
                continue;
            };
            let rows = match loading.try_recv() {
                Ok(rows) => rows,
                Err(mpsc::TryRecvError::Empty) => continue,
                Err(mpsc::TryRecvError::Disconnected) => Vec::new(),
            };
            let Some(stored) = tab.stored.take() else {
                continue;
            };
            let mut view = None;
            for (position, (saved, data)) in stored.results.into_iter().zip(rows).enumerate() {
                // A missing or damaged file leaves its result out.
                let Some(data) = data else {
                    continue;
                };
                if stored.view == Some(position) {
                    view = Some(tab.results.len());
                }
                tab.results.push(saved_result(saved, data));
            }
            tab.view = view.map_or(ServicesView::Output, ServicesView::Result);
        }
    }

    /// Keeps results in memory within `budget` ([`RESULTS_MEMORY`]): while over it, the
    /// clean results of the tab unused for longest go back to their session
    /// files, to be read again when the tab is shown. The active tab, running
    /// tabs, pending edits and rows not yet written to a file stay in memory.
    pub(super) fn release_results_over(&mut self, budget: usize) {
        let in_memory = |tab: &super::Tab| tab.results.iter().map(|rs| rs.data_bytes).sum::<usize>();
        let mut total: usize = self.tabs.iter().map(in_memory).sum();
        if total <= budget {
            return;
        }
        let mut candidates: Vec<usize> = (0..self.tabs.len())
            .filter(|&index| {
                let tab = &self.tabs[index];
                index != self.active
                    && kept(&tab.ds_id)
                    && !tab.is_running()
                    && tab.stored.is_none()
                    && !tab.results.is_empty()
                    && tab.results.iter().all(|rs| {
                        rs.edits.is_empty()
                            && !rs.is_editing()
                            && result_file_name(&rs.session_file)
                    })
            })
            .collect();
        if candidates.is_empty() { return; }
        let present = match self.session_writer.release_check.as_ref().map(mpsc::Receiver::try_recv) {
            Some(Ok(present)) => {
                self.session_writer.release_check = None;
                present
            }
            Some(Err(mpsc::TryRecvError::Empty)) => return,
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                self.session_writer.release_check = None;
                return;
            }
            None => {
                // Unsaved or failed files must not create a tight polling loop.
                let retry = Duration::from_secs(1);
                if let Some(at) = self.session_writer.release_check_at
                    && at.elapsed() < retry
                {
                    self.backend.ctx().request_repaint_after(retry - at.elapsed().min(retry));
                    return;
                }
                let files = candidates.iter().flat_map(|&index| &self.tabs[index].results)
                    .map(|rs| rs.session_file.clone()).collect();
                let (done, receiver) = mpsc::channel();
                self.session_writer.release_check = Some(receiver);
                self.session_writer.release_check_at = Some(Instant::now());
                self.session_writer.send(Job::CheckResultFiles(self.session_writer.path(), files, done, self.backend.ctx().clone()));
                return;
            }
        };
        self.backend.ctx().request_repaint_after(Duration::from_secs(1));
        candidates.retain(|&index| self.tabs[index].results.iter().all(|rs| present.contains(&rs.session_file)));
        candidates.sort_by_key(|&index| self.tabs[index].last_active);
        for index in candidates {
            if total <= budget {
                break;
            }
            let tab = &mut self.tabs[index];
            total -= in_memory(tab);
            let view = match tab.view {
                ServicesView::Result(i) => Some(i),
                ServicesView::Output | ServicesView::Plan => None,
            };
            let results = tab.results.drain(..).map(|rs| stored_result(&rs)).collect();
            tab.stored = Some(StoredResults {
                results,
                view,
                loading: None,
            });
            tab.view = ServicesView::Output;
        }
    }

    /// Writes the session when it changed, at most every [`SAVE_INTERVAL`]
    /// unless `now` is set.
    pub(super) fn save_session(&mut self, now: bool) {
        // Until the saved session is restored, saving would replace it.
        if !self.config_loaded || self.session_loading.is_some() {
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
            .send(Job::Write(self.session_writer.path(), Box::new(session)));
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
