//! Edit authority and mutation acknowledgements for console results and table
//! tabs. Both use stable result IDs and the same parameterized mutation path.

use super::{App, DriverKind, LogKind, RunKind, ServicesNotice, TabKind};
use crate::backend::{MutationRequest, ResultMetadataRequest};
use crate::model::{MutationFailure, MutationReport, TableEditMetadata};

impl App {
    /// Table tabs know their target; console results must pass the SELECT grammar.
    fn edit_sql(&self, tab_index: usize, result_index: usize) -> Option<&str> {
        let tab = &self.tabs[tab_index];
        match tab.kind {
            TabKind::Table { .. } => None,
            TabKind::Console { .. } => Some(tab.results[result_index].sql.as_str()),
        }
    }

    pub(super) fn check_result_editability(&mut self, tab_index: usize, result_index: usize) {
        let tab = &self.tabs[tab_index];
        let result = &tab.results[result_index];
        if result.origin.as_ref().is_some_and(|origin| origin.bound_query.is_some()) {
            self.tabs[tab_index].results[result_index].set_edit_read_only(
                "Related query results are currently read-only. Open the table or execute a regular SELECT to edit.",
            );
            return;
        }
        let sql = self.edit_sql(tab_index, result_index);
        let request = (|| {
            let mut ds = self
                .source(&tab.ds_id)
                .cloned()
                .ok_or("The data source is no longer available.")?;
            if ds.read_only {
                return Err("This data source is read-only.".to_string());
            }
            if let Some(reason) = self.result_context_blocked_reason(tab_index, result_index) {
                return Err(reason.into());
            }
            if ds.driver == DriverKind::Postgres
                && let Some(database) = &tab.database
            {
                ds = ds.with_database(database);
            }
            crate::result_edit::candidate(sql, ds.driver, &result.data)?;
            let origin = result
                .origin
                .as_ref()
                .ok_or("Execute the query again to establish its source.")?;
            let source_identity = origin
                .source
                .clone()
                .ok_or("Execute the query again to establish its source.")?;
            let expected_context = result
                .data
                .context
                .clone()
                .ok_or("Execute the query again to establish its database context.")?;
            Ok(ResultMetadataRequest {
                console_id: tab.id,
                result_id: result.action_identity(),
                request_id: 0,
                ds,
                source_identity,
                expected_context,
                source: result.data.source.clone().unwrap(),
            })
        })();
        match request {
            Ok(mut request) => {
                self.metadata_request_id = self.metadata_request_id.wrapping_add(1);
                request.request_id = self.metadata_request_id;
                self.tabs[tab_index].results[result_index].begin_edit_metadata(request.request_id);
                self.backend.load_result_metadata(request);
            }
            Err(reason) => self.tabs[tab_index].results[result_index].set_edit_read_only(reason),
        }
    }

    pub(super) fn result_metadata_loaded(
        &mut self,
        console_id: u64,
        result_id: u64,
        request_id: u64,
        metadata: Result<TableEditMetadata, String>,
    ) {
        let Some(tab_index) = self.tabs.iter().position(|tab| tab.id == console_id) else {
            return;
        };
        let Some(result_index) = self.tabs[tab_index]
            .results
            .iter()
            .position(|rs| rs.action_identity() == result_id)
        else {
            return;
        };
        if !self.tabs[tab_index].results[result_index].accepts_edit_metadata(request_id) {
            return;
        }
        let driver = self.source(&self.tabs[tab_index].ds_id).map(|ds| ds.driver);
        // A SQLite table without a usable primary key is identified by rowid,
        // which a plain SELECT * does not return. Select it and check again.
        if let (Ok(table), Some(DriverKind::Sqlite)) = (&metadata, driver)
            && table.primary_key == [crate::model::SQLITE_ROWID]
            && let TabKind::Table { select_rowid, .. } = &mut self.tabs[tab_index].kind
            && !*select_rowid
        {
            *select_rowid = true;
            self.run_tab(tab_index, false);
            return;
        }
        let sql = self.edit_sql(tab_index, result_index);
        let result = &self.tabs[tab_index].results[result_index];
        let metadata = metadata.and_then(|metadata| {
            if let Some(reason) = self.result_context_blocked_reason(tab_index, result_index) {
                return Err(reason.into());
            }
            if let TabKind::Table { schema, table, .. } = &self.tabs[tab_index].kind {
                let expected_schema = schema.as_deref().unwrap_or("main");
                if metadata.table.name != *table || metadata.table.schema != expected_schema {
                    return Err("The result does not come from this tab's table.".into());
                }
            }
            let driver = driver.ok_or("The data source is no longer available.")?;
            crate::result_edit::resolve(sql, driver, &result.data, &metadata)
        });
        self.tabs[tab_index].results[result_index].finish_edit_metadata(request_id, metadata);
    }

