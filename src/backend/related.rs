use super::*;
use crate::model::{CapturedRelatedQuery, RelatedDirection};

pub struct RelatedMetadataRequest {
    pub console_id: u64,
    pub result_id: u64,
    pub request_id: u64,
    pub ds: DataSource,
    pub source_identity: SourceIdentity,
    pub expected_context: ResolvedContext,
    pub source: ResultSource,
    pub direction: RelatedDirection,
}

pub struct RelatedSourceGuard {
    pub console_id: u64,
    pub session_epoch: u64,
}

pub struct BoundRunRequest {
    pub console_id: u64,
    pub ds: DataSource,
    pub query: Arc<CapturedRelatedQuery>,
    pub page: Page,
    pub manual_tx: bool,
    pub source_guard: Option<RelatedSourceGuard>,
}

impl Backend {
    pub fn load_related_metadata(&self, request: RelatedMetadataRequest) {
        let send = self.send();
        let session = self.session(&request.ds.id, &request.console_id.to_string());
        self.spawn_metadata(request.ds.id.clone(), async move {
            let mut state = session.lock().await;
            let result = async {
                state.validate_result_session(&request.ds, &request.source_identity, request.source.session_epoch)?;
                let conn = state.conn.as_mut().unwrap();
                ensure_source_context(conn, &request.expected_context).await?;
                anyhow::ensure!(!request.ds.id.starts_with("file:"), "Related Rows requires a persistent database source; imported files are not supported yet.");
                anyhow::ensure!(request.source.columns.len() == request.source.exact_columns.len(), "Incomplete column origin metadata.");
                let mut origins = request.source.columns.iter().flatten();
                let table = origins.next().ok_or_else(|| anyhow::anyhow!("No physical table columns in this result."))?.table.clone();
                anyhow::ensure!(origins.all(|column| column.table == table), "Related Rows currently requires columns from one physical table.");
                let key = (table.clone(), request.direction);
                if let Some(metadata) = state.related_metadata.get(&key) { return Ok(metadata.clone()); }
                let metadata = state.conn.as_mut().unwrap().foreign_keys(&table, request.direction).await?;
                if state.related_metadata.len() >= 256 { state.related_metadata.clear(); }
                state.related_metadata.insert(key, metadata.clone());
                anyhow::Ok(metadata)
            }.await;
            send(Event::RelatedMetadata {
                console_id: request.console_id, result_id: request.result_id,
                request_id: request.request_id, direction: request.direction,
                result: result.map_err(fmt_err),
            });
        });
    }

