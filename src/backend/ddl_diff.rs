//! Read-only DDL snapshots use dedicated connections, never console sessions.

use super::{Backend, Event};
use crate::{
    ddl_diff::{DdlDefinition, DdlObjectIdentity},
    drivers::{CancelHandle, Connection},
    model::{DataSource, DbObject, DriverKind, ObjectKind},
};
use futures_util::FutureExt;
use std::{
    panic::AssertUnwindSafe,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

pub struct DdlObjectRequest {
    pub ds: DataSource,
    pub identity: DdlObjectIdentity,
    pub node: DbObject,
}

pub struct StructureCompareRequest {
    pub request_id: u64,
    pub origin: DdlObjectRequest,
    pub target: DdlObjectRequest,
    pub cancelled: Arc<AtomicBool>,
}

#[derive(Default)]
pub(super) struct StructureWork {
    running: bool,
    active: Option<Arc<AtomicBool>>,
    pending: Option<StructureCompareRequest>,
}

impl StructureWork {
    fn enqueue(&mut self, request: StructureCompareRequest) -> (bool, Option<u64>) {
        if let Some(active) = &self.active {
            active.store(true, Ordering::Relaxed);
        }
        let replaced = self.pending.replace(request).map(|old| {
            old.cancelled.store(true, Ordering::Relaxed);
            old.request_id
        });
        let start = !self.running;
        self.running = true;
        (start, replaced)
    }

    fn next(&mut self) -> Option<StructureCompareRequest> {
        let request = self.pending.take();
        self.active = request.as_ref().map(|request| request.cancelled.clone());
        self.running = request.is_some();
        request
    }

    pub(super) fn cancel_all(&mut self) {
        if let Some(active) = &self.active {
            active.store(true, Ordering::Relaxed);
        }
        if let Some(pending) = self.pending.take() {
            pending.cancelled.store(true, Ordering::Relaxed);
        }
    }
}

const CANCELLED: &str = "Structure comparison cancelled.";
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);
const CANCEL_POLL: Duration = Duration::from_millis(25);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);

impl Backend {
    pub fn compare_structure(&self, request: StructureCompareRequest) {
        let send = self.send();
        let (start, replaced) = self.structure_work.lock().unwrap().enqueue(request);
        if let Some(request_id) = replaced {
            send(Event::StructureCompareDone {
                request_id,
                result: Err(CANCELLED.into()),
            });
        }
        if !start {
            return;
        }
        let work = self.structure_work.clone();
        self.rt.spawn(async move {
            loop {
                let Some(request) = work.lock().unwrap().next() else {
                    break;
                };
                let request_id = request.request_id;
                let cancelled = request.cancelled.clone();
                let result = AssertUnwindSafe(compare_request(request))
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|_| Err("Could not complete the structure comparison.".into()));
                let result = if cancelled.load(Ordering::Relaxed) {
                    Err(CANCELLED.into())
                } else {
                    result
                };
                send(Event::StructureCompareDone {
                    request_id,
                    result: result.map(Box::new),
                });
            }
        });
    }
}

fn validate_object(request: &DdlObjectRequest) -> Result<(), String> {
    let DdlObjectRequest { ds, identity, node } = request;
    if ds.id != identity.ds_id || ds.identity() != identity.source {
        return Err("The data source definition has changed. Choose the object again.".into());
    }
    if node.kind != identity.kind
        || node.path != identity.path
        || node.path.last() != Some(&node.name)
    {
        return Err("The selected object identity no longer matches its path and kind.".into());
    }
    if !matches!(node.kind, ObjectKind::Table | ObjectKind::View) {
        return Err("DDL Diff supports tables and views only.".into());
    }
    let expected_path = if ds.driver == DriverKind::Postgres {
        3
    } else {
        2
    };
    if node.path.len() != expected_path
        || node
            .path
            .iter()
            .any(|part| part.is_empty() || part.contains('\0'))
    {
        return Err(
            "The selected object does not have a complete database/schema/name path.".into(),
        );
    }
    if ds.driver == DriverKind::Postgres && node.path[0] != ds.database {
        return Err(
            "The selected object belongs to a different physical PostgreSQL database.".into(),
        );
    }
    if ds.driver == DriverKind::Sqlite
        && (node.path[0] != "main" || ds.file.is_empty() || ds.file == ":memory:")
    {
        return Err("DDL Diff requires a persistent SQLite file and an object in main; temporary and attached databases are not supported.".into());
    }
    Ok(())
}

