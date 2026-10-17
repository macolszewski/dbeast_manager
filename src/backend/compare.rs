//! A bounded worker queue for in-memory comparisons, independent of DB sessions.

use super::{Backend, Event};
use crate::compare::{CompareRequest, compare};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Default)]
pub(super) struct CompareWork {
    running: bool,
    active: Option<Arc<AtomicBool>>,
    pending: Option<CompareRequest>,
}

impl CompareWork {
    /// Only the newest pending snapshot is retained. No new blocking task is
    /// queued while a worker is active; cancellation never waits for that task.
    fn enqueue(&mut self, request: CompareRequest) -> (bool, Option<u64>) {
        if let Some(active) = &self.active {
            active.store(true, Ordering::Relaxed);
        }
        let replaced = self.pending.replace(request).map(|old| {
            old.cancelled.store(true, Ordering::Relaxed);
            old.request_id
        });
        let start_worker = !self.running;
        self.running = true;
        (start_worker, replaced)
    }

    fn next(&mut self) -> Option<CompareRequest> {
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

impl Backend {
    pub fn compare_data(&self, request: CompareRequest) {
        let send = self.send();
        let (start_worker, replaced) = self.compare_work.lock().unwrap().enqueue(request);
        if let Some(request_id) = replaced {
            send(Event::CompareDataDone {
                request_id,
                result: Err("Data comparison cancelled.".into()),
            });
        }
        if !start_worker {
            return;
        }
        let work = self.compare_work.clone();
        self.rt.spawn_blocking(move || {
            loop {
                let Some(request) = work.lock().unwrap().next() else {
                    break;
                };
                let request_id = request.request_id;
                let cancelled = request.cancelled.clone();
                // A failed worker must not leave the queue permanently marked
                // as running. Report one acknowledgement and drain the latest.
                let result = catch_unwind(AssertUnwindSafe(|| compare(request)))
                    .unwrap_or_else(|_| Err("Could not complete the data comparison.".into()));
                let result = if cancelled.load(Ordering::Relaxed) {
                    Err("Data comparison cancelled.".into())
                } else {
                    result
                };
                send(Event::CompareDataDone { request_id, result });
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        compare::{ColumnPair, CompareSpec},
        model::{Column, QueryResult, Value},
    };
    use std::time::Duration;

    fn request(id: u64) -> CompareRequest {
        let data = Arc::new(QueryResult {
            columns: vec![Column {
                name: "id".into(),
                type_name: "INTEGER".into(),
            }],
            rows: vec![vec![Value::Int(1)]],
            ..Default::default()
        });
        CompareRequest {
            request_id: id,
            left: data.clone(),
            right: data,
            spec: CompareSpec {
                columns: vec![ColumnPair { left: 0, right: 0 }],
                key_pairs: vec![0],
            },
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    #[test]
    fn queue_keeps_one_worker_and_only_the_latest_pending_snapshot() {
        let mut work = CompareWork::default();
        let first = request(1);
        let first_cancel = first.cancelled.clone();
        let first_weak = Arc::downgrade(&first.left);
        assert_eq!(work.enqueue(first), (true, None));
        let active = work.next().unwrap();
        let second = request(2);
        let second_cancel = second.cancelled.clone();
        let second_weak = Arc::downgrade(&second.left);
        assert_eq!(work.enqueue(second), (false, None));
        assert!(first_cancel.load(Ordering::Relaxed));
        assert_eq!(work.enqueue(request(3)), (false, Some(2)));
        assert!(second_cancel.load(Ordering::Relaxed));
        assert!(second_weak.upgrade().is_none());
        assert_eq!(work.pending.as_ref().unwrap().request_id, 3);
        assert!(first_weak.upgrade().is_some());
        drop(active);
        assert!(first_weak.upgrade().is_none());
        let latest = work.next().unwrap();
        assert_eq!(latest.request_id, 3);
        assert!(!latest.cancelled.load(Ordering::Relaxed));
        let latest_weak = Arc::downgrade(&latest.left);
        drop(latest);
        assert!(latest_weak.upgrade().is_none());
        assert!(work.next().is_none());
        assert!(!work.running);
        assert!(work.active.is_none());
        assert_eq!(work.enqueue(request(4)), (true, None));
    }

    #[test]
    fn replacement_before_worker_start_does_not_queue_another_worker() {
        let mut work = CompareWork::default();
        let first = request(1);
        let weak = Arc::downgrade(&first.left);
        assert_eq!(work.enqueue(first), (true, None));
        assert_eq!(work.enqueue(request(2)), (false, Some(1)));
        assert!(weak.upgrade().is_none());
        assert_eq!(work.next().unwrap().request_id, 2);
        assert!(work.next().is_none());
    }

    #[test]
    fn backend_drop_cancels_active_and_pending_work_before_runtime_shutdown() {
        let backend = Backend::new(egui::Context::default());
        // Control the queue directly so teardown cannot race a fast comparison.
        let first = request(1);
        let first_cancel = first.cancelled.clone();
        let second = request(2);
        let second_cancel = second.cancelled.clone();
        let pending_weak = Arc::downgrade(&second.left);
        let active = {
            let mut work = backend.compare_work.lock().unwrap();
            work.enqueue(first);
            let active = work.next().unwrap();
            work.enqueue(second);
            active
        };
        first_cancel.store(false, Ordering::Relaxed);
        drop(backend);
        assert!(first_cancel.load(Ordering::Relaxed));
        assert!(second_cancel.load(Ordering::Relaxed));
        assert!(pending_weak.upgrade().is_none());
        drop(active);
    }

    #[test]
    fn comparison_emits_one_event_without_database_sessions_or_cancel_handles() {
        let backend = Backend::new(egui::Context::default());
        let request = request(9);
        let weak = Arc::downgrade(&request.left);
        backend.compare_data(request);
        let event = backend.rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let Event::CompareDataDone { request_id, result } = event else {
            panic!("unexpected database event");
        };
        assert_eq!(request_id, 9);
        let result = result.unwrap();
        assert_eq!(result.counts.equal, 1);
        assert!(backend.sessions.lock().unwrap().is_empty());
        assert!(backend.cancels.lock().unwrap().is_empty());
        assert!(backend.rx.try_recv().is_err());
        drop(result);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn precancelled_comparison_acknowledges_and_releases_the_snapshot() {
        let backend = Backend::new(egui::Context::default());
        let request = request(10);
        let weak = Arc::downgrade(&request.left);
        request.cancelled.store(true, Ordering::Relaxed);
        backend.compare_data(request);
        let Event::CompareDataDone { request_id, result } =
            backend.rx.recv_timeout(Duration::from_secs(10)).unwrap()
        else {
            panic!("unexpected database event");
        };
        assert_eq!(request_id, 10);
        assert!(result.unwrap_err().contains("cancelled"));
        assert!(weak.upgrade().is_none());
        assert!(backend.sessions.lock().unwrap().is_empty());
    }
}