    pub(super) fn submit_result_edits(&mut self, tab_index: usize, result_index: usize) {
        let Some(tab) = self.tabs.get(tab_index) else {
            return;
        };
        if tab.is_running() || tab.context_pending.is_some() || self.source_is_saving(&tab.ds_id) {
            return;
        }
        let Some(result) = tab.results.get(result_index) else {
            return;
        };
        let request = (|| {
            if let Some(reason) = self.result_context_blocked_reason(tab_index, result_index) {
                return Err(reason.into());
            }
            let metadata = result.edit_metadata().ok_or_else(|| {
                result
                    .edit_read_only_reason()
                    .unwrap_or("This result is read-only.")
                    .to_string()
            })?;
            let mut ds = self
                .source(&tab.ds_id)
                .cloned()
                .ok_or("The data source is no longer available.")?;
            if ds.read_only {
                return Err("This data source is read-only.".into());
            }
            if ds.driver == DriverKind::Postgres
                && let Some(database) = &tab.database
            {
                ds = ds.with_database(database);
            }
            let mutations = result.pending_mutations(ds.driver)?;
            let origin = result
                .origin
                .as_ref()
                .ok_or("Execute the query again before editing this result.")?;
            Ok(MutationRequest {
                console_id: tab.id,
                result_id: result.action_identity(),
                request_id: 0,
                ds,
                source_identity: origin
                    .source
                    .clone()
                    .ok_or("The result source is unknown.")?,
                expected_context: result
                    .data
                    .context
                    .clone()
                    .ok_or("The result context is unknown.")?,
                session_epoch: metadata.session_epoch,
                metadata: metadata.clone(),
                mutations,
                manual_tx: tab.manual_tx,
            })
        })();
        match request {
            Ok(mut request) => {
                if request.mutations.is_empty() {
                    return;
                }
                self.metadata_request_id = self.metadata_request_id.wrapping_add(1);
                request.request_id = self.metadata_request_id;
                let tab = &mut self.tabs[tab_index];
                tab.running = Some(RunKind::Edits {
                    result_id: request.result_id,
                    request_id: request.request_id,
                });
                for mutation in &request.mutations {
                    tab.log(
                        LogKind::Sql,
                        format!("{} [{} bound parameters]", mutation.sql, mutation.params.len()),
                    );
                }
                self.notice = None;
                self.backend.apply_updates(request);
            }
            Err(reason) => {
                self.tabs[tab_index].log(LogKind::Error, &reason);
                self.notice = Some(ServicesNotice::error(reason));
            }
        }
    }

