//! Plans and actual analysis use the console session. Parsing never holds its lock.

pub(super) mod cancel;

use super::*;
use crate::drivers::ExplainError;
use crate::model::BoundQuery;
use crate::query_plan::ExplainMode;
use cancel::{Registry, Token};

pub struct ExplainRequest {
    pub console_id: u64,
    pub request_id: u64,
    pub ds: DataSource,
    pub sql: String,
    pub bound_query: Option<Arc<crate::model::CapturedRelatedQuery>>,
    pub context: ExecutionContext,
    pub mode: ExplainMode,
    pub manual_tx: bool,
}

/// The outcome of the analysed SQL, independent of parsing and session cleanup.
/// Plan does not execute the analysed instruction and always uses NotStarted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExplainExecution {
    NotStarted,
    Succeeded,
    Failed,
    Unknown,
}

#[derive(Debug)]
pub struct ExplainCompletion {
    pub ds_id: String,
    pub execution: ExplainExecution,
    /// None means the request was cancelled/rejected before observing the session.
    pub in_tx: Option<bool>,
    pub session_closed: bool,
    pub post_context: Option<ResolvedContext>,
}

impl ExplainCompletion {
    fn new(ds_id: String) -> Self {
        Self {
            ds_id,
            execution: ExplainExecution::NotStarted,
            in_tx: None,
            session_closed: false,
            post_context: None,
        }
    }
}

impl Backend {
    pub fn explain(&self, mut request: ExplainRequest) {
        let send = self.send();
        // Register synchronously: Stop must work before the task starts or takes
        // the session mutex. Duplicate requests cannot replace the live token.
        let Some((registration, token)) = Registry::register(
            &self.explain_requests,
            &request.ds.id,
            request.console_id,
            request.request_id,
        ) else {
            send(Event::ExplainDone {
                console_id: request.console_id,
                request_id: request.request_id,
                completion: ExplainCompletion::new(request.ds.id),
                result: Err("A query plan request is already running in this console.".into()),
            });
            return;
        };
        let session = self.session(&request.ds.id, &request.console_id.to_string());
        let sessions = self.sessions.clone();
        self.rt.spawn(async move {
            let ds = context_source(request.ds.clone(), &request.context);
            let driver = ds.driver;
            let mut completion = ExplainCompletion::new(ds.id.clone());
            // Validation is CPU work, outside the UI thread and session lock.
            let result =
                match prepare_query(&ds, &mut request) {
                    Err(error) => Err(error),
                    Ok(generated) => {
                        let state = cancel::lock(&token, &session).await;
                        match state {
                            None => Err("Cancelled before accessing the database session.".into()),
                            Some(mut state) => {
                                execute_plan(
                                    &mut state,
                                    &ds,
                                    &request,
                                    generated,
                                    &token,
                                    &mut completion,
                                )
                                .await
                            }
                        }
                    }
                };
            if completion.execution != ExplainExecution::NotStarted {
                // Never wait on another session while holding our own mutex.
                // Snapshot current sessions after execution, not when queued.
                let dependent: Vec<_> = sessions
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|((id, _), _)| id == &ds.id)
                    .map(|(_, session)| session.clone())
                    .collect();
                for session in dependent {
                    tokio::spawn(async move {
                        let mut state = session.lock().await;
                        state.edit_metadata.clear();
                        state.page_keys.clear();
                        state.related_metadata.clear();
                        if let Some(conn) = state.conn.as_mut() {
                            conn.clear_prepared_updates();
                        }
                    });
                }
            }
            // Stop during normalization never targets the now-idle connection.
            let result = match result {
                Ok(data) => {
                    let sql = request.sql;
                    let mode = request.mode;
                    tokio::task::spawn_blocking(move || {
                        crate::query_plan::parse_mode(driver, sql, data, mode)
                    })
                    .await
                    .map_err(|error| format!("Could not prepare the query plan: {error}"))
                }
                Err(error) => Err(error),
            };
            drop(registration);
            send(Event::ExplainDone {
                console_id: request.console_id,
                request_id: request.request_id,
                completion,
                result,
            });
        });
    }
}

/// The captured shape is authoritative; editor text never supplies bound SQL.
fn prepare_query(ds: &DataSource, request: &mut ExplainRequest) -> Result<BoundQuery, String> {
    let mut query = if let Some(captured) = &request.bound_query {
        super::related::validate_source(ds, captured).map_err(fmt_err)?;
        crate::sqlgen::related_query(ds.driver, captured)?
    } else {
        BoundQuery { sql: request.sql.clone(), params: Vec::new() }
    };
    request.sql = query.sql.clone();
    query.sql = crate::query_plan::explain_sql_mode(ds.driver, &query.sql, request.mode)?;
    Ok(query)
}