    /// Stop and Disconnect reach the read while it waits for either session
    /// and while it runs (see [`super::run_stoppable`]).
    pub fn run_bound(&self, request: BoundRunRequest) {
        use super::Stoppable;
        use super::explain::cancel;
        let send = self.send();
        let generated = crate::sqlgen::related_query(request.ds.driver, &request.query);
        let sql = generated.as_ref().map_or_else(|_| String::new(), |query| query.sql.clone());
        let console_id = request.console_id;
        let page = request.page;
        let finish = move |sql: String, result: anyhow::Result<QueryResult>, in_tx: bool| {
            send(Event::StatementDone { console_id, sql, page, result: result.map_err(fmt_err) });
            send(Event::RunFinished { console_id, in_tx });
        };
        let Some((registration, token)) = cancel::Registry::register(&self.query_requests, &request.ds.id, console_id, 0) else {
            finish(sql, Err(anyhow::anyhow!("Another operation is running in this console.")), false);
            return;
        };
        let source = request
            .source_guard
            .as_ref()
            .map(|guard| self.session(&request.ds.id, &guard.console_id.to_string()));
        let session = self.session(&request.ds.id, &console_id.to_string());
        let cancels = self.cancels.clone();
        self.rt.spawn(async move {
            let _registration = registration;
            let stopped = || anyhow::anyhow!("Stopped.");
            // The original and destination mutexes are never held together.
            let source_check = async {
                validate_source(&request.ds, &request.query)?;
                if let (Some(source), Some(guard)) = (source, request.source_guard.as_ref()) {
                    let mut state = cancel::lock(&token, &source).await.ok_or_else(stopped)?;
                    let check = async {
                        state.validate_result_session(&request.ds, &request.query.source_identity, guard.session_epoch)?;
                        let conn = state.conn.as_mut().unwrap();
                        ensure_source_context(conn, &request.query.context).await?;
                        validate_relation(conn, &request.query).await
                    };
                    match super::run_stoppable(&token, check, guard.console_id, &cancels).await {
                        Stoppable::Done(result) => result?,
                        Stoppable::Cancelled(_) | Stoppable::Abandoned => {
                            state.close_after_stop();
                            return Err(anyhow::anyhow!("Stopped. The source console's session was closed."));
                        }
                    }
                }
                anyhow::Ok(())
            }.await;
            let Some(mut state) = cancel::lock(&token, &session).await else {
                finish(sql, Err(stopped()), false);
                return;
            };
            let read = async {
                source_check?;
                let query = generated.map_err(anyhow::Error::msg)?;
                let fresh = state.conn.is_none();
                let conn = state.conn(&request.ds).await?;
                if let Some(handle) = conn.cancel_handle() { cancels.lock().unwrap().insert(console_id, handle); }
                if fresh { conn.apply_resolved_context(&request.query.context).await?; }
                anyhow::ensure!(conn.resolved_context().await? == request.query.context,
                    "The related query's database context has changed. Reopen Related Rows from its source.");
                validate_relation(conn, &request.query).await?;
                state.in_tx = state.conn.as_mut().unwrap().has_open_transaction().await?;
                if request.manual_tx && !state.in_tx {
                    state.conn.as_mut().unwrap().execute_control(begin_statement(request.ds.driver)).await?;
                    state.in_tx = true;
                }
                let conn = state.conn.as_mut().unwrap();
                let keys = order_keys(conn, &request.query).await?;
                let (paged, window) = crate::sqlgen::related_page(request.ds.driver, request.query.direction, &query, &keys, request.page);
                let data = conn.execute_bound_query(&paged, window, Some(&request.query.context)).await?;
                validate_output(&data, &request.query)?;
                anyhow::Ok(data)
            };
            let result = match super::run_stoppable(&token, read, console_id, &cancels).await {
                Stoppable::Done(result) => result,
                Stoppable::Cancelled(_) | Stoppable::Abandoned => {
                    state.close_after_stop();
                    Err(anyhow::anyhow!("Stopped. The session was closed."))
                }
            };
            if result.is_err() {
                state.related_metadata.clear();
                state.edit_metadata.clear();
                state.page_keys.clear();
            }
            cancels.lock().unwrap().remove(&console_id);
            let in_tx = state.in_tx;
            finish(sql, result, in_tx);
        });
    }

    pub fn export_related(
        &self,
        task: super::TaskId,
        ds: DataSource,
        query: Arc<CapturedRelatedQuery>,
        options: crate::export::ExportOptions,
        table: crate::export::TableName,
        path: PathBuf,
    ) {
        self.spawn_task(task, move |sender, token| async move {
            let setup = async {
                validate_source(&ds, &query)?;
                let bound = crate::sqlgen::related_query(ds.driver, &query).map_err(anyhow::Error::msg)?;
                let mut conn = Connection::open(&ds).await?;
                conn.apply_resolved_context(&query.context).await?;
                validate_relation(&mut conn, &query).await?;
                anyhow::ensure!(
                    conn.resolved_context().await? == query.context,
                    "The related query's database context has changed. Reopen Related Rows from its source."
                );
                anyhow::Ok((conn, bound))
            };
            let (mut conn, bound) = tokio::select! {
                result = setup => result?,
                () = token.cancelled() => return Err(crate::drivers::stopped()),
            };
            let item = super::transfer::ExportItem { ds, query: bound, context: None, table, path: path.clone() };
            // Rows stream to disk in bounded batches; the origins of the query
            // that actually runs are checked before the first row is written.
            // export_item handles Stop itself and removes its partial file.
            let check: super::transfer::OriginCheck = Box::new(move |origins| validate_origins(origins, &query));
            let base = super::transfer::Progress { items: 1, ..Default::default() };
            let (rows, _) = super::transfer::export_item(&mut conn, item, options, &sender, &token, base, Some(check)).await?;
            Ok(format!("{} exported to {}", super::rows_text(rows), path.display()))
        });
    }
}