    pub(super) fn result_edits_done(
        &mut self,
        console_id: u64,
        result_id: u64,
        request_id: u64,
        in_tx: Option<bool>,
        result: Result<MutationReport, MutationFailure>,
    ) {
        let Some(tab_index) = self.tabs.iter().position(|tab| tab.id == console_id) else {
            return;
        };
        let tab = &mut self.tabs[tab_index];
        if tab.running
            != Some(RunKind::Edits {
                result_id,
                request_id,
            })
        {
            return;
        }
        tab.running = None;
        if let Some(in_tx) = in_tx {
            tab.in_tx = in_tx;
        }
        let Some(result_index) = tab
            .results
            .iter()
            .position(|rs| rs.action_identity() == result_id)
        else {
            return;
        };
        match result {
            Ok(report) => {
                let rs = &mut tab.results[result_index];
                rs.revert();
                rs.set_edit_read_only(
                    "Changes were saved. Reload the result before making further edits.",
                );
                tab.unsaved_writes |= self.files.iter().any(|file| {
                    file.source.id == tab.ds_id
                        && matches!(
                            file.kind,
                            crate::files::FileKind::Csv | crate::files::FileKind::DataPackage
                        )
                        && !file.tables.is_empty()
                });
                let message = if tab.in_tx {
                    format!(
                        "{} rows changed in the open transaction. Commit to apply them.",
                        report.changed_rows
                    )
                } else {
                    format!("{} rows changed. Reloading the result…", report.changed_rows)
                };
                tab.log(LogKind::Info, &message);
                self.notice = Some(ServicesNotice::info(message));
                self.reload_result(tab_index, result_index);
                self.save_opened_file(tab_index);
            }
            Err(failure) => {
                let row = failure
                    .source_row
                    .map_or_else(String::new, |row| format!("Row {}: ", row + 1));
                let message = format!("{row}{}", failure.message);
                if failure.outcome_unknown {
                    tab.results[result_index].set_edit_read_only("The save outcome is unknown. Verify the database with a fresh SELECT before editing again; pending values are kept as a reference.");
                }
                tab.log(LogKind::Error, &message);
                self.notice = Some(ServicesNotice::error(message));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;
    use crate::sqlgen::CellEdit;
    use crate::ui::{
        ServicesView, actions,
        results::{ResultOrigin, ResultSet},
    };

    fn fixture() -> (App, TableEditMetadata) {
        let mut app = actions::tests::isolated_app(&egui::Context::default());
        let source = DataSource::new(DriverKind::Sqlite);
        app.config.data_sources.push(source.clone());
        app.add_tab(
            "console".into(),
            source.id.clone(),
            TabKind::Console {
                sql: "SELECT * FROM records".into(),
            },
        );
        let data = QueryResult {
            columns: ["key_alias", "note"]
                .iter()
                .map(|name| Column {
                    name: (*name).into(),
                    type_name: String::new(),
                })
                .collect(),
            rows: vec![vec![Value::Int(1), Value::Text("old".into())]],
            context: Some(ResolvedContext::default()),
            source: Some(ResultSource {
                session_epoch: 1,
                columns: ["id", "note"]
                    .iter()
                    .map(|name| {
                        Some(ColumnOrigin {
                            table: OriginTable::Named {
                                schema: "main".into(),
                                table: "records".into(),
                            },
                            column: OriginColumn::Named((*name).into()),
                        })
                    })
                    .collect(),
                exact_columns: vec![true; 2],
            }),
            ..Default::default()
        };
        let mut result = ResultSet::new(
            "records".into(),
            "SELECT id AS key_alias,note FROM records".into(),
            data,
            Page::first(500),
        );
        result.origin = Some(ResultOrigin {
            ds_id: source.id.clone(),
            source: Some(source.identity()),
            context: ExecutionContext::default(),
            bound_query: None,
        });
        app.tabs[0].results.push(result);
        app.tabs[0].view = ServicesView::Result(0);
        let table = TableEditMetadata {
            table: PhysicalTable {
                schema: "main".into(),
                name: "records".into(),
                oid: None,
            },
            columns: ["id", "note"]
                .iter()
                .enumerate()
                .map(|(i, name)| PhysicalColumn {
                    name: (*name).into(),
                    ordinal: i as u32,
                    nullable: false,
                    generated: false,
                    update_default: false,
                    comparable: true,
                })
                .collect(),
            primary_key: vec!["id".into()],
            transactional: true,
        };
        (app, table)
    }

    #[test]
    fn metadata_acknowledgement_needs_current_result_and_request() {
        let (mut app, table) = fixture();
        let id = app.tabs[0].id;
        let result_id = app.tabs[0].results[0].action_identity();
        app.tabs[0].results[0].begin_edit_metadata(8);
        app.result_metadata_loaded(id, result_id, 7, Ok(table.clone()));
        assert!(app.tabs[0].results[0].edit_metadata_pending());
        app.result_metadata_loaded(id, result_id + 100, 8, Ok(table.clone()));
        assert!(app.tabs[0].results[0].edit_metadata_pending());
        app.result_metadata_loaded(id, result_id, 8, Ok(table.clone()));
        assert!(app.tabs[0].results[0].edit_metadata().is_some());
        app.tabs[0].results[0].begin_edit_metadata(9);
        app.config.data_sources[0].file = "another.sqlite".into();
        app.result_metadata_loaded(id, result_id, 9, Ok(table));
        assert!(app.tabs[0].results[0].edit_metadata().is_none());
    }

    #[test]
    fn table_tabs_verify_their_target_and_select_rowid_without_a_key() {
        let (mut app, mut table) = fixture();
        let id = app.tabs[0].id;
        app.tabs[0].kind = TabKind::Table {
            schema: None,
            table: "records".into(),
            filter: String::new(),
            order: String::new(),
            select_rowid: false,
        };
        let result_id = app.tabs[0].results[0].action_identity();
        // Another table's metadata never authorizes this tab.
        let mut other = table.clone();
        other.table.name = "other".into();
        app.tabs[0].results[0].begin_edit_metadata(1);
        app.result_metadata_loaded(id, result_id, 1, Ok(other));
        assert!(app.tabs[0].results[0].edit_metadata().is_none());
        // The table tab skips the console grammar but still maps physical columns.
        app.tabs[0].results[0].begin_edit_metadata(2);
        app.result_metadata_loaded(id, result_id, 2, Ok(table.clone()));
        assert!(app.tabs[0].results[0].edit_metadata().is_some());
        // Without a usable key SQLite falls back to rowid: the tab reloads with it.
        table.primary_key = vec![crate::model::SQLITE_ROWID.into()];
        app.tabs[0].results[0].begin_edit_metadata(3);
        app.result_metadata_loaded(id, result_id, 3, Ok(table));
        assert!(matches!(app.tabs[0].kind, TabKind::Table { select_rowid: true, .. }));
        assert!(app.tabs[0].is_running());
        assert_eq!(
            super::super::table_query(&app.tabs[0].kind, DriverKind::Sqlite),
            r#"SELECT rowid, * FROM "records""#
        );
    }

    #[test]
    fn uncertain_save_keeps_edits_but_disables_retry_and_ignores_old_ack() {
        let (mut app, table) = fixture();
        let id = app.tabs[0].id;
        let result_id = app.tabs[0].results[0].action_identity();
        app.tabs[0].results[0].begin_edit_metadata(1);
        app.result_metadata_loaded(id, result_id, 1, Ok(table));
        app.tabs[0].results[0]
            .edits
            .changed
            .insert((0, 1), CellEdit::Text("pending".into()));
        app.tabs[0].running = Some(RunKind::Edits {
            result_id,
            request_id: 3,
        });
        app.result_edits_done(
            id,
            result_id,
            2,
            Some(false),
            Ok(MutationReport { changed_rows: 1 }),
        );
        assert!(app.tabs[0].is_running());
        app.result_edits_done(
            id,
            result_id,
            3,
            Some(false),
            Err(MutationFailure {
                message: "Connection lost during commit".into(),
                source_row: None,
                outcome_unknown: true,
            }),
        );
        assert!(!app.tabs[0].is_running());
        let result = &app.tabs[0].results[0];
        assert!(!result.edits.is_empty());
        assert!(!result.can_submit_edits());
        assert!(result.edit_read_only_reason().unwrap().contains("unknown"));
    }

    #[test]
    fn execute_does_not_discard_pending_console_updates() {
        let (mut app, _) = fixture();
        app.tabs[0].results[0]
            .edits
            .changed
            .insert((0, 1), CellEdit::Null);
        app.start_run(0, vec!["SELECT 2".into()], Page::first(500), RunKind::Query);
        assert!(!app.tabs[0].is_running());
        assert_eq!(app.tabs[0].results.len(), 1);
        assert!(!app.tabs[0].results[0].edits.is_empty());
        assert!(
            app.notice
                .as_ref()
                .unwrap()
                .text
                .contains("pending grid changes")
        );
    }
}