fn check_cancel(token: &Token) -> Result<(), ExplainError> {
    if token.is_cancelled() {
        Err(ExplainError::local(
            "Cancelled before executing the statement.",
        ))
    } else {
        Ok(())
    }
}

async fn send_cancel(handle: Option<CancelHandle>) -> anyhow::Result<()> {
    match handle {
        Some(handle) => handle.cancel().await,
        None => anyhow::bail!("This connection cannot cancel its current statement."),
    }
}

async fn execute_plan(
    state: &mut SessionState,
    ds: &DataSource,
    request: &ExplainRequest,
    generated: BoundQuery,
    token: &Token,
    completion: &mut ExplainCompletion,
) -> Result<QueryResult, String> {
    check_cancel(token).map_err(|error| error.message)?;
    completion.in_tx = Some(state.in_tx);
    let mut savepoint = None;
    let mut close_reason = None;
    let result: Result<QueryResult, ExplainError> = async {
        // Setup can perform network reads too. Stop must cover connection,
        // context and transaction preparation, not only PREPARE/EXECUTE.
        let setup = cancel::setup(token, async {
        state.ensure_usable().map_err(|error| ExplainError::local(fmt_err(error)))?;
        let fresh = state.conn.is_none();
        state.in_tx = state.conn(ds).await.map_err(|error| ExplainError::local(fmt_err(error)))?
            .has_open_transaction().await.map_err(|error| ExplainError::local(fmt_err(error)))?;
        check_cancel(token)?;
        if let Some(query) = &request.bound_query {
            if fresh {
                state.conn.as_mut().unwrap().apply_resolved_context(&query.context).await
                    .map_err(|error| ExplainError::local(fmt_err(error)))?;
            }
        } else {
            state.configure(ds, &request.context.schema).await.map_err(|error| ExplainError::local(fmt_err(error)))?;
        }
        check_cancel(token)?;
        if request.mode == ExplainMode::Analyse && request.manual_tx && !state.in_tx {
            state.conn.as_mut().unwrap().explain_control("BEGIN").await?;
            state.in_tx = true;
            check_cancel(token)?;
        }
        if state.in_tx {
            let name = format!("dbeast_explain_{}", uuid::Uuid::new_v4().simple());
            state.conn.as_mut().unwrap().explain_control(&format!("SAVEPOINT {name}")).await?;
            savepoint = Some(name);
            check_cancel(token)?;
        }
            Ok(())
        }).await;
        close_reason = setup.close_reason;
        setup.result?;
        if close_reason.is_some() {
            return Err(ExplainError::local("Session setup completed while cancellation was in flight; the statement was not executed."));
        }
        check_cancel(token)?;
        if ds.driver != DriverKind::Sqlite {
            let conn = state.conn.as_mut().unwrap();
            let handle = conn.cancel_handle();
            let prepared = cancel::run(token, conn.prepare_explain(&generated.sql, &generated.params, request.bound_query.as_ref().map(|query| &query.context)), || send_cancel(handle)).await;
            close_reason = prepared.close_reason;
            let prepared = prepared.result?;
            // A late cancel during prepare must not be followed by Execute.
            if close_reason.is_some() {
                return Err(ExplainError::local("Preparation completed while cancellation was in flight; the statement was not executed."));
            }
            check_cancel(token)?;
            if request.mode == ExplainMode::Analyse {
                state.edit_metadata.clear();
                state.page_keys.clear();
                state.related_metadata.clear();
                state.conn.as_mut().unwrap().clear_prepared_updates();
            }
            let conn = state.conn.as_mut().unwrap();
            let handle = conn.cancel_handle();
            let executed = cancel::run(token, conn.execute_explain(prepared, &generated.params, request.bound_query.as_ref().map(|query| &query.context)), || send_cancel(handle)).await;
            if request.mode == ExplainMode::Analyse && executed.started {
                completion.execution = if ds.driver == DriverKind::MySql
                    && executed.close_reason.is_some() && executed.result.is_ok() {
                    // MySQL can return a partial TREE with protocol success after
                    // KILL QUERY. It does not prove the iterators completed.
                    ExplainExecution::Unknown
                } else { execution_outcome(&executed.result) };
            }
            close_reason = executed.close_reason;
            executed.result
        } else {
            // SQLite keeps its prepared single-statement Plan path.
            let conn = state.conn.as_mut().unwrap();
            let handle = conn.cancel_handle();
            let query = generated;
            let result = cancel::run(token, async {
                conn.execute_bound_query(&query, Page::ALL, request.bound_query.as_ref().map(|query| &query.context)).await
                    .map_err(|error| ExplainError::from_error(error, ds.driver))
            }, || send_cancel(handle)).await;
            close_reason = result.close_reason;
            result.result
        }
    }.await;
    if close_reason.is_none() && result.as_ref().is_err_and(|error| error.uncertain) {
        close_reason =
            Some("The database connection could not confirm the operation's outcome.".into());
    }
    let mut result = result.map_err(|error| error.message);
    if close_reason.is_none()
        && let Some(name) = savepoint
    {
        let conn = state.conn.as_mut().unwrap();
        let cleanup = async {
            if result.is_err() {
                conn.explain_control(&format!("ROLLBACK TO SAVEPOINT {name}"))
                    .await?;
            }
            conn.explain_control(&format!("RELEASE SAVEPOINT {name}"))
                .await
        };
        match tokio::time::timeout(cancel::RECOVERY_LIMIT, cleanup).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                close_reason = Some(format!(
                    "EXPLAIN savepoint cleanup failed: {}",
                    error.message
                ))
            }
            Err(_) => {
                close_reason = Some("EXPLAIN savepoint cleanup exceeded the recovery limit.".into())
            }
        }
    }
    if close_reason.is_none()
        && state.reconnect_required.is_none()
        && let Some(conn) = state.conn.as_mut()
    {
        // Bound the entire post-execution read, not each round trip separately.
        // If context fails, never start another network read on the same session.
        let recovery = async {
            let context = if request.mode == ExplainMode::Analyse
                && completion.execution != ExplainExecution::NotStarted
            {
                Some(conn.explain_context().await?)
            } else {
                result.as_ref().ok().and_then(|data| data.context.clone())
            };
            let in_tx = conn
                .has_open_transaction()
                .await
                .map_err(|error| ExplainError::from_error(error, ds.driver))?;
            Ok::<_, ExplainError>((context, in_tx))
        };
        match tokio::time::timeout(cancel::RECOVERY_LIMIT, recovery).await {
            Ok(Ok((context, in_tx))) => {
                completion.post_context = context;
                state.in_tx = in_tx;
            }
            Ok(Err(error)) => close_reason = Some(format!("Could not confirm session context and transaction state after execution: {}", error.message)),
            Err(_) => close_reason = Some("Could not confirm session context and transaction state within the recovery limit.".into()),
        }
    }
    if let Some(reason) = close_reason {
        let message = uncertain_message(&reason, completion.execution);
        state.close_uncertain(message.clone());
        completion.session_closed = true;
        completion.post_context = None;
        result = Err(match result {
            Ok(_) => message,
            Err(primary) => format!("{primary}\n{message}"),
        });
    }
    completion.in_tx = Some(state.in_tx);
    result
}