async fn compare_request(
    request: StructureCompareRequest,
) -> Result<crate::ddl_diff::ComparedDdl, String> {
    let StructureCompareRequest {
        origin,
        target,
        cancelled,
        ..
    } = request;
    if cancelled.load(Ordering::Relaxed) {
        return Err(CANCELLED.into());
    }
    validate_object(&origin).map_err(|error| format!("Origin: {error}"))?;
    validate_object(&target).map_err(|error| format!("Target: {error}"))?;
    if origin.ds.driver != target.ds.driver || origin.node.kind != target.node.kind {
        return Err("Choose two objects of the same kind and database driver.".into());
    }
    let stop_sibling = Arc::new(AtomicBool::new(false));
    let deadline = tokio::time::Instant::now() + FETCH_TIMEOUT;
    let (origin, target) = tokio::join!(
        fetch_definition(origin, &cancelled, &stop_sibling, deadline),
        fetch_definition(target, &cancelled, &stop_sibling, deadline),
    );
    if cancelled.load(Ordering::Relaxed) {
        return Err(CANCELLED.into());
    }
    // Prefer the original failure over cancellation of its sibling.
    if let Err(error) = &origin
        && error != CANCELLED
    {
        return Err(format!("Origin: {error}"));
    }
    if let Err(error) = &target
        && error != CANCELLED
    {
        return Err(format!("Target: {error}"));
    }
    let (origin, target) = (origin?, target?);
    tokio::task::spawn_blocking(move || crate::ddl_diff::compare_ddl(origin, target, &cancelled))
        .await
        .map_err(|_| "Could not prepare the DDL comparison.".to_string())?
}

async fn stop_reason(
    cancelled: &AtomicBool,
    sibling: &AtomicBool,
    deadline: tokio::time::Instant,
) -> String {
    loop {
        if cancelled.load(Ordering::Relaxed) || sibling.load(Ordering::Relaxed) {
            return CANCELLED.into();
        }
        if tokio::time::Instant::now() >= deadline {
            return "Reading DDL timed out after 15 seconds.".into();
        }
        tokio::time::sleep_until((tokio::time::Instant::now() + CANCEL_POLL).min(deadline)).await;
    }
}

async fn fetch_definition(
    request: DdlObjectRequest,
    cancelled: &AtomicBool,
    sibling: &AtomicBool,
    deadline: tokio::time::Instant,
) -> Result<DdlDefinition, String> {
    let result = fetch_inner(request, cancelled, sibling, deadline).await;
    if result.is_err() {
        sibling.store(true, Ordering::Relaxed);
    }
    result
}

async fn fetch_inner(
    request: DdlObjectRequest,
    cancelled: &AtomicBool,
    sibling: &AtomicBool,
    deadline: tokio::time::Instant,
) -> Result<DdlDefinition, String> {
    let DdlObjectRequest {
        mut ds,
        identity,
        node,
    } = request;
    // Keep the captured source identity unchanged; only this dedicated
    // connection is forced read-only. SQLite therefore never uses CREATE.
    ds.read_only = true;
    let mut conn = tokio::select! {
        biased;
        reason = stop_reason(cancelled, sibling, deadline) => return Err(reason),
        result = Connection::open(&ds) => result.map_err(super::fmt_err)?,
    };
    let mut cancel_handle = OperationCancellation(conn.cancel_handle());
    let (result, interrupted) = tokio::select! {
        biased;
        reason = stop_reason(cancelled, sibling, deadline) => (Err(reason), true),
        result = conn.ddl_for_comparison(&node) => (result.map_err(super::fmt_err), false),
    };
    if interrupted {
        if let Some(handle) = cancel_handle.0.take() {
            let _ = tokio::time::timeout(CLEANUP_TIMEOUT, handle.cancel()).await;
        }
        if ds.driver == DriverKind::Postgres {
            let _ = tokio::time::timeout(CLEANUP_TIMEOUT, conn.execute_control("ROLLBACK")).await;
        }
    }
    cancel_handle.0 = None;
    drop(conn);
    let (sql, coverage) = result?;
    Ok(DdlDefinition {
        identity,
        sql,
        coverage,
    })
}

struct OperationCancellation(Option<CancelHandle>);

