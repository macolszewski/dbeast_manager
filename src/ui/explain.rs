//! Explain shares the console session while keeping ordinary results intact.

use super::*;
use crate::backend::{ExplainCompletion, ExplainExecution, ExplainRequest};
use crate::query_plan::ExplainMode;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);

fn statement(
    sql: &str,
    cursor: Option<&Range<usize>>,
    driver: DriverKind,
    mode: ExplainMode,
) -> Result<String, String> {
    let cursor = cursor.cloned().unwrap_or(0..0);
    let byte = |offset: usize| {
        sql.char_indices()
            .nth(offset)
            .map_or(sql.len(), |(byte, _)| byte)
    };
    let selected = if cursor.start != cursor.end {
        sql[byte(cursor.start.min(cursor.end))..byte(cursor.start.max(cursor.end))].to_owned()
    } else {
        splitter::statement_at(sql, driver, byte(cursor.start))
            .ok_or("Place the caret in a statement to explain.")?
            .text
    };
    crate::query_plan::explain_sql_mode(driver, &selected, mode)?;
    Ok(selected)
}

impl App {
    pub(super) fn explain_blocked_reason(
        &self,
        index: usize,
        mode: ExplainMode,
    ) -> Option<&'static str> {
        let Some(tab) = self.tabs.get(index) else {
            return Some("Open a query console to explain a statement");
        };
        if !matches!(tab.kind, TabKind::Console { .. }) {
            return Some("Open a query console to explain a statement");
        }
        if tab.is_running() || tab.context_pending.is_some() {
            return Some("Wait for the current operation before explaining a statement");
        }
        if self.source_is_saving(&tab.ds_id) {
            return Some("Wait for the data source to finish saving");
        }
        if tab
            .results
            .iter()
            .any(|result| result.action_state().has_changes)
        {
            return Some("Submit or revert pending grid changes before explaining a statement");
        }
        let Some(source) = self.source(&tab.ds_id) else {
            return Some("Select a data source to explain a statement");
        };
        if mode == ExplainMode::Analyse && source.driver == DriverKind::Sqlite {
            return Some("Explain Analyse is available for PostgreSQL and MySQL data sources");
        }
        None
    }

    pub(super) fn explain_tab(&mut self, index: usize, mode: ExplainMode) {
        let Some(tab) = self.tabs.get_mut(index) else {
            return;
        };
        for result in &mut tab.results {
            result.stage_action_buffer();
        }
        let request = (|| -> Result<_, String> {
            if let Some(reason) = self.explain_blocked_reason(index, mode) {
                return Err(reason.into());
            }
            let tab = &self.tabs[index];
            let TabKind::Console { sql } = &tab.kind else {
                return Err("Open a query console to explain a statement.".into());
            };
            let mut ds = self
                .source(&tab.ds_id)
                .cloned()
                .ok_or("Select a data source to explain a statement.")?;
            if ds.driver == DriverKind::Postgres
                && let Some(database) = &tab.database
            {
                ds = ds.with_database(database);
            }
            let sql = if let Some(query) = &tab.bound_query {
                if ds.identity() != query.source_identity {
                    return Err("The data source definition has changed. Reopen Related Rows from its source.".into());
                }
                crate::sqlgen::related_query(ds.driver, query)?.sql
            } else {
                statement(sql, tab.cursor.as_ref(), ds.driver, mode)?
            };
            Ok((ds, sql))
        })();
        let (ds, sql) = match request {
            Ok(request) => request,
            Err(error) => {
                self.notice = Some(ServicesNotice::error(error));
                return;
            }
        };
        self.related_rows.cancel();
        self.notice = None;
        let request_id = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
        let tab = &mut self.tabs[index];
        let context = ExecutionContext {
            database: tab.database.clone(),
            schema: tab.schema.clone(),
        };
        tab.running = Some(RunKind::Explain {
            mode,
            request_id,
            context_revision: tab.context_revision,
        });
        tab.running_origin = Some(results::ResultOrigin {
            ds_id: tab.ds_id.clone(),
            source: Some(ds.identity()),
            context: context.clone(),
            bound_query: None,
        });
        tab.log(
            LogKind::Sql,
            super::results::log_text(&[mode.label(), "> ", sql.as_str()]),
        );
        self.show_services = true;
        self.backend.explain(ExplainRequest {
            console_id: tab.id,
            request_id,
            ds,
            sql,
            bound_query: tab.bound_query.clone(),
            context,
            mode,
            manual_tx: tab.manual_tx,
        });
    }

    /// The analysed statement may have changed tables, privileges or the
    /// session. Existing grids stay as snapshots, but write access and
    /// completion metadata must be confirmed again.
    fn invalidate_after_analyse(&mut self, ds_id: &str) {
        self.invalidate_source_metadata(ds_id);
        let results: Vec<_> = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, tab)| tab.ds_id == ds_id && matches!(tab.kind, TabKind::Console { .. }))
            .flat_map(|(tab, data)| (0..data.results.len()).map(move |result| (tab, result)))
            .collect();
        // A new request id also rejects metadata replies that were in flight.
        for (tab, result) in results {
            self.check_result_editability(tab, result);
        }
    }

    pub(super) fn explain_done(
        &mut self,
        console_id: u64,
        request_id: u64,
        completion: ExplainCompletion,
        result: Result<crate::query_plan::QueryPlan, String>,
    ) {
        let executed = completion.execution != ExplainExecution::NotStarted;
        // Also for errors, stale replies and closed consoles: the SQL may have run.
        if executed || completion.session_closed {
            self.invalidate_after_analyse(&completion.ds_id);
        }
        let Some(index) = self.tabs.iter().position(|tab| tab.id == console_id) else {
            return;
        };
        let tab = &self.tabs[index];
        let Some(RunKind::Explain {
            mode,
            request_id: pending,
            context_revision,
        }) = tab.running
        else {
            return;
        };
        if pending != request_id {
            return;
        }
        let current_source = self.source(&tab.ds_id).map(|ds| {
            if ds.driver == DriverKind::Postgres
                && let Some(database) = &tab.database
            {
                ds.with_database(database).identity()
            } else {
                ds.identity()
            }
        });
        let same_context = tab.running_origin.as_ref().is_some_and(|origin| {
            context_revision == tab.context_revision
                && origin.source == current_source
                && origin.ds_id == tab.ds_id
                && origin.context.database == tab.database
                && origin.context.schema == tab.schema
        });
        let tab = &mut self.tabs[index];
        tab.running = None;
        tab.running_origin = None;
        if !same_context {
            tab.log(
                LogKind::Error,
                if executed {
                    "The data source or context changed while the statement was being analysed. The statement was executed; its plan is not shown."
                } else {
                    "The data source or context changed while the plan was loading. Explain the statement again."
                },
            );
            tab.view = ServicesView::Output;
            return;
        }
        if completion.session_closed {
            tab.in_tx = false;
            tab.resolved_context = None;
            tab.context_ui.invalidate();
        } else {
            if let Some(in_tx) = completion.in_tx {
                tab.in_tx = in_tx;
            }
            // After execution only a fresh read is trusted: functions can
            // change search_path, so a missing read clears the old snapshot.
            if executed || completion.post_context.is_some() {
                tab.resolved_context = completion.post_context;
                tab.context_ui.invalidate_context();
            }
        }
        match result {
            Ok(plan) => {
                let elapsed = plan.elapsed.as_millis();
                tab.log(
                    LogKind::Info,
                    match mode {
                        ExplainMode::Plan => format!("query plan loaded in {elapsed} ms"),
                        ExplainMode::Analyse => {
                            format!("statement executed and analysed in {elapsed} ms")
                        }
                    },
                );
                tab.plan = Some(query_plan::PlanView::new(plan));
                tab.view = ServicesView::Plan;
            }
            Err(error) => {
                tab.log(LogKind::Error, error);
                tab.view = ServicesView::Output;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completion(ds: &DataSource, in_tx: bool) -> ExplainCompletion {
        ExplainCompletion {
            ds_id: ds.id.clone(),
            execution: ExplainExecution::NotStarted,
            in_tx: Some(in_tx),
            session_closed: false,
            post_context: None,
        }
    }

    #[test]
    fn selected_or_current_statement_uses_source_character_offsets() {
        let sql = "-- Żółw\nSELECT 'ą';\nSELECT 'second';\nDELETE FROM things";
        let start = sql[..sql.find("SELECT 'second'").unwrap()].chars().count();
        assert_eq!(
            statement(
                sql,
                Some(&(start..start)),
                DriverKind::Postgres,
                ExplainMode::Plan
            )
            .unwrap(),
            "SELECT 'second'"
        );
        assert_eq!(
            statement(
                sql,
                Some(&(start..start + 15)),
                DriverKind::Postgres,
                ExplainMode::Plan
            )
            .unwrap(),
            "SELECT 'second'"
        );
        assert!(
            statement(
                sql,
                Some(&(start..sql.chars().count())),
                DriverKind::Postgres,
                ExplainMode::Plan
            )
            .is_err()
        );
    }

    #[test]
    fn stale_explain_reply_cannot_clear_another_operation_or_replace_results() {
        let ctx = egui::Context::default();
        let mut app = super::super::actions::tests::isolated_app(&ctx);
        let ds = DataSource::new(DriverKind::Sqlite);
        app.config.data_sources.push(ds.clone());
        let index = app.add_tab(
            "Plan".into(),
            ds.id.clone(),
            TabKind::Console {
                sql: "SELECT 1".into(),
            },
        );
        let id = app.tabs[index].id;
        app.tabs[index].results.push(ResultSet::new(
            "existing data".into(),
            "SELECT 99".into(),
            QueryResult {
                rows: vec![vec![crate::model::Value::Int(99)]],
                ..Default::default()
            },
            Page::first(500),
        ));
        app.tabs[index].running = Some(RunKind::Explain {
            mode: ExplainMode::Plan,
            request_id: 7,
            context_revision: 0,
        });
        app.explain_done(id, 6, completion(&ds, false), Err("stale".into()));
        assert!(
            app.tabs[index].running
                == Some(RunKind::Explain {
                    mode: ExplainMode::Plan,
                    request_id: 7,
                    context_revision: 0
                })
        );
        assert!(app.tabs[index].log.is_empty());
        app.tabs[index].running_origin = Some(results::ResultOrigin {
            ds_id: ds.id.clone(),
            source: Some(ds.identity()),
            context: ExecutionContext::default(),
            bound_query: None,
        });
        let plan = crate::query_plan::parse(
            DriverKind::Postgres,
            "SELECT 1".into(),
            QueryResult {
                rows: vec![vec![crate::model::Value::Text(
                    "[{\"Plan\":{\"Node Type\":\"Result\"}}]".into(),
                )]],
                ..Default::default()
            },
        );
        app.explain_done(id, 7, completion(&ds, true), Ok(plan));
        assert!(app.tabs[index].plan.is_some());
        assert!(app.tabs[index].in_tx);
        assert!(app.tabs[index].view == ServicesView::Plan);
        assert_eq!(
            app.tabs[index].results[0].data.rows,
            [vec![crate::model::Value::Int(99)]]
        );
        app.tabs[index].running = Some(RunKind::Query);
        app.explain_done(id, 7, completion(&ds, false), Err("duplicate".into()));
        assert!(app.tabs[index].running == Some(RunKind::Query));
        assert!(app.tabs[index].plan.is_some());
    }

    #[test]
    fn disconnect_invalidates_plan_acknowledgement_without_resurrecting_transaction() {
        let ctx = egui::Context::default();
        let mut app = super::super::actions::tests::isolated_app(&ctx);
        let ds = DataSource::new(DriverKind::Sqlite);
        app.config.data_sources.push(ds.clone());
        let index = app.add_tab(
            "Plan".into(),
            ds.id.clone(),
            TabKind::Console {
                sql: "SELECT 1".into(),
            },
        );
        let id = app.tabs[index].id;
        app.tabs[index].running = Some(RunKind::Explain {
            mode: ExplainMode::Plan,
            request_id: 8,
            context_revision: 0,
        });
        app.tabs[index].running_origin = Some(results::ResultOrigin {
            ds_id: ds.id.clone(),
            source: Some(ds.identity()),
            context: ExecutionContext::default(),
            bound_query: None,
        });
        app.tabs[index].in_tx = true;
        app.disconnect(&ds.id);
        app.explain_done(
            id,
            8,
            completion(&ds, true),
            Ok(crate::query_plan::parse(
                DriverKind::Sqlite,
                "SELECT 1".into(),
                QueryResult::default(),
            )),
        );
        assert!(app.tabs[index].running.is_none());
        assert!(!app.tabs[index].in_tx);
        assert!(app.tabs[index].plan.is_none());
        assert!(app.tabs[index].resolved_context.is_none());
        assert!(
            app.tabs[index]
                .log
                .last()
                .unwrap()
                .text
                .contains("context changed")
        );
    }

    fn analysed(ds: &DataSource) -> ExplainCompletion {
        ExplainCompletion {
            execution: ExplainExecution::Succeeded,
            ..completion(ds, true)
        }
    }

    fn console(app: &mut App, ds: &DataSource) -> usize {
        let index = app.add_tab(
            "Analyse".into(),
            ds.id.clone(),
            TabKind::Console {
                sql: "UPDATE t SET n = n + 1".into(),
            },
        );
        app.tabs[index].results.push(ResultSet::new(
            "existing data".into(),
            "SELECT id, n FROM t".into(),
            QueryResult {
                rows: vec![vec![crate::model::Value::Int(1)]],
                ..Default::default()
            },
            Page::first(500),
        ));
        index
    }

    fn running(app: &mut App, index: usize, ds: &DataSource, request_id: u64) {
        app.tabs[index].running = Some(RunKind::Explain {
            mode: ExplainMode::Analyse,
            request_id,
            context_revision: app.tabs[index].context_revision,
        });
        app.tabs[index].running_origin = Some(results::ResultOrigin {
            ds_id: ds.id.clone(),
            source: Some(ds.identity()),
            context: ExecutionContext::default(),
            bound_query: None,
        });
    }

    #[test]
    fn analyse_is_offered_for_postgresql_and_mysql_consoles() {
        let ctx = egui::Context::default();
        let mut app = super::super::actions::tests::isolated_app(&ctx);
        let sqlite = DataSource::new(DriverKind::Sqlite);
        let postgres = DataSource::new(DriverKind::Postgres);
        let mysql = DataSource::new(DriverKind::MySql);
        app.config
            .data_sources
            .extend([sqlite.clone(), postgres.clone(), mysql.clone()]);
        let local = console(&mut app, &sqlite);
        let server = console(&mut app, &postgres);
        let mysql_console = console(&mut app, &mysql);
        assert!(
            app.explain_blocked_reason(mysql_console, ExplainMode::Analyse)
                .is_none()
        );
        assert!(
            app.explain_blocked_reason(local, ExplainMode::Plan)
                .is_none()
        );
        assert!(
            app.explain_blocked_reason(local, ExplainMode::Analyse)
                .unwrap()
                .contains("PostgreSQL")
        );
        assert!(
            app.explain_blocked_reason(server, ExplainMode::Analyse)
                .is_none()
        );
        app.explain_tab(local, ExplainMode::Analyse);
        assert!(app.tabs[local].running.is_none());
        assert!(app.notice.as_ref().unwrap().text.contains("PostgreSQL"));
    }

    #[test]
    fn executed_analyse_invalidates_source_metadata_even_for_stale_replies() {
        let ctx = egui::Context::default();
        let mut app = super::super::actions::tests::isolated_app(&ctx);
        let ds = DataSource::new(DriverKind::Postgres);
        let other = DataSource::new(DriverKind::Postgres);
        app.config.data_sources.extend([ds.clone(), other.clone()]);
        let index = console(&mut app, &ds);
        let unrelated = console(&mut app, &other);
        let id = app.tabs[index].id;
        let key = crate::model::MetadataKey::for_source(&ds);
        let other_key = crate::model::MetadataKey::for_source(&other);
        let reset = |app: &mut App| {
            app.meta.insert(key.clone(), Default::default());
            app.meta.insert(other_key.clone(), Default::default());
            app.tabs[index].results[0].begin_edit_metadata(500);
            app.tabs[unrelated].results[0].begin_edit_metadata(501);
        };
        // Plan never executes the statement: nothing is invalidated.
        reset(&mut app);
        app.explain_done(id, 1, completion(&ds, false), Err("stale".into()));
        assert!(app.meta.contains_key(&key));
        assert!(app.tabs[index].results[0].accepts_edit_metadata(500));
        // A stale or closed-console reply still reports a possible execution.
        app.explain_done(id, 1, analysed(&ds), Err("stale".into()));
        assert!(!app.meta.contains_key(&key));
        assert!(app.meta.contains_key(&other_key));
        assert!(!app.tabs[index].results[0].accepts_edit_metadata(500));
        assert!(app.tabs[unrelated].results[0].accepts_edit_metadata(501));
        assert_eq!(
            app.tabs[index].results[0].data.rows,
            [vec![crate::model::Value::Int(1)]]
        );
        reset(&mut app);
        app.explain_done(9999, 1, analysed(&ds), Err("closed".into()));
        assert!(!app.meta.contains_key(&key));
        assert!(!app.tabs[index].results[0].accepts_edit_metadata(500));
    }

    #[test]
    fn analyse_context_comes_only_from_the_post_execution_read() {
        let ctx = egui::Context::default();
        let mut app = super::super::actions::tests::isolated_app(&ctx);
        let ds = DataSource::new(DriverKind::Postgres);
        app.config.data_sources.push(ds.clone());
        let index = console(&mut app, &ds);
        let id = app.tabs[index].id;
        let context = |path: &str| crate::model::ResolvedContext {
            database: Some("dbeast".into()),
            search_path: Some(path.into()),
            schemas: vec![path.into()],
            temporary_schema: None,
        };
        let before = context("public");
        app.tabs[index].resolved_context = Some(before.clone());
        let plan = || {
            let mut data = QueryResult {
                rows: vec![vec![crate::model::Value::Text(
                    "[{\"Plan\":{\"Node Type\":\"Result\",\"Actual Loops\":1}}]".into(),
                )]],
                ..Default::default()
            };
            data.context = Some(context("public"));
            crate::query_plan::parse_mode(
                DriverKind::Postgres,
                "SELECT 1".into(),
                data,
                ExplainMode::Analyse,
            )
        };
        running(&mut app, index, &ds, 3);
        app.explain_done(
            id,
            3,
            ExplainCompletion {
                post_context: Some(context("pg_catalog")),
                ..analysed(&ds)
            },
            Ok(plan()),
        );
        assert_eq!(
            app.tabs[index].resolved_context,
            Some(context("pg_catalog"))
        );
        assert!(app.tabs[index].in_tx);
        assert!(app.tabs[index].view == ServicesView::Plan);
        assert!(
            app.tabs[index]
                .log
                .last()
                .unwrap()
                .text
                .contains("executed and analysed")
        );

        // An unconfirmed read clears the old snapshot instead of restoring it.
        running(&mut app, index, &ds, 4);
        app.explain_done(id, 4, analysed(&ds), Err("server error".into()));
        assert!(app.tabs[index].resolved_context.is_none());

        // A closed session ends the transaction and keeps the plan snapshot.
        app.tabs[index].resolved_context = Some(before);
        running(&mut app, index, &ds, 5);
        app.explain_done(
            id,
            5,
            ExplainCompletion {
                session_closed: true,
                execution: ExplainExecution::Unknown,
                ..completion(&ds, true)
            },
            Err("Disconnect and reconnect the data source before continuing.".into()),
        );
        assert!(!app.tabs[index].in_tx);
        assert!(app.tabs[index].resolved_context.is_none());
        assert!(app.tabs[index].plan.is_some());
        assert!(app.tabs[index].view == ServicesView::Output);
    }
}