fn execution_outcome<T>(result: &Result<T, ExplainError>) -> ExplainExecution {
    match result {
        Ok(_) => ExplainExecution::Succeeded,
        Err(error) if error.uncertain => ExplainExecution::Unknown,
        Err(_) => ExplainExecution::Failed,
    }
}

fn uncertain_message(reason: &str, execution: ExplainExecution) -> String {
    let outcome = match execution {
        ExplainExecution::Succeeded => {
            "The analysed statement completed successfully. Auto-mode effects may already be committed."
        }
        ExplainExecution::Unknown => {
            "The analysed statement's outcome is unknown. Auto-mode effects may already be committed; do not retry without checking."
        }
        ExplainExecution::Failed => "The database reported an error for the analysed statement.",
        ExplainExecution::NotStarted => "The analysed statement was not executed.",
    };
    format!(
        "{reason} {outcome} The session was closed; any uncommitted transaction may have been rolled back. Disconnect and reconnect the data source before continuing."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn captured_request() -> ExplainRequest {
        let (data, relation, _, context) = crate::related::tests::fixture();
        let mut ds = DataSource::new(DriverKind::Sqlite);
        ds.file = "/tmp/explain-unit-not-opened.sqlite".into();
        let captured = crate::related::capture(&data, 0, &relation,
            crate::model::RelatedDirection::Referenced, ds.identity(), context).unwrap();
        ExplainRequest { console_id: 1, request_id: 1, ds, sql: "DELETE FROM ignored".into(),
            bound_query: Some(Arc::new(captured)), context: ExecutionContext::default(),
            mode: ExplainMode::Plan, manual_tx: false }
    }

    #[test]
    fn captured_shape_preserves_types_without_interpolating_or_trusting_editor() {
        use crate::model::Value;
        for value in [Value::Int(i64::MAX), Value::Float(1.25), Value::Bool(true),
            Value::Decimal("12345678901234567890.123456789".into()),
            Value::Text("'; DROP TABLE accounts; -- Żółw".into()), Value::Bytes(vec![0, 255, 39])] {
            let mut request = captured_request();
            Arc::make_mut(request.bound_query.as_mut().unwrap()).values[0] = value.clone();
            let ds = request.ds.clone();
            let query = prepare_query(&ds, &mut request).unwrap();
            assert_eq!(query.params[0], value);
            assert!(!query.sql.contains("DROP TABLE"));
            assert!(!query.sql.contains("DELETE FROM"));
            assert!(request.sql.contains('?'));
        }
    }

    #[test]
    #[ignore = "manual release CPU measurement; no database connection"]
    fn benchmark_captured_explain_preparation() {
        let mut request = captured_request();
        let ds = request.ds.clone();
        let start = std::time::Instant::now();
        for _ in 0..100_000 {
            std::hint::black_box(prepare_query(&ds, &mut request).unwrap());
        }
        eprintln!("captured Explain validation + generation: {:.3} us/request (2 parameters, 100000 repetitions)", start.elapsed().as_secs_f64() * 10.0);
    }

    #[tokio::test]
    async fn poisoned_session_refuses_automatic_open_and_result_authority() {
        let ds = DataSource::new(DriverKind::Postgres);
        let mut state = SessionState::default();
        state.close_uncertain("The previous statement has an unknown outcome.".into());
        let error = match state.conn(&ds).await {
            Ok(_) => panic!("must not open"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("Disconnect and reconnect"));
        assert!(state.conn.is_none());
        assert!(state.source_identity.is_none());
        assert!(
            state
                .validate_result_session(&ds, &ds.identity(), 1)
                .is_err()
        );
        assert!(
            state
                .configure(&ds, &SchemaSelection::Default)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn cancelled_before_setup_preserves_unobserved_transaction_and_source() {
        let ds = DataSource::new(DriverKind::Postgres);
        let request = ExplainRequest {
            console_id: 1,
            request_id: 2,
            ds: ds.clone(),
            sql: "update sample set n=n+1".into(),
            bound_query: None,
            context: ExecutionContext::default(),
            mode: ExplainMode::Analyse,
            manual_tx: true,
        };
        let mut state = SessionState {
            in_tx: true,
            ..Default::default()
        };
        let token = Token::default();
        token.cancel();
        let mut completion = ExplainCompletion::new(ds.id.clone());
        assert!(
            execute_plan(
                &mut state,
                &ds,
                &request,
                BoundQuery { sql: "unused".into(), params: vec![] },
                &token,
                &mut completion
            )
            .await
            .is_err()
        );
        assert_eq!(completion.execution, ExplainExecution::NotStarted);
        assert_eq!(completion.in_tx, None);
        assert_eq!(completion.ds_id, ds.id);
        assert!(!completion.session_closed);
        assert!(state.in_tx);
        assert!(state.conn.is_none());
    }

    #[test]
    fn parsing_or_cleanup_does_not_relabel_the_sql_outcome() {
        assert_eq!(
            execution_outcome(&Ok::<_, ExplainError>(())),
            ExplainExecution::Succeeded
        );
        assert_eq!(
            execution_outcome::<()>(&Err(ExplainError::local("server error"))),
            ExplainExecution::Failed
        );
        assert_eq!(
            execution_outcome::<()>(&Err(ExplainError::uncertain("transport"))),
            ExplainExecution::Unknown
        );
        let success = uncertain_message("Cleanup failed.", ExplainExecution::Succeeded);
        assert!(success.contains("completed successfully"));
        assert!(success.contains("may already be committed"));
        assert!(!success.contains("was cancelled"));
        let unknown = uncertain_message("Lost connection.", ExplainExecution::Unknown);
        assert!(unknown.contains("do not retry without checking"));
    }
}