impl Drop for OperationCancellation {
    fn drop(&mut self) {
        // Runtime shutdown may drop this future before its cancellation poll.
        // SQLite work remains on the blocking pool, so interrupt it directly.
        // Network connections are dedicated and close when the future drops.
        if let Some(CancelHandle::Sqlite(handle)) = &self.0 {
            handle.interrupt();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(driver: DriverKind, name: &str) -> DdlObjectRequest {
        let mut ds = DataSource::new(driver);
        ds.database = "catalog".into();
        ds.file = "/not-opened-by-unit-tests.sqlite".into();
        let parent: Vec<String> = match driver {
            DriverKind::Postgres => vec!["catalog".into(), "public".into()],
            DriverKind::MySql => vec!["other_schema".into()],
            DriverKind::Sqlite => vec!["main".into()],
        };
        let mut node = DbObject::new(ObjectKind::Table, name, &parent);
        node.children = None;
        let identity = DdlObjectIdentity {
            ds_id: ds.id.clone(),
            source: ds.identity(),
            kind: node.kind,
            path: node.path.clone(),
        };
        DdlObjectRequest { ds, identity, node }
    }

    fn request(id: u64) -> StructureCompareRequest {
        StructureCompareRequest {
            request_id: id,
            origin: object(DriverKind::Sqlite, "origin"),
            target: object(DriverKind::Sqlite, "target"),
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    #[test]
    fn identity_guards_source_kind_name_and_full_path_before_io() {
        for driver in [DriverKind::Postgres, DriverKind::MySql, DriverKind::Sqlite] {
            assert!(validate_object(&object(driver, "quoted ' \" name λ")).is_ok());
            let mut changed = object(driver, "table");
            changed.ds.host.push_str(".other");
            assert!(
                validate_object(&changed)
                    .unwrap_err()
                    .contains("data source definition")
            );
            let mut changed = object(driver, "table");
            changed.identity.ds_id.push_str("other");
            assert!(validate_object(&changed).is_err());
            let mut changed = object(driver, "table");
            changed.node.name = "different".into();
            assert!(validate_object(&changed).unwrap_err().contains("identity"));
            let mut changed = object(driver, "table");
            changed.node.kind = ObjectKind::View;
            assert!(validate_object(&changed).is_err());
            let mut changed = object(driver, "table");
            changed.identity.path[0].push_str("other");
            assert!(validate_object(&changed).is_err());
        }
    }

    #[test]
    fn namespace_validation_distinguishes_physical_database_and_schema() {
        let mut pg = object(DriverKind::Postgres, "table");
        pg.ds.database = "different".into();
        pg.identity.source = pg.ds.identity();
        assert!(
            validate_object(&pg)
                .unwrap_err()
                .contains("physical PostgreSQL database")
        );
        // MySQL's selected object may belong to another schema than its default.
        assert!(validate_object(&object(DriverKind::MySql, "table")).is_ok());
        for schema in ["temp", "attached"] {
            let mut sqlite = object(DriverKind::Sqlite, "table");
            sqlite.node.path[0] = schema.into();
            sqlite.identity.path = sqlite.node.path.clone();
            assert!(
                validate_object(&sqlite)
                    .unwrap_err()
                    .contains("persistent SQLite")
            );
        }
        for file in ["", ":memory:"] {
            let mut sqlite = object(DriverKind::Sqlite, "table");
            sqlite.ds.file = file.into();
            sqlite.identity.source = sqlite.ds.identity();
            assert!(validate_object(&sqlite).is_err());
        }
        let mut folder = object(DriverKind::Sqlite, "table");
        folder.node.path[0] = "\0tables".into();
        folder.identity.path = folder.node.path.clone();
        assert!(validate_object(&folder).unwrap_err().contains("complete"));
    }

    #[test]
    fn unsupported_kind_and_incomplete_paths_are_rejected() {
        let mut unsupported = object(DriverKind::Sqlite, "table");
        unsupported.node.kind = ObjectKind::Schema;
        unsupported.identity.kind = ObjectKind::Schema;
        assert!(
            validate_object(&unsupported)
                .unwrap_err()
                .contains("tables and views")
        );
        for path in [
            vec![],
            vec!["table".into()],
            vec!["".into(), "table".into()],
        ] {
            let mut incomplete = object(DriverKind::Sqlite, "table");
            incomplete.node.path = path;
            incomplete.identity.path = incomplete.node.path.clone();
            assert!(validate_object(&incomplete).is_err());
        }
    }

    #[test]
    fn queue_replaces_pending_request_and_cancels_active_work() {
        let mut work = StructureWork::default();
        let first = request(1);
        let first_cancel = first.cancelled.clone();
        assert_eq!(work.enqueue(first), (true, None));
        let active = work.next().unwrap();
        let second = request(2);
        let pending_weak = Arc::downgrade(&second.cancelled);
        assert_eq!(work.enqueue(second), (false, None));
        assert!(first_cancel.load(Ordering::Relaxed));
        assert_eq!(work.enqueue(request(3)), (false, Some(2)));
        assert!(pending_weak.upgrade().is_none());
        drop(active);
        assert_eq!(work.next().unwrap().request_id, 3);
        assert!(work.next().is_none());
        assert_eq!(work.enqueue(request(4)), (true, None));
        assert_eq!(work.enqueue(request(5)), (false, Some(4)));
        assert_eq!(work.next().unwrap().request_id, 5);
    }

    #[test]
    fn shutdown_cancels_and_releases_pending_structure_request() {
        let backend = Backend::new(egui::Context::default());
        let first = request(1);
        let active_cancel = first.cancelled.clone();
        let pending = request(2);
        let pending_cancel = pending.cancelled.clone();
        let active = {
            let mut work = backend.structure_work.lock().unwrap();
            work.enqueue(first);
            let active = work.next().unwrap();
            work.enqueue(pending);
            active
        };
        active_cancel.store(false, Ordering::Relaxed);
        drop(backend);
        assert!(active_cancel.load(Ordering::Relaxed));
        assert!(pending_cancel.load(Ordering::Relaxed));
        assert_eq!(Arc::strong_count(&pending_cancel), 1);
        drop(active);
    }

    #[test]
    fn rejected_or_cancelled_requests_emit_only_their_own_ack_without_sessions() {
        let backend = Backend::new(egui::Context::default());
        let cancelled = request(10);
        cancelled.cancelled.store(true, Ordering::Relaxed);
        backend.compare_structure(cancelled);
        let Event::StructureCompareDone { request_id, result } =
            backend.rx.recv_timeout(Duration::from_secs(10)).unwrap()
        else {
            panic!("unexpected event");
        };
        assert_eq!(request_id, 10);
        assert_eq!(result.unwrap_err(), CANCELLED);
        let mut invalid = request(11);
        invalid.target.identity.source.file.push_str("changed");
        backend.compare_structure(invalid);
        let Event::StructureCompareDone { request_id, result } =
            backend.rx.recv_timeout(Duration::from_secs(10)).unwrap()
        else {
            panic!("unexpected event");
        };
        assert_eq!(request_id, 11);
        assert!(
            result
                .unwrap_err()
                .starts_with("Target: The data source definition")
        );
        let mut mismatched = request(12);
        mismatched.target = object(DriverKind::Postgres, "other");
        backend.compare_structure(mismatched);
        let Event::StructureCompareDone { result, .. } =
            backend.rx.recv_timeout(Duration::from_secs(10)).unwrap()
        else {
            panic!("unexpected event");
        };
        assert!(
            result
                .unwrap_err()
                .contains("same kind and database driver")
        );
        assert!(backend.sessions.lock().unwrap().is_empty());
        assert!(backend.cancels.lock().unwrap().is_empty());
        assert!(backend.rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn cancellation_and_timeout_are_checked_before_waiting() {
        let cancelled = AtomicBool::new(true);
        let sibling = AtomicBool::new(false);
        assert_eq!(
            stop_reason(
                &cancelled,
                &sibling,
                tokio::time::Instant::now() + FETCH_TIMEOUT
            )
            .await,
            CANCELLED
        );
        cancelled.store(false, Ordering::Relaxed);
        sibling.store(true, Ordering::Relaxed);
        assert_eq!(
            stop_reason(
                &cancelled,
                &sibling,
                tokio::time::Instant::now() + FETCH_TIMEOUT
            )
            .await,
            CANCELLED
        );
        sibling.store(false, Ordering::Relaxed);
        assert!(
            stop_reason(&cancelled, &sibling, tokio::time::Instant::now())
                .await
                .contains("timed out")
        );
    }
}
