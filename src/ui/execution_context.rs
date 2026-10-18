//! Console context changes are serialized with execution and acknowledged by revision.

use super::{App, LogKind, TabKind, console};
use crate::model::{
    CompletionMetadata, DriverKind, ExecutionContext, MetadataKey, ResolvedContext, SchemaSelection,
};

impl App {
    pub(super) fn result_replay_blocked_reason(
        &self,
        index: usize,
        result: usize,
    ) -> Option<&'static str> {
        let tab = self.tabs.get(index)?;
        let result = tab.results.get(result)?;
        let driver = result
            .origin
            .as_ref()
            .and_then(|origin| origin.source.as_ref())
            .map(|source| source.driver)
            .or_else(|| self.source(&tab.ds_id).map(|source| source.driver))?;
        if let Some(query) = result
            .origin
            .as_ref()
            .and_then(|origin| origin.bound_query.as_ref())
        {
            return crate::related::validate_query(driver, query).is_err().then_some(
                "The related query's parameters are invalid. Reopen Related Rows from its source."
            );
        }
        if tab.bound_query.is_some() {
            return Some(
                "The related query's parameters are unavailable. Reopen Related Rows from its source.",
            );
        }
        (!crate::result_edit::can_replay(&result.sql, driver)).then_some(
            "This statement cannot be safely rerun for paging, reload or full export. Execute it explicitly in the console; the fetched page can still be copied or exported."
        )
    }

    pub(super) fn begin_metadata_request(&mut self, key: MetadataKey) -> u64 {
        self.metadata_request_id = self.metadata_request_id.wrapping_add(1);
        let request_id = self.metadata_request_id;
        self.metadata_loading.insert(key, request_id);
        request_id
    }

    pub(super) fn update_metadata_key(&mut self, index: usize) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let Some(mut source) = self.source(&tab.ds_id).cloned() else {
            return;
        };
        if source.driver == DriverKind::Postgres
            && let Some(database) = &tab.database
        {
            source = source.with_database(database);
        }
        let key = MetadataKey::for_source(&source);
        if self.tabs[index].metadata_key != key {
            self.tabs[index].metadata_key = key;
            self.tabs[index].context_ui.invalidate();
        }
    }

    pub(super) fn ensure_console_metadata(&mut self, index: usize) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let key = tab.metadata_key.clone();
        if self.meta.contains_key(&key) || self.metadata_loading.contains_key(&key) {
            return;
        }
        let Some(mut source) = self.source(&tab.ds_id).cloned() else {
            return;
        };
        if source.driver == DriverKind::Postgres
            && let Some(database) = &tab.database
        {
            source = source.with_database(database);
        }
        self.metadata_errors.remove(&key);
        let request_id = self.begin_metadata_request(key);
        self.backend.load_completion(source, request_id);
    }

    pub(super) fn invalidate_source_metadata(&mut self, source: &str) {
        self.related_rows.cancel();
        self.backend.invalidate_result_metadata(source);
        self.meta.retain(|key, _| key.ds_id != source);
        self.metadata_loading.retain(|key, _| key.ds_id != source);
        self.metadata_errors.retain(|key, _| key.ds_id != source);
        for tab in self.tabs.iter_mut().filter(|tab| tab.ds_id == source) {
            tab.context_ui.invalidate();
        }
    }

    pub(super) fn completion_loaded(
        &mut self,
        key: MetadataKey,
        request_id: u64,
        result: Result<CompletionMetadata, String>,
        complete: bool,
    ) {
        if self.source(&key.ds_id).is_none() || self.metadata_loading.get(&key) != Some(&request_id)
        {
            return;
        }
        if !complete {
            // Names only: usable now, the request stays open for the columns.
            if let Ok(metadata) = result {
                self.meta.insert(key.clone(), metadata);
                for tab in self.tabs.iter_mut().filter(|tab| tab.metadata_key == key) {
                    tab.context_ui.invalidate();
                }
            }
            return;
        }
        self.metadata_loading.remove(&key);
        match result {
            Ok(metadata) => {
                self.metadata_errors.remove(&key);
                self.meta.insert(key.clone(), metadata);
            }
            Err(error) => {
                self.metadata_errors.insert(key.clone(), error);
            }
        }
        for tab in self.tabs.iter_mut().filter(|tab| tab.metadata_key == key) {
            tab.context_ui.invalidate();
        }
    }

    pub(super) fn context_change_blocked_reason(&self, tab_id: u64) -> Option<&'static str> {
        let Some(tab) = self.tabs.iter().find(|tab| tab.id == tab_id) else {
            return Some("The console is closed");
        };
        if !matches!(tab.kind, TabKind::Console { .. }) {
            return Some("Table editors keep their original database context");
        }
        if self.source_is_saving(&tab.ds_id) {
            return Some("Wait for the data source file to finish saving");
        }
        console::source_change_blocked_reason(tab)
    }

    pub(super) fn apply_console_context(
        &mut self,
        tab_id: u64,
        intent: console::ContextIntent,
    ) -> bool {
        self.related_rows.cancel();
        let Some(index) = self.tabs.iter().position(|tab| tab.id == tab_id) else {
            return false;
        };
        if matches!(intent, console::ContextIntent::LoadSchemas) {
            self.ensure_console_metadata(index);
            return true;
        }
        if let Some(reason) = self.context_change_blocked_reason(tab_id) {
            self.tabs[index].log(LogKind::Info, reason);
            return false;
        }
        match intent {
            console::ContextIntent::Source(source) => {
                if self.tabs[index].ds_id == source || self.source(&source).is_none() {
                    return false;
                }
                self.backend.close_session(&self.tabs[index].ds_id, tab_id);
                self.tabs[index].ds_id = source;
                self.tabs[index].database = None;
            }
            console::ContextIntent::Database(database) => {
                if self.tabs[index].database == database {
                    return false;
                }
                let Some(source) = self.source(&self.tabs[index].ds_id) else {
                    return false;
                };
                if source.driver != DriverKind::Postgres
                    || self.tabs[index]
                        .database
                        .as_deref()
                        .unwrap_or(&source.database)
                        == database.as_deref().unwrap_or(&source.database)
                {
                    return false;
                }
                self.backend.close_session(&self.tabs[index].ds_id, tab_id);
                self.tabs[index].database = database;
            }
            console::ContextIntent::Schema(schema) => {
                let tab = &self.tabs[index];
                let Some(mut source) = self.source(&tab.ds_id).cloned() else {
                    return false;
                };
                if source.driver == DriverKind::Sqlite {
                    return false;
                }
                if source.driver == DriverKind::Postgres
                    && let Some(database) = &tab.database
                {
                    source = source.with_database(database);
                }
                let tab = &mut self.tabs[index];
                let requested = ExecutionContext {
                    database: tab.database.clone(),
                    schema,
                };
                tab.context_revision = tab.context_revision.wrapping_add(1);
                tab.context_pending = Some(requested.clone());
                tab.completion = None;
                tab.context_ui.invalidate_context();
                self.backend
                    .change_schema(tab_id, source, requested, tab.context_revision);
                self.explorer_ui.cancel_reveal();
                return true;
            }
            console::ContextIntent::LoadSchemas => unreachable!(),
        }
        let tab = &mut self.tabs[index];
        tab.schema = SchemaSelection::Default;
        tab.resolved_context = None;
        tab.context_revision = tab.context_revision.wrapping_add(1);
        tab.completion = None;
        tab.filter_history = None;
        tab.context_ui.invalidate();
        self.update_metadata_key(index);
        self.ensure_console_metadata(index);
        self.explorer_ui.cancel_reveal();
        self.save_config();
        self.save_session(true);
        self.pending_editor_focus = true;
        true
    }

    pub(super) fn schema_changed(
        &mut self,
        console_id: u64,
        source: String,
        revision: u64,
        requested: ExecutionContext,
        result: Result<ResolvedContext, String>,
    ) {
        let Some(tab) = self.tabs.iter_mut().find(|tab| {
            tab.id == console_id
                && tab.ds_id == source
                && tab.context_revision == revision
                && tab.context_pending.as_ref() == Some(&requested)
        }) else {
            return;
        };
        tab.context_pending = None;
        tab.context_ui.invalidate_context();
        match result {
            Ok(resolved) => {
                tab.schema = requested.schema;
                tab.resolved_context = Some(resolved);
                tab.log(LogKind::Info, "Default schema changed for this console");
                self.pending_editor_focus = true;
                self.save_config();
                self.save_session(true);
            }
            Err(error) => tab.log(LogKind::Error, format!("Cannot change schema: {error}")),
        }
    }

    pub(super) fn result_source_blocked_reason(
        &self,
        index: usize,
        result: usize,
    ) -> Option<&'static str> {
        let origin = self.tabs.get(index)?.results.get(result)?.origin.as_ref()?;
        let expected = origin.source.as_ref()?;
        let Some(source) = self.source(&origin.ds_id) else {
            return Some(
                "The result's data source is no longer available. Execute the query again.",
            );
        };
        let mut actual = source.identity();
        if actual.driver == DriverKind::Postgres
            && let Some(database) = &origin.context.database
        {
            actual.database = database.clone();
        }
        (actual != *expected).then_some("The data source connection settings have changed since this result was created. Execute the query again before reloading or exporting it.")
    }

    pub(super) fn result_context_blocked_reason(
        &self,
        index: usize,
        result: usize,
    ) -> Option<&'static str> {
        if let Some(reason) = self.result_source_blocked_reason(index, result) {
            return Some(reason);
        }
        let tab = self.tabs.get(index)?;
        let result = tab.results.get(result)?;
        if result
            .data
            .context
            .as_ref()
            .zip(tab.resolved_context.as_ref())
            .is_some_and(|(expected, current)| expected != current)
            || result.origin.as_ref().is_some_and(|origin| {
                origin.ds_id != tab.ds_id
                    || origin.context.database != tab.database
                    || origin.context.schema != tab.schema
            })
        {
            return Some(
                "This result belongs to another database or schema. Switch back to its context or execute the query again before reloading or fetching another page.",
            );
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        actions,
        results::{ResultOrigin, ResultSet},
    };
    use super::*;
    use crate::model::{DataSource, Page, QueryResult};

    #[test]
    fn same_source_and_database_preserve_console_context() {
        let mut app = actions::tests::isolated_app(&egui::Context::default());
        let source = DataSource::new(DriverKind::Postgres);
        app.config.data_sources.push(source.clone());
        let i = app.add_tab(
            "query".into(),
            source.id.clone(),
            TabKind::Console {
                sql: "select 1".into(),
            },
        );
        let id = app.tabs[i].id;
        app.tabs[i].schema = SchemaSelection::Named("sales".into());
        app.tabs[i].database = Some("other".into());
        assert!(!app.apply_console_context(id, console::ContextIntent::Source(source.id)));
        assert!(
            !app.apply_console_context(id, console::ContextIntent::Database(Some("other".into())))
        );
        assert_eq!(app.tabs[i].schema, SchemaSelection::Named("sales".into()));
        assert_eq!(app.tabs[i].context_revision, 0);
    }

    #[test]
    fn metadata_is_partitioned_by_physical_database_and_errors_allow_retry() {
        let mut app = actions::tests::isolated_app(&egui::Context::default());
        let source = DataSource::new(DriverKind::Postgres);
        app.config.data_sources.push(source.clone());
        let a = app.add_tab(
            "a".into(),
            source.id.clone(),
            TabKind::Console { sql: String::new() },
        );
        let b = app.add_tab(
            "b".into(),
            source.id.clone(),
            TabKind::Console { sql: String::new() },
        );
        app.tabs[b].database = Some("other".into());
        app.update_metadata_key(b);
        let ka = app.tabs[a].metadata_key.clone();
        let kb = app.tabs[b].metadata_key.clone();
        assert_ne!(ka, kb);
        let ra = app.begin_metadata_request(ka.clone());
        let rb = app.begin_metadata_request(kb.clone());
        app.completion_loaded(
            ka.clone(),
            ra,
            Ok(CompletionMetadata {
                tables: vec![],
                schemas: vec!["empty".into()],
                default_context: Default::default(),
            }),
            true,
        );
        app.completion_loaded(kb.clone(), rb, Err("catalog unavailable".into()), true);
        assert_eq!(app.meta[&ka].schemas, ["empty"]);
        assert!(!app.meta.contains_key(&kb));
        assert!(!app.metadata_loading.contains_key(&kb));
        assert!(app.metadata_errors.contains_key(&kb));
        app.invalidate_source_metadata(&source.id);
        let fresh = app.begin_metadata_request(ka.clone());
        app.completion_loaded(ka.clone(), ra, Ok(CompletionMetadata::default()), true);
        assert_eq!(app.metadata_loading.get(&ka), Some(&fresh));
        assert!(app.meta.is_empty());
        // Names arrive first and are usable while the request stays open.
        let names = CompletionMetadata {
            schemas: vec!["named".into()],
            ..Default::default()
        };
        app.completion_loaded(ka.clone(), fresh, Ok(names), false);
        assert_eq!(app.meta[&ka].schemas, ["named"]);
        assert_eq!(app.metadata_loading.get(&ka), Some(&fresh));
        app.completion_loaded(ka.clone(), fresh, Err("fresh error".into()), true);
        assert_eq!(app.metadata_errors[&ka], "fresh error");
        app.invalidate_source_metadata(&source.id);
        app.completion_loaded(ka, ra, Ok(CompletionMetadata::default()), true);
        assert!(
            app.meta.is_empty(),
            "a late response cannot restore disconnected metadata"
        );
    }

    #[test]
    fn pending_context_blocks_run_actions_close_and_late_acknowledgements() {
        let ctx = egui::Context::default();
        let mut app = actions::tests::isolated_app(&ctx);
        let source = DataSource::new(DriverKind::Postgres);
        app.config.data_sources.push(source.clone());
        let i = app.add_tab(
            "query".into(),
            source.id.clone(),
            TabKind::Console { sql: String::new() },
        );
        let id = app.tabs[i].id;
        let requested = ExecutionContext {
            database: None,
            schema: SchemaSelection::Named("sales".into()),
        };
        app.tabs[i].context_pending = Some(requested.clone());
        app.tabs[i].context_revision = 2;
        let captured = app.capture_action_context(&ctx);
        assert!(
            !app.action_availability(super::super::ActionId::Execute, &captured)
                .enabled
        );
        assert!(app.context_change_blocked_reason(id).is_some());
        app.schema_changed(
            id,
            source.id.clone(),
            1,
            requested.clone(),
            Ok(Default::default()),
        );
        assert!(app.tabs[i].context_pending.is_some());
        app.schema_changed(id, source.id, 2, requested, Err("permission denied".into()));
        assert!(app.tabs[i].context_pending.is_none());
        assert_eq!(app.tabs[i].schema, SchemaSelection::Default);
        assert_eq!(app.tabs[i].context_revision, 2);
    }

    #[test]
    fn result_replay_checks_identity_and_allows_switching_back() {
        let mut app = actions::tests::isolated_app(&egui::Context::default());
        let i = app.add_tab(
            "query".into(),
            "source".into(),
            TabKind::Console { sql: String::new() },
        );
        let mut result = ResultSet::new(
            "result".into(),
            "select * from items".into(),
            QueryResult::default(),
            Page::first(500),
        );
        result.origin = Some(ResultOrigin {
            ds_id: "source".into(),
            source: None,
            context: ExecutionContext::default(),
            bound_query: None,
        });
        app.tabs[i].results.push(result);
        assert!(app.result_context_blocked_reason(i, 0).is_none());
        app.tabs[i].schema = SchemaSelection::Named("sales".into());
        assert!(app.result_context_blocked_reason(i, 0).is_some());
        app.tabs[i].schema = SchemaSelection::Default;
        assert!(app.result_context_blocked_reason(i, 0).is_none());
        app.tabs[i].database = Some("other".into());
        assert!(app.result_context_blocked_reason(i, 0).is_some());
    }
    #[test]
    fn result_cannot_replay_on_a_reconfigured_source_with_the_same_id() {
        let mut app = actions::tests::isolated_app(&egui::Context::default());
        let source = DataSource::new(DriverKind::Postgres);
        app.config.data_sources.push(source.clone());
        let index = app.add_tab(
            "query".into(),
            source.id.clone(),
            TabKind::Console { sql: String::new() },
        );
        let mut result = ResultSet::new(
            "result".into(),
            "select 1".into(),
            QueryResult::default(),
            Page::first(500),
        );
        result.origin = Some(ResultOrigin {
            ds_id: source.id.clone(),
            source: Some(source.identity()),
            context: ExecutionContext::default(),
            bound_query: None,
        });
        app.tabs[index].results.push(result);
        assert!(app.result_source_blocked_reason(index, 0).is_none());
        app.config.data_sources[0].host = "other-host".into();
        assert!(app.result_source_blocked_reason(index, 0).is_some());
        assert!(app.result_context_blocked_reason(index, 0).is_some());
    }
}