pub(super) fn validate_source(ds: &DataSource, query: &CapturedRelatedQuery) -> anyhow::Result<()> {
    crate::related::validate_query(ds.driver, query).map_err(anyhow::Error::msg)?;
    anyhow::ensure!(
        ds.identity() == query.source_identity,
        "The data source definition has changed. Reopen Related Rows from its source."
    );
    anyhow::ensure!(
        !ds.id.starts_with("file:"),
        "Related Rows requires a persistent database source; imported files are not supported yet."
    );
    Ok(())
}

async fn ensure_source_context(
    conn: &mut Connection,
    expected: &ResolvedContext,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        conn.resolved_context().await? == *expected,
        "The source context has changed. Execute the query again before opening Related Rows."
    );
    anyhow::ensure!(
        expected.temporary_schema.is_none(),
        "Related Rows opens a separate session and cannot reproduce temporary objects."
    );
    anyhow::ensure!(
        !conn.has_open_transaction().await?,
        "Commit or roll back the source transaction before opening Related Rows."
    );
    Ok(())
}

/// Columns that give related pages a stable order: the output table's primary
/// key, else the referenced key, which is unique. Read fresh for every page.
async fn order_keys(
    conn: &mut Connection,
    query: &CapturedRelatedQuery,
) -> anyhow::Result<Vec<String>> {
    let table = match query.direction {
        RelatedDirection::Referenced => &query.relation.target,
        RelatedDirection::Referencing => &query.relation.source,
    };
    let keys = conn.primary_key(Some(&table.schema), &table.name).await?;
    if keys.is_empty() && query.direction == RelatedDirection::Referenced {
        return Ok(query
            .relation
            .columns
            .iter()
            .map(|pair| pair.target_name.clone())
            .collect());
    }
    Ok(keys)
}

async fn validate_relation(
    conn: &mut Connection,
    query: &CapturedRelatedQuery,
) -> anyhow::Result<()> {
    let table = match query.direction {
        RelatedDirection::Referenced => &query.relation.source,
        RelatedDirection::Referencing => &query.relation.target,
    };
    let origin = table.oid.map_or_else(
        || OriginTable::Named {
            schema: table.schema.clone(),
            table: table.name.clone(),
        },
        OriginTable::Postgres,
    );
    // Always fresh: a cached chooser is never authority to replay a dropped or changed FK.
    let metadata = conn.foreign_keys(&origin, query.direction).await?;
    anyhow::ensure!(
        metadata.table == *table && metadata.keys.iter().any(|key| key == &query.relation),
        "The foreign key has changed or no longer exists. Reopen Related Rows from its source."
    );
    Ok(())
}

fn validate_output(data: &QueryResult, query: &CapturedRelatedQuery) -> anyhow::Result<()> {
    let source = data
        .source
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Related Rows did not return physical column origins."))?;
    anyhow::ensure!(
        source.columns.len() == data.columns.len(),
        "The related query resolved to another physical table. Reopen Related Rows from its source."
    );
    validate_origins(&source.columns, query)
}

fn validate_origins(
    origins: &[Option<crate::model::ColumnOrigin>],
    query: &CapturedRelatedQuery,
) -> anyhow::Result<()> {
    let table = match query.direction {
        RelatedDirection::Referenced => &query.relation.target,
        RelatedDirection::Referencing => &query.relation.source,
    };
    anyhow::ensure!(
        !origins.is_empty()
            && origins
                .iter()
                .all(
                    |column| column.as_ref().is_some_and(|column| match &column.table {
                        OriginTable::Postgres(oid) => table.oid == Some(*oid),
                        OriginTable::Named {
                            schema,
                            table: name,
                        } => *schema == table.schema && *name == table.name,
                    })
                ),
        "The related query resolved to another physical table. Reopen Related Rows from its source."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ColumnOrigin, OriginColumn};

    #[test]
    fn streamed_exports_check_every_column_origin() {
        let (data, relation, identity, context) = crate::related::tests::fixture();
        let query = crate::related::capture(&data, 0, &relation, RelatedDirection::Referenced, identity, context).unwrap();
        let origin = |table: &str| {
            Some(ColumnOrigin {
                table: OriginTable::Named { schema: "main".into(), table: table.into() },
                column: OriginColumn::Named("id".into()),
            })
        };
        assert!(validate_origins(&[origin("accounts"), origin("accounts")], &query).is_ok());
        for origins in [vec![], vec![origin("accounts"), None], vec![origin("accounts"), origin("invoices")]] {
            assert!(validate_origins(&origins, &query).is_err(), "{origins:?}");
        }
    }
}
