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

    pub fn run_bound(&self, request: BoundRunRequest) {
        let send = self.send();
        let source = request
            .source_guard
            .as_ref()
            .map(|guard| self.session(&request.ds.id, &guard.console_id.to_string()));
        let session = self.session(&request.ds.id, &request.console_id.to_string());
        let cancels = self.cancels.clone();
        self.rt.spawn(async move {
            let generated = crate::sqlgen::related_query(request.ds.driver, &request.query);
            let sql = generated.as_ref().map_or_else(|_| String::new(), |query| query.sql.clone());
            // The original and destination mutexes are never held together.
            let source_check = async {
                validate_source(&request.ds, &request.query)?;
                if let (Some(source), Some(guard)) = (source, request.source_guard.as_ref()) {
                    let mut state = source.lock().await;
                    state.validate_result_session(&request.ds, &request.query.source_identity, guard.session_epoch)?;
                    let conn = state.conn.as_mut().unwrap();
                    ensure_source_context(conn, &request.query.context).await?;
                    validate_relation(conn, &request.query).await?;
                }
                anyhow::Ok(())
            }.await;
            let mut state = session.lock().await;
            let result = async {
                source_check?;
                let query = generated.map_err(anyhow::Error::msg)?;
                let fresh = state.conn.is_none();
                let conn = state.conn(&request.ds).await?;
                if let Some(handle) = conn.cancel_handle() { cancels.lock().unwrap().insert(request.console_id, handle); }
                if fresh { conn.apply_resolved_context(&request.query.context).await?; }
                anyhow::ensure!(conn.resolved_context().await? == request.query.context,
                    "The related query's database context has changed. Reopen Related Rows from its source.");
                validate_relation(conn, &request.query).await?;
                state.in_tx = state.conn.as_mut().unwrap().has_open_transaction().await?;
                if request.manual_tx && !state.in_tx {
                    state.conn.as_mut().unwrap().execute_control(begin_statement(request.ds.driver)).await?;
                    state.in_tx = true;
                }
                let data = state.conn.as_mut().unwrap().execute_bound_query(&query, request.page, Some(&request.query.context)).await?;
                validate_output(&data, &request.query)?;
                anyhow::Ok(data)
            }.await;
            if result.is_err() {
                state.related_metadata.clear();
                state.edit_metadata.clear();
            }
            cancels.lock().unwrap().remove(&request.console_id);
            send(Event::StatementDone { console_id: request.console_id, sql, page: request.page, result: result.map_err(fmt_err) });
            send(Event::RunFinished { console_id: request.console_id, in_tx: state.in_tx });
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
            let work = async {
                validate_source(&ds, &query)?;
                let bound =
                    crate::sqlgen::related_query(ds.driver, &query).map_err(anyhow::Error::msg)?;
                let mut conn = Connection::open(&ds).await?;
                conn.apply_resolved_context(&query.context).await?;
                validate_relation(&mut conn, &query).await?;
                // Related results are validated as a whole before anything is written.
                let data = conn
                    .execute_bound_query(&bound, Page::ALL, Some(&query.context))
                    .await?;
                validate_output(&data, &query)?;
                let rows = data.rows.len() as u64;
                let driver = ds.driver;
                let target = super::transfer::partial_path(&path);
                let (partial, final_path) = (target.clone(), path.clone());
                tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                    let file = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(&target)?);
                    let mut writer = crate::export::Writer::new(file, options, driver, table);
                    writer.begin(&data.columns)?;
                    writer.rows(&data.rows)?;
                    writer.finish()?;
                    Ok(())
                })
                .await?
                .and_then(|()| Ok(std::fs::rename(&partial, &final_path)?))
                .inspect_err(|_| {
                    let _ = std::fs::remove_file(&partial);
                })?;
                anyhow::Ok(rows)
            };
            let rows = tokio::select! {
                result = work => result,
                () = token.cancelled() => Err(crate::drivers::stopped()),
            };
            match rows {
                Ok(rows) => {
                    sender.log(format!("{} written to {}", super::rows_text(rows), path.display()));
                    Ok(format!("{} exported to {}", super::rows_text(rows), path.display()))
                }
                Err(error) => {
                    let _ = std::fs::remove_file(super::transfer::partial_path(&path));
                    Err(error)
                }
            }
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
    let table = match query.direction {
        RelatedDirection::Referenced => &query.relation.target,
        RelatedDirection::Referencing => &query.relation.source,
    };
    let source = data
        .source
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Related Rows did not return physical column origins."))?;
    anyhow::ensure!(
        source.columns.len() == data.columns.len()
            && !source.columns.is_empty()
            && source
                .columns
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
