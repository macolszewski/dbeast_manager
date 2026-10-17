//! Related result tabs retain the same immutable query through every replay.

use super::*;
use crate::backend::{BoundRunRequest, RelatedSourceGuard};
use crate::model::{CapturedRelatedQuery, RelatedDirection};
use std::sync::Arc;

impl App {
    pub(super) fn open_related_query(
        &mut self,
        parent: usize,
        query: Arc<CapturedRelatedQuery>,
        source_guard: RelatedSourceGuard,
    ) {
        let Some(tab) = self.tabs.get(parent) else {
            return;
        };
        if tab.id != source_guard.console_id
            || tab.is_running()
            || tab.context_pending.is_some()
            || tab.in_tx
            || tab
                .results
                .iter()
                .any(|result| result.action_state().has_changes)
        {
            self.notice = Some(ServicesNotice::error("The source result has changed. Reopen Related Rows after submitting or reverting pending changes.".into()));
            return;
        }
        let generated = match crate::sqlgen::related_query(query.source_identity.driver, &query) {
            Ok(generated) => generated,
            Err(reason) => {
                self.notice = Some(ServicesNotice::error(reason));
                return;
            }
        };
        let source_id = tab.ds_id.clone();
        let database = tab.database.clone();
        let schema = tab.schema.clone();
        let table = match query.direction {
            RelatedDirection::Referenced => &query.relation.target,
            RelatedDirection::Referencing => &query.relation.source,
        };
        let title = format!("Related: {}.{}", table.schema, table.name);
        let index = self.add_tab(title, source_id, TabKind::Console { sql: generated.sql });
        let tab = &mut self.tabs[index];
        tab.database = database;
        tab.schema = schema;
        tab.bound_query = Some(query.clone());
        self.update_metadata_key(index);
        self.start_related_run(
            index,
            query,
            Page::first(self.config.max_rows),
            RunKind::Query,
            Some(source_guard),
        );
    }

    pub(super) fn start_related_run(
        &mut self,
        index: usize,
        query: Arc<CapturedRelatedQuery>,
        page: Page,
        kind: RunKind,
        source_guard: Option<RelatedSourceGuard>,
    ) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        if tab.is_running() || tab.context_pending.is_some() || self.source_is_saving(&tab.ds_id) {
            return;
        }
        if tab
            .results
            .iter()
            .any(|result| result.action_state().has_changes)
        {
            self.notice = Some(ServicesNotice::error(
                "Submit or revert pending grid changes before executing another query.".into(),
            ));
            return;
        }
        let request = (|| -> Result<_, String> {
            let mut ds = self
                .source(&tab.ds_id)
                .cloned()
                .ok_or("The data source is no longer available.")?;
            if ds.driver == DriverKind::Postgres
                && let Some(database) = &tab.database
            {
                ds = ds.with_database(database);
            }
            if ds.identity() != query.source_identity {
                return Err(
                    "The data source definition has changed. Reopen Related Rows from its source."
                        .into(),
                );
            }
            let generated = crate::sqlgen::related_query(ds.driver, &query)?;
            Ok((ds, generated))
        })();
        let (ds, generated) = match request {
            Ok(request) => request,
            Err(reason) => {
                self.tabs[index].log(LogKind::Error, &reason);
                self.notice = Some(ServicesNotice::error(reason));
                return;
            }
        };
        self.related_rows.cancel();
        self.notice = None;
        let tab = &mut self.tabs[index];
        if kind == RunKind::Query {
            tab.results.clear();
            tab.view = ServicesView::Output;
        }
        tab.log(
            LogKind::Sql,
            format!(
                "{}> {} [{} parameters]",
                ds.display_name(),
                generated.sql,
                generated.params.len()
            ),
        );
        tab.running = Some(kind);
        tab.running_origin = Some(results::ResultOrigin {
            ds_id: tab.ds_id.clone(),
            source: Some(ds.identity()),
            context: ExecutionContext {
                database: tab.database.clone(),
                schema: tab.schema.clone(),
            },
            bound_query: Some(query.clone()),
        });
        self.backend.run_bound(BoundRunRequest {
            console_id: tab.id,
            ds,
            query,
            page,
            manual_tx: tab.manual_tx,
            source_guard,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_source_and_missing_page_parameters_cannot_fall_back_to_raw_sql() {
        let (data, relation, identity, context) = crate::related::tests::fixture();
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
        let mut app = super::super::actions::tests::isolated_app(&egui::Context::default());
        app.config.data_sources.push(ds.clone());
        let index = app.add_tab(
            "Related".into(),
            ds.id.clone(),
            TabKind::Console {
                sql: "SELECT ?".into(),
            },
        );
        app.tabs[index].bound_query = Some(query);
        app.tabs[index].results.push(ResultSet::new(
            "old result".into(),
            "SELECT ?".into(),
            data,
            Page::first(5),
        ));
        app.start_run(
            index,
            vec!["SELECT ?".into()],
            Page::first(5),
            RunKind::Page(0),
        );
        assert!(app.tabs[index].running.is_none());
        assert!(app.notice.as_ref().unwrap().text.contains("parameters"));
        app.config.data_sources[0].file = "/tmp/different-related-source.sqlite".into();
        app.run_tab(index, false);
        assert!(app.tabs[index].running.is_none());
        assert_eq!(app.tabs[index].results.len(), 1);
        assert!(
            app.notice
                .as_ref()
                .unwrap()
                .text
                .contains("definition has changed")
        );
        assert!(app.backend.rx.try_recv().is_err());
    }
}
