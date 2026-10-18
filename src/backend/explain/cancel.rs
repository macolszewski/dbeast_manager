//! Request-scoped cancellation. Only the worker owning the session sends a
//! database cancellation; no detached cancel can reach a subsequent statement.

use crate::drivers::ExplainError;
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

pub(crate) const RECOVERY_LIMIT: Duration = Duration::from_secs(5);

#[derive(Default)]
pub(crate) struct Token {
    cancelled: AtomicBool,
    wake: Notify,
}

impl Token {
    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.wake.notify_waiters();
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub(crate) async fn cancelled(&self) {
        let notified = self.wake.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !self.is_cancelled() {
            notified.await;
        }
    }
}

pub(crate) async fn lock<'a, T>(
    token: &Token,
    mutex: &'a tokio::sync::Mutex<T>,
) -> Option<tokio::sync::MutexGuard<'a, T>> {
    tokio::select! {
        biased;
        _ = token.cancelled() => None,
        state = mutex.lock() => Some(state),
    }
}

struct Entry {
    request_id: u64,
    ds_id: String,
    token: Arc<Token>,
}

#[derive(Default)]
pub(crate) struct Registry(HashMap<u64, Entry>);

pub(crate) struct Registration {
    registry: Arc<Mutex<Registry>>,
    console_id: u64,
    request_id: u64,
}

impl Registry {
    pub(crate) fn register(
        registry: &Arc<Mutex<Self>>,
        ds_id: &str,
        console_id: u64,
        request_id: u64,
    ) -> Option<(Registration, Arc<Token>)> {
        let mut entries = registry.lock().unwrap();
        if entries.0.contains_key(&console_id) {
            return None;
        }
        let token = Arc::new(Token::default());
        entries.0.insert(
            console_id,
            Entry {
                request_id,
                ds_id: ds_id.into(),
                token: token.clone(),
            },
        );
        Some((
            Registration {
                registry: registry.clone(),
                console_id,
                request_id,
            },
            token,
        ))
    }

    pub(crate) fn cancel(&self, console_id: u64) -> bool {
        if let Some(entry) = self.0.get(&console_id) {
            entry.token.cancel();
            true
        } else {
            false
        }
    }

    pub(crate) fn cancel_session(&self, ds_id: &str, console_id: u64) {
        if let Some(entry) = self.0.get(&console_id).filter(|entry| entry.ds_id == ds_id) {
            entry.token.cancel();
        }
    }

    pub(crate) fn cancel_source(&self, ds_id: &str) {
        for entry in self.0.values().filter(|entry| entry.ds_id == ds_id) {
            entry.token.cancel();
        }
    }

    pub(crate) fn cancel_all(&self) {
        for entry in self.0.values() {
            entry.token.cancel();
        }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut registry = self.registry.lock().unwrap();
        if registry
            .0
            .get(&self.console_id)
            .is_some_and(|entry| entry.request_id == self.request_id)
        {
            registry.0.remove(&self.console_id);
        }
    }
}

pub(super) struct Stage<T> {
    pub result: Result<T, ExplainError>,
    pub started: bool,
    /// A cancellation packet may arrive late, or the operation did not drain.
    pub close_reason: Option<String>,
}

impl<T> Stage<T> {
    fn completed(result: Result<T, ExplainError>) -> Self {
        let close_reason = result
            .as_ref()
            .err()
            .filter(|error| error.uncertain)
            .map(|_| "The database did not confirm the operation's outcome.".to_owned());
        Self {
            result,
            started: true,
            close_reason,
        }
    }
}

/// Setup may be between several control commands, so sending KILL here could
/// target a later command. Drop its future and poison the session instead.
pub(super) async fn setup<T, F>(token: &Token, operation: F) -> Stage<T>
where
    F: Future<Output = Result<T, ExplainError>>,
{
    if token.is_cancelled() {
        return Stage {
            result: Err(ExplainError::local("Cancelled before session setup.")),
            started: false,
            close_reason: None,
        };
    }
    tokio::select! {
        biased;
        result = operation => Stage::completed(result),
        _ = token.cancelled() => Stage {
            result: Err(ExplainError::local("Cancelled during session setup; the statement was not executed.")),
            started: true,
            close_reason: Some("Session setup was interrupted before its state could be confirmed.".into()),
        },
    }
}

pub(super) async fn run<T, F, C, CF>(token: &Token, operation: F, cancel: C) -> Stage<T>
where
    F: Future<Output = Result<T, ExplainError>>,
    C: FnOnce() -> CF,
    CF: Future<Output = anyhow::Result<()>>,
{
    run_with_limit(token, operation, cancel, RECOVERY_LIMIT).await
}

async fn run_with_limit<T, F, C, CF>(
    token: &Token,
    operation: F,
    cancel: C,
    limit: Duration,
) -> Stage<T>
where
    F: Future<Output = Result<T, ExplainError>>,
    C: FnOnce() -> CF,
    CF: Future<Output = anyhow::Result<()>>,
{
    if token.is_cancelled() {
        return Stage {
            result: Err(ExplainError::local(
                "Cancelled before executing the statement.",
            )),
            started: false,
            close_reason: None,
        };
    }
    tokio::pin!(operation);
    tokio::select! {
        biased;
        result = &mut operation => return Stage::completed(result),
        _ = token.cancelled() => {}
    }
    // Prefer a result that became ready with Stop over sending a late cancel.
    tokio::select! {
        biased;
        result = &mut operation => return Stage::completed(result),
        _ = std::future::ready(()) => {}
    }
    // From this point a packet may have been sent. Its send result is not an
    // acknowledgement from PostgreSQL: only the original query can confirm it.
    let cancellation = cancel();
    tokio::pin!(cancellation);
    let deadline = tokio::time::sleep(limit);
    tokio::pin!(deadline);
    let mut outcome = None;
    let mut sent = None;
    while outcome.is_none() || sent.is_none() {
        tokio::select! {
            biased;
            result = &mut operation, if outcome.is_none() => outcome = Some(result),
            result = &mut cancellation, if sent.is_none() => sent = Some(result),
            _ = &mut deadline => break,
        }
    }
    // A cancellation send may have made the original response ready in the
    // same poll as the deadline. Preserve that known result without waiting.
    if outcome.is_none() {
        tokio::select! {
            biased;
            result = &mut operation => outcome = Some(result),
            _ = std::future::ready(()) => {},
        }
    }
    let confirmed = outcome.as_ref().is_some_and(|result| {
        result
            .as_ref()
            .is_err_and(|error| error.cancelled && !error.uncertain)
    }) && sent.as_ref().is_some_and(Result::is_ok);
    let close_reason = if confirmed {
        None
    } else {
        Some(if outcome.is_some() && sent.as_ref().is_some_and(Result::is_ok) {
            "Cancellation was sent, but the original statement did not confirm cancellation. A late cancellation could affect the next statement."
        } else {
            "Cancellation could not be completed and the database session could not be recovered within the recovery limit."
        }.to_owned())
    };
    Stage {
        result: outcome.unwrap_or_else(|| {
            Err(ExplainError::uncertain(
                "The database did not confirm the statement's outcome after Stop.",
            ))
        }),
        started: true,
        close_reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use tokio::sync::oneshot;

    fn cancelled_error() -> ExplainError {
        ExplainError {
            message: "cancelled by server".into(),
            cancelled: true,
            uncertain: false,
        }
    }

    #[tokio::test]
    async fn setup_cancellation_drops_pending_control_without_polling_user_sql() {
        let token = Token::default();
        let result = setup(&token, async {
            token.cancel();
            std::future::pending::<Result<(), ExplainError>>().await
        })
        .await;
        assert!(result.started);
        assert!(result.close_reason.is_some());
        assert!(result.result.unwrap_err().message.contains("not executed"));
        let polled = Cell::new(false);
        let result = setup(&token, async {
            polled.set(true);
            Ok(())
        })
        .await;
        assert!(!result.started);
        assert!(result.close_reason.is_none());
        assert!(!polled.get());
    }

    #[test]
    fn registry_deduplicates_and_removes_only_its_request() {
        let registry = Arc::new(Mutex::new(Registry::default()));
        let (old, first) = Registry::register(&registry, "alpha", 3, 10).unwrap();
        assert!(Registry::register(&registry, "beta", 3, 11).is_none());
        assert!(registry.lock().unwrap().cancel(3));
        assert!(first.is_cancelled());
        registry.lock().unwrap().0.remove(&3);
        let (new, second) = Registry::register(&registry, "alpha", 3, 11).unwrap();
        drop(old);
        assert!(!second.is_cancelled());
        assert!(registry.lock().unwrap().cancel(3));
        drop(new);
        assert!(!registry.lock().unwrap().cancel(3));
        assert_eq!(Arc::strong_count(&second), 1);
    }

    #[test]
    fn source_session_and_shutdown_cancellation_are_scoped() {
        let registry = Arc::new(Mutex::new(Registry::default()));
        let (_a, alpha) = Registry::register(&registry, "alpha", 1, 1).unwrap();
        let (_b, beta) = Registry::register(&registry, "beta", 2, 1).unwrap();
        registry.lock().unwrap().cancel_session("beta", 1);
        assert!(!alpha.is_cancelled());
        registry.lock().unwrap().cancel_source("alpha");
        assert!(alpha.is_cancelled());
        assert!(!beta.is_cancelled());
        registry.lock().unwrap().cancel_all();
        assert!(beta.is_cancelled());
    }

    #[tokio::test]
    async fn already_cancelled_never_polls_sql_or_sends_cancel() {
        let token = Token::default();
        token.cancel();
        let polled = Cell::new(false);
        let sent = Cell::new(false);
        let result = run(
            &token,
            async {
                polled.set(true);
                Ok::<_, ExplainError>(())
            },
            || async {
                sent.set(true);
                Ok(())
            },
        )
        .await;
        assert!(!result.started);
        assert!(result.close_reason.is_none());
        assert!(!polled.get());
        assert!(!sent.get());
        token.cancelled().await; // cancellation before creating the waiter is retained
    }

    #[tokio::test]
    async fn ready_success_wins_stop_and_never_sends_late_cancel() {
        let token = Token::default();
        let sent = Cell::new(false);
        let result = run(
            &token,
            async {
                token.cancel();
                Ok::<_, ExplainError>(42)
            },
            || async {
                sent.set(true);
                Ok(())
            },
        )
        .await;
        assert_eq!(result.result.unwrap(), 42);
        assert!(result.close_reason.is_none());
        assert!(!sent.get());
    }

    #[tokio::test]
    async fn cancel_between_prepare_and_execute_never_polls_execute() {
        let token = Token::default();
        let prepared = run(&token, async { Ok::<_, ExplainError>(17) }, || async {
            panic!("no cancel")
        })
        .await;
        assert_eq!(prepared.result.unwrap(), 17);
        token.cancel();
        let execute = Cell::new(false);
        let result = run(
            &token,
            async {
                execute.set(true);
                Ok::<_, ExplainError>(())
            },
            || async { panic!("no cancel") },
        )
        .await;
        assert!(!execute.get());
        assert!(!result.started);
    }

    #[tokio::test]
    async fn confirmed_server_cancel_drains_both_futures_and_permits_cleanup() {
        let token = Token::default();
        let (tx, rx) = oneshot::channel();
        let drained = Cell::new(false);
        let result = run(
            &token,
            async {
                token.cancel();
                rx.await.unwrap();
                drained.set(true);
                Err::<(), _>(cancelled_error())
            },
            || async {
                tx.send(()).unwrap();
                Ok(())
            },
        )
        .await;
        assert!(result.result.unwrap_err().cancelled);
        assert!(result.close_reason.is_none());
        assert!(drained.get());
    }

    #[tokio::test]
    async fn successful_sql_after_cancel_keeps_success_but_requires_close() {
        let token = Token::default();
        let (tx, rx) = oneshot::channel();
        let result = run(
            &token,
            async {
                token.cancel();
                rx.await.unwrap();
                Ok::<_, ExplainError>("committed once")
            },
            || async {
                tx.send(()).unwrap();
                Ok(())
            },
        )
        .await;
        assert_eq!(result.result.unwrap(), "committed once");
        assert!(
            result
                .close_reason
                .unwrap()
                .contains("did not confirm cancellation")
        );
    }

    #[tokio::test]
    async fn failed_cancel_with_unfinished_sql_has_unknown_outcome_and_drops_future() {
        struct DropFlag<'a>(&'a Cell<bool>);
        impl Drop for DropFlag<'_> {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }
        let token = Token::default();
        let dropped = Cell::new(false);
        let result = run_with_limit(
            &token,
            async {
                let _guard = DropFlag(&dropped);
                token.cancel();
                std::future::pending::<Result<(), ExplainError>>().await
            },
            || async { anyhow::bail!("transport failed") },
            Duration::ZERO,
        )
        .await;
        assert!(dropped.get());
        assert!(result.result.unwrap_err().uncertain);
        assert!(result.close_reason.is_some());
    }

    #[tokio::test]
    async fn known_sql_success_survives_cancel_transport_timeout() {
        let token = Token::default();
        let (tx, rx) = oneshot::channel();
        let result = run_with_limit(
            &token,
            async {
                token.cancel();
                rx.await.unwrap();
                Ok::<_, ExplainError>(1)
            },
            || async {
                tx.send(()).unwrap();
                std::future::pending::<anyhow::Result<()>>().await
            },
            Duration::ZERO,
        )
        .await;
        assert_eq!(result.result.unwrap(), 1);
        assert!(result.close_reason.is_some());
    }

    #[tokio::test]
    async fn transport_error_without_stop_also_requires_close() {
        let token = Token::default();
        let result = run(
            &token,
            async { Err::<(), _>(ExplainError::uncertain("connection lost")) },
            || async { panic!("no cancel") },
        )
        .await;
        assert!(result.result.unwrap_err().uncertain);
        assert!(result.close_reason.is_some());
    }
}

#[cfg(test)]
mod lock_tests {
    use super::*;

    #[tokio::test]
    async fn stop_while_waiting_returns_without_accessing_or_waiting_for_session() {
        let session = tokio::sync::Mutex::new(7);
        let held = session.lock().await;
        let token = Token::default();
        let wait = lock(&token, &session);
        tokio::pin!(wait);
        assert!(futures_util::poll!(&mut wait).is_pending());
        token.cancel();
        assert!(wait.await.is_none());
        // The original owner still owns the mutex; no DB operation was entered.
        assert_eq!(*held, 7);
    }
}
