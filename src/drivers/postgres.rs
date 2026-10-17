use super::{ServerInfo, schema_folders, table_folders, table_kind};
use crate::model::{
    BoundQuery, BoundUpdate, Column, ColumnOrigin, DataSource, DbObject, ForeignKey,
    ForeignKeyColumn, ObjectKind, OriginColumn, OriginTable, Page, PhysicalColumn, PhysicalTable,
    QueryResult, RelatedDirection, RelatedMetadata, ResolvedContext, ResultSource, SchemaSelection,
    TableEditMetadata, TableMeta, Value,
};
use anyhow::{Context, Result};
use std::error::Error;
use tokio_postgres::types::{FromSql, Type};
use tokio_postgres::{Client, Config, Row, Statement};
pub(super) mod graph;
pub(super) mod mcp;

const CONTEXT_SQL: &str = "select pg_catalog.current_database(), \
    pg_catalog.current_setting('search_path'), pg_catalog.current_schemas(false), \
    (select nspname from pg_catalog.pg_namespace where oid = pg_catalog.pg_my_temp_schema())";

/// Detail of the database node a connection was opened to.
pub const DEFAULT_DATABASE: &str = "default";

pub struct PgConn {
    client: Client,
    // Dropping Client alone can leave an in-flight query running in a detached
    // connection task. Abort the driver task to close an uncertain session.
    _connection_task: ConnectionTask,
    tls: Option<native_tls::TlsConnector>,
    tls_mode: crate::model::TlsMode,
    context_statement: Statement,
    initial_search_path: String,
    update_statements: std::collections::HashMap<String, Statement>,
    mcp_endpoint: Option<(String, u16, String)>,
}

struct ConnectionTask(tokio::task::JoinHandle<Result<(), tokio_postgres::Error>>);

impl Drop for ConnectionTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(super) struct PreparedExplain {
    pub(super) statement: Statement,
    pub(super) context: ResolvedContext,
}

impl PgConn {
    pub(super) fn clear_prepared_updates(&mut self) {
        self.update_statements.clear();
    }

    pub(super) async fn prepare_explain(&self, sql: &str) -> Result<PreparedExplain> {
        // Drain both pipelined operations, including when one is cancelled.
        let (context, statement) = tokio::join!(biased;
            self.resolved_context(), self.client.prepare(sql));
        let context = context?;
        let statement = statement?;
        anyhow::ensure!(
            !statement.columns().is_empty(),
            "EXPLAIN did not return a plan."
        );
        Ok(PreparedExplain { statement, context })
    }

    pub(super) async fn execute_explain(&self, prepared: PreparedExplain, params: &[Value], expected: Option<&ResolvedContext>) -> Result<QueryResult> {
        super::check_expected_context(Some(&prepared.context), expected)?;
        anyhow::ensure!(prepared.statement.params().len() == params.len(), "EXPLAIN parameter count mismatch.");
        self.query_prepared(&prepared.statement, params, Page::ALL, Some(prepared.context))
            .await
    }

    pub async fn open(ds: &DataSource, endpoint: &super::Endpoint) -> Result<Self> {
        let mut cfg = Config::new();
        // Through a tunnel the socket goes to the local port, while TLS still
        // verifies the certificate against the real host name.
        cfg.host(&ds.host)
            .port(endpoint.port)
            .user(&ds.user)
            .application_name("DBeast");
        if endpoint.tunneled {
            cfg.hostaddr(endpoint.host.parse()?);
        }
        if !ds.password.is_empty() {
            cfg.password(&ds.password);
        }
        if !ds.database.is_empty() {
            cfg.dbname(&ds.database);
        }
        cfg.connect_timeout(std::time::Duration::from_secs(10));
        cfg.keepalives(true)
            .keepalives_idle(std::time::Duration::from_secs(30))
            .keepalives_interval(std::time::Duration::from_secs(10))
            .keepalives_retries(3)
            .tcp_user_timeout(std::time::Duration::from_secs(60));

        cfg.ssl_mode(if ds.use_tls { tokio_postgres::config::SslMode::Require } else { tokio_postgres::config::SslMode::Disable });
        let tls = super::tls::prepare(ds).await?.and_then(|material| material.connector);
        let (client, connection_task) = if let Some(tls) = &tls {
            let (client, conn) = cfg
                .connect(postgres_native_tls::MakeTlsConnector::new(tls.clone()))
                .await?;
            (client, ConnectionTask(tokio::spawn(conn)))
        } else {
            let (client, conn) = cfg.connect(tokio_postgres::NoTls).await?;
            (client, ConnectionTask(tokio::spawn(conn)))
        };
        if ds.read_only {
            client
                .batch_execute("SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY")
                .await?;
        }
        let context_statement = client.prepare(CONTEXT_SQL).await?;
        let initial_search_path = client.query_one(&context_statement, &[]).await?.get(1);
        Ok(Self {
            client,
            _connection_task: connection_task,
            tls,
            tls_mode: ds.tls.mode,
            context_statement,
            initial_search_path,
            update_statements: Default::default(),
            mcp_endpoint: None,
        })
    }

    pub fn cancel_handle(&self) -> super::CancelHandle {
        let token = self.client.cancel_token();
        if let Some((host, port, server_name)) = &self.mcp_endpoint {
            return super::CancelHandle::McpPostgres(mcp::McpCancel {
                token,
                host: host.clone(),
                port: *port,
                server_name: server_name.clone(),
                tls: self.tls.clone(),
            });
        }
        if let Some(tls) = &self.tls {
            super::CancelHandle::PostgresTls(token, tls.clone())
        } else {
            super::CancelHandle::Postgres(token)
        }
    }

    pub async fn server_info(&mut self) -> Result<ServerInfo> {
        let row = self
            .client
            .query_one("select current_setting('server_version')", &[])
            .await?;
        let encrypted: bool = self.client.query_one(
            "select coalesce((select ssl from pg_catalog.pg_stat_ssl where pid = pg_backend_pid()), false)", &[]).await?.get(0);
        anyhow::ensure!(self.tls.is_none() || encrypted, "Server did not confirm TLS encryption");
        Ok(ServerInfo {
            product: "PostgreSQL".into(),
            version: row.get(0),
            transport: if encrypted { self.tls_mode.transport() } else { "Unencrypted" }.into(),
        })
    }

    pub async fn execute(&mut self, sql: &str, page: Page) -> Result<QueryResult> {
        self.execute_inner(sql, page, false, None).await
    }

    pub async fn execute_with_context(
        &mut self,
        sql: &str,
        page: Page,
        expected: Option<&ResolvedContext>,
    ) -> Result<QueryResult> {
        self.execute_inner(sql, page, true, expected).await
    }

    async fn execute_inner(
        &mut self,
        sql: &str,
        page: Page,
        capture: bool,
        expected: Option<&ResolvedContext>,
    ) -> Result<QueryResult> {
        // The snapshot and the existing prepare are pipelined. Poll the snapshot
        // first so it describes the namespace before parsing/executing user SQL.
        // Do not short-circuit on a snapshot error: ROLLBACK must still execute
        // in an aborted transaction, where the context SELECT is rejected.
        let (stmt, context) = if capture {
            let (context, stmt) = tokio::join!(biased;
                self.resolved_context(),
                self.client.prepare(sql)
            );
            let stmt = stmt?;
            let context = if stmt.columns().is_empty() && expected.is_none() {
                context.ok()
            } else {
                Some(context?)
            };
            (stmt, context)
        } else {
            (self.client.prepare(sql).await?, None)
        };
        super::check_expected_context(context.as_ref(), expected)?;
        if stmt.columns().is_empty() {
            self.update_statements.clear();
            let affected = self.client.execute(&stmt, &[]).await?;
            return Ok(QueryResult {
                affected: Some(affected),
                ..Default::default()
            });
        }

        self.query_prepared(&stmt, &[], page, context).await
    }

    async fn query_prepared(
        &self,
        stmt: &Statement,
        params: &[Value],
        page: Page,
        context: Option<ResolvedContext>,
    ) -> Result<QueryResult> {
        let origins = stmt
            .columns()
            .iter()
            .map(|column| {
                Some(ColumnOrigin {
                    table: OriginTable::Postgres(column.table_oid()?),
                    column: OriginColumn::Postgres(column.column_id()?),
                })
            })
            .collect();
        let mut exact_columns = vec![true; stmt.columns().len()];
        let columns = stmt
            .columns()
            .iter()
            .map(|c| Column {
                name: c.name().to_string(),
                type_name: c.type_().name().to_string(),
            })
            .collect();
        let inputs: Vec<_> = params.iter().map(PgInput).collect();
        let rows = self
            .client
            .query_raw(
                stmt,
                inputs
                    .iter()
                    .map(|value| value as &(dyn tokio_postgres::types::ToSql + Sync)),
            )
            .await?;
        let (rows, truncated) = collect_rows(rows, page, &mut exact_columns).await?;
        Ok(QueryResult {
            columns,
            rows,
            truncated,
            context,
            source: Some(ResultSource {
                session_epoch: 0,
                columns: origins,
                exact_columns,
            }),
            ..Default::default()
        })
    }

    pub async fn copy_text_rows(&self, target: &super::InsertTarget, rows: &[Vec<Option<String>>]) -> Result<()> {
        use futures_util::SinkExt;
        let sql = format!(
            "COPY {} ({}) FROM STDIN WITH (FORMAT csv)",
            target.qualified('"'),
            target.column_list('"')
        );
        let sink = self.client.copy_in::<_, bytes::Bytes>(&sql).await?;
        let mut sink = std::pin::pin!(sink);
        // In COPY CSV an unquoted empty field is NULL and a quoted one is text.
        let mut buffer = Vec::with_capacity(1 << 20);
        for row in rows {
            for (i, value) in row.iter().enumerate() {
                if i > 0 {
                    buffer.push(b',');
                }
                if let Some(text) = value {
                    buffer.push(b'"');
                    for &b in text.as_bytes() {
                        if b == b'"' {
                            buffer.push(b'"');
                        }
                        buffer.push(b);
                    }
                    buffer.push(b'"');
                }
            }
            buffer.push(b'\n');
            if buffer.len() >= 1 << 20 {
                sink.send(bytes::Bytes::from(std::mem::take(&mut buffer))).await?;
            }
        }
        if !buffer.is_empty() {
            sink.send(bytes::Bytes::from(buffer)).await?;
        }
        sink.as_mut().finish().await?;
        Ok(())
    }

    pub async fn stream_rows(
        &self,
        query: &BoundQuery,
        tx: tokio::sync::mpsc::Sender<super::RowBatch>,
    ) -> Result<u64> {
        use futures_util::StreamExt;
        let statement = self.client.prepare(&query.sql).await?;
        anyhow::ensure!(!statement.columns().is_empty(), "The statement returns no rows.");
        let columns = statement
            .columns()
            .iter()
            .map(|c| Column {
                name: c.name().to_string(),
                type_name: c.type_().name().to_string(),
            })
            .collect();
        tx.send(super::RowBatch::Columns(columns)).await.map_err(|_| super::stopped())?;
        let inputs: Vec<_> = query.params.iter().map(PgInput).collect();
        let stream = self
            .client
            .query_raw(
                &statement,
                inputs
                    .iter()
                    .map(|value| value as &(dyn tokio_postgres::types::ToSql + Sync)),
            )
            .await?;
        let mut stream = std::pin::pin!(stream);
        let mut exact = vec![true; statement.columns().len()];
        let mut batch = Vec::with_capacity(super::STREAM_BATCH);
        let mut total = 0u64;
        while let Some(row) = stream.next().await {
            batch.push(row_values(&row?, &mut exact));
            if batch.len() == super::STREAM_BATCH {
                total += batch.len() as u64;
                let full = std::mem::replace(&mut batch, Vec::with_capacity(super::STREAM_BATCH));
                tx.send(super::RowBatch::Rows(full)).await.map_err(|_| super::stopped())?;
            }
        }
        total += batch.len() as u64;
        if !batch.is_empty() {
            tx.send(super::RowBatch::Rows(batch)).await.map_err(|_| super::stopped())?;
        }
        Ok(total)
    }

    pub async fn execute_bound_query(
        &self,
        query: &BoundQuery,
        page: Page,
        expected: Option<&ResolvedContext>,
    ) -> Result<QueryResult> {
        let (context, statement) =
            tokio::join!(biased; self.resolved_context(), self.client.prepare(&query.sql));
        let context = context?;
        super::check_expected_context(Some(&context), expected)?;
        let statement = statement?;
        anyhow::ensure!(
            !statement.columns().is_empty(),
            "A related query must return rows."
        );
        self.query_prepared(&statement, &query.params, page, Some(context))
            .await
    }

    pub async fn foreign_keys(
        &self,
        origin: &OriginTable,
        direction: RelatedDirection,
    ) -> Result<RelatedMetadata> {
        let OriginTable::Postgres(oid) = origin else {
            anyhow::bail!("The result does not contain PostgreSQL table provenance.");
        };
        let selected = self.client.query_opt(
            "select n.nspname,c.relname,c.relkind::text,c.relpersistence::text,exists(select 1 from pg_catalog.pg_inherits i where i.inhrelid=c.oid or i.inhparent=c.oid) from pg_catalog.pg_class c join pg_catalog.pg_namespace n on n.oid=c.relnamespace where c.oid=$1", &[oid]).await?
            .ok_or_else(|| anyhow::anyhow!("The source table no longer exists."))?;
        anyhow::ensure!(
            selected.get::<_, String>(2) == "r"
                && selected.get::<_, String>(3) != "t"
                && !selected.get::<_, bool>(4),
            "Related Rows currently supports persistent ordinary PostgreSQL tables without inheritance or partitioning."
        );
        let table = PhysicalTable {
            schema: selected.get(0),
            name: selected.get(1),
            oid: Some(*oid),
        };
        let filter = match direction {
            RelatedDirection::Referenced => "con.conrelid",
            RelatedDirection::Referencing => "con.confrelid",
        };
        let sql = format!("select con.oid,con.conname,sn.nspname,src.relname,src.oid,dn.nspname,dst.relname,dst.oid,
            src.relkind='r' and dst.relkind='r' and src.relpersistence<>'t' and dst.relpersistence<>'t'
              and not exists(select 1 from pg_catalog.pg_inherits i where i.inhrelid in(src.oid,dst.oid) or i.inhparent in(src.oid,dst.oid)),
            k.ord,sa.attnum,sa.attname,st.typname,da.attnum,da.attname,dt.typname
            from pg_catalog.pg_constraint con join pg_catalog.pg_class src on src.oid=con.conrelid
            join pg_catalog.pg_namespace sn on sn.oid=src.relnamespace join pg_catalog.pg_class dst on dst.oid=con.confrelid
            join pg_catalog.pg_namespace dn on dn.oid=dst.relnamespace
            cross join lateral pg_catalog.unnest(con.conkey) with ordinality k(source_attnum,ord)
            join lateral pg_catalog.unnest(con.confkey) with ordinality f(target_attnum,ord) on f.ord=k.ord
            join pg_catalog.pg_attribute sa on sa.attrelid=src.oid and sa.attnum=k.source_attnum
            join pg_catalog.pg_attribute da on da.attrelid=dst.oid and da.attnum=f.target_attnum
            join pg_catalog.pg_type st on st.oid=sa.atttypid join pg_catalog.pg_type dt on dt.oid=da.atttypid
            where con.contype='f' and {filter}=$1 order by con.oid,k.ord");
        let rows = self.client.query(&sql, &[oid]).await?;
        let mut keys: Vec<ForeignKey> = Vec::new();
        for row in rows {
            anyhow::ensure!(
                row.get::<_, bool>(8),
                "A related table is temporary, inherited, partitioned, or not an ordinary table."
            );
            let id = row.get::<_, u32>(0).to_string();
            if keys.last().is_none_or(|key| key.catalog_id != id) {
                keys.push(ForeignKey {
                    name: row.get(1),
                    catalog_id: id,
                    source: PhysicalTable {
                        schema: row.get(2),
                        name: row.get(3),
                        oid: Some(row.get(4)),
                    },
                    target: PhysicalTable {
                        schema: row.get(5),
                        name: row.get(6),
                        oid: Some(row.get(7)),
                    },
                    columns: Vec::new(),
                });
            }
            keys.last_mut().unwrap().columns.push(ForeignKeyColumn {
                source_name: row.get(11),
                source_origin: OriginColumn::Postgres(row.get(10)),
                source_type: row.get(12),
                target_name: row.get(14),
                target_origin: OriginColumn::Postgres(row.get(13)),
                target_type: row.get(15),
            });
        }
        Ok(RelatedMetadata { table, keys })
    }

    pub async fn execute_control(&self, sql: &str) -> Result<()> {
        Ok(self.client.batch_execute(sql).await?)
    }

    pub async fn table_edit_metadata(&self, origin: &OriginTable) -> Result<TableEditMetadata> {
        let OriginTable::Postgres(oid) = origin else {
            anyhow::bail!("The result does not contain PostgreSQL table provenance.");
        };
        let table = self.client.query_opt(
            "select n.nspname, c.relname, c.relkind::text, \
             exists(select 1 from pg_catalog.pg_inherits i where i.inhrelid=c.oid or i.inhparent=c.oid) \
             from pg_catalog.pg_class c join pg_catalog.pg_namespace n on n.oid=c.relnamespace \
             where c.oid=$1", &[oid]).await?
            .ok_or_else(|| anyhow::anyhow!("The result's source table no longer exists."))?;
        anyhow::ensure!(
            table.get::<_, String>(2) == "r" && !table.get::<_, bool>(3),
            "Only ordinary tables without inheritance can be edited from a console result."
        );
        let schema: String = table.get(0);
        let name: String = table.get(1);
        let rows = self.client.query(
            "select a.attname, a.attnum, a.attnotnull, a.attgenerated::text, a.attidentity::text, \
             t.typname, t.typtype::text from pg_catalog.pg_attribute a \
             join pg_catalog.pg_type t on t.oid=a.atttypid \
             where a.attrelid=$1 and a.attnum>0 and not a.attisdropped order by a.attnum", &[oid]).await?;
        let columns = rows
            .iter()
            .map(|row| {
                let generated =
                    !row.get::<_, String>(3).is_empty() || !row.get::<_, String>(4).is_empty();
                PhysicalColumn {
                    name: row.get(0),
                    ordinal: row.get::<_, i16>(1) as u32,
                    nullable: !row.get::<_, bool>(2),
                    generated,
                    update_default: !generated,
                    comparable: editable_pg_type(
                        &row.get::<_, String>(5),
                        row.get::<_, String>(6) == "e",
                    ),
                }
            })
            .collect();
        let keys = self
            .client
            .query(
                "select a.attname from pg_catalog.pg_index i \
             join lateral unnest(i.indkey) with ordinality k(attnum, ordinal) on k.ordinal<=i.indnkeyatts \
             join pg_catalog.pg_attribute a on a.attrelid=i.indrelid and a.attnum=k.attnum \
             where i.indrelid=$1 and i.indisprimary order by k.ordinal",
                &[oid],
            )
            .await?;
        Ok(TableEditMetadata {
            table: PhysicalTable {
                schema,
                name,
                oid: Some(*oid),
            },
            columns,
            primary_key: keys.iter().map(|row| row.get(0)).collect(),
            transactional: true,
        })
    }

    pub async fn execute_bound_update(&mut self, update: &BoundUpdate) -> Result<u64> {
        let statement = if let Some(statement) = self.update_statements.get(&update.sql) {
            statement.clone()
        } else {
            let statement = self.client.prepare(&update.sql).await?;
            if self.update_statements.len() >= 128 {
                self.update_statements.clear();
            }
            self.update_statements
                .insert(update.sql.clone(), statement.clone());
            statement
        };
        let values: Vec<_> = update.params.iter().map(PgInput).collect();
        let params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = values
            .iter()
            .map(|value| value as &(dyn tokio_postgres::types::ToSql + Sync))
            .collect();
        Ok(self.client.execute(&statement, &params).await?)
    }

    pub async fn resolved_context(&self) -> Result<ResolvedContext> {
        let row = self.client.query_one(&self.context_statement, &[]).await?;
        Ok(ResolvedContext {
            database: Some(row.get(0)),
            search_path: Some(row.get(1)),
            schemas: row.get(2),
            temporary_schema: row.get(3),
        })
    }

    pub async fn schema_names(&self) -> Result<Vec<String>> {
        let rows = self
            .client
            .query(
                "select nspname from pg_catalog.pg_namespace \
             where nspname not like 'pg\\_%' escape '\\' \
             and nspname <> 'information_schema' \
             and pg_catalog.has_schema_privilege(oid, 'USAGE') order by nspname",
                &[],
            )
            .await?;
        Ok(rows.iter().map(|row| row.get(0)).collect())
    }

    pub async fn change_schema(&mut self, selection: &SchemaSelection) -> Result<ResolvedContext> {
        let previous = self.resolved_context().await?;
        let path = match selection {
            SchemaSelection::Default => self.initial_search_path.clone(),
            SchemaSelection::Named(name) => {
                let exists = self
                    .client
                    .query_opt(
                        "select nspname from pg_catalog.pg_namespace \
                     where nspname = $1 and pg_catalog.has_schema_privilege(oid, 'USAGE')",
                        &[name],
                    )
                    .await?
                    .is_some();
                anyhow::ensure!(
                    exists,
                    "Schema {name:?} does not exist or is not accessible."
                );
                super::quote_ident(name, '"')
            }
        };
        self.set_search_path(&path).await?;
        let resolved = self.resolved_context().await?;
        if let SchemaSelection::Named(name) = selection
            && resolved.schemas.first() != Some(name)
        {
            // `$user` is special even inside a quoted search_path element;
            // permissions may also change between validation and SET.
            if let Some(previous) = &previous.search_path {
                self.set_search_path(previous).await?;
            }
            anyhow::bail!("Schema {name:?} could not become the current schema.");
        }
        Ok(resolved)
    }

    pub async fn apply_resolved_context(&mut self, context: &ResolvedContext) -> Result<()> {
        let actual = self.resolved_context().await?;
        anyhow::ensure!(
            actual.database == context.database,
            "The connection belongs to a different database."
        );
        if let Some(path) = &context.search_path {
            self.set_search_path(path).await?;
        }
        super::check_expected_context(Some(&self.resolved_context().await?), Some(context))?;
        Ok(())
    }

    async fn set_search_path(&self, path: &str) -> Result<()> {
        self.client
            .query_one(
                "select pg_catalog.set_config('search_path', $1, false)",
                &[&path],
            )
            .await?;
        Ok(())
    }

    pub async fn children(
        &mut self,
        kind: Option<ObjectKind>,
        path: &[String],
    ) -> Result<Vec<DbObject>> {
        match kind {
            None => {
                // Every database the account may connect to; the explorer shows the chosen ones.
                let rows = self
                    .client
                    .query(
                        "select datname, datname = current_database()
                           from pg_catalog.pg_database
                          where not datistemplate and datallowconn
                            and has_database_privilege(datname, 'CONNECT')
                          order by datname = current_database() desc, datname",
                        &[],
                    )
                    .await?;
                Ok(rows
                    .iter()
                    .map(|r| {
                        let node = DbObject::new(ObjectKind::Database, r.get::<_, String>(0), path);
                        if r.get::<_, bool>(1) {
                            node.with_detail(DEFAULT_DATABASE)
                        } else {
                            node
                        }
                    })
                    .collect())
            }
            Some(ObjectKind::Database) => self.schemas(path).await,
            Some(ObjectKind::Schema) => self.schema_objects(path).await,
            Some(ObjectKind::Table | ObjectKind::View) => self.table_details(path).await,
            _ => Ok(Vec::new()),
        }
    }

    pub async fn completion_meta(&mut self) -> Result<Vec<TableMeta>> {
        let rows = self
            .client
            .query(
                "select n.nspname, c.relname, c.relkind in ('v', 'm'),
                        a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod)
                   from pg_catalog.pg_class c
                   join pg_catalog.pg_namespace n on n.oid = c.relnamespace
                   left join pg_catalog.pg_attribute a
                          on a.attrelid = c.oid and a.attnum > 0 and not a.attisdropped
                  where c.relkind in ('r', 'p', 'f', 'v', 'm')
                    and n.nspname not in ('pg_catalog', 'information_schema')
                    and n.nspname not like 'pg\\_toast%' and n.nspname not like 'pg\\_temp%'
                  order by n.nspname, c.relname, a.attnum",
                &[],
            )
            .await?;
        Ok(super::group_meta(rows.iter().map(|r| {
            (
                r.get(0),
                r.get(1),
                r.get(2),
                r.get::<_, Option<String>>(3),
                r.get::<_, Option<String>>(4),
            )
        })))
    }

    pub async fn primary_key(&mut self, schema: &str, table: &str) -> Result<Vec<String>> {
        let rows = self
            .client
            .query(
                "select a.attname
                   from pg_catalog.pg_index i
                   join pg_catalog.pg_class c on c.oid = i.indrelid
                   join pg_catalog.pg_namespace n on n.oid = c.relnamespace
                   join lateral unnest(i.indkey) with ordinality k(attnum, ordinal) on k.ordinal <= i.indnkeyatts
                   join pg_catalog.pg_attribute a on a.attrelid = c.oid and a.attnum = k.attnum
                  where i.indisprimary and n.nspname = $1 and c.relname = $2
                  order by k.ordinal",
                &[&schema, &table],
            )
            .await?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }

    pub async fn ddl_for_comparison(&mut self, node: &DbObject) -> Result<(String, Vec<String>)> {
        anyhow::ensure!(
            matches!(node.kind, ObjectKind::Table | ObjectKind::View),
            "DDL Diff supports tables and views only."
        );
        let schema = node
            .path
            .iter()
            .rev()
            .nth(1)
            .context("missing schema path")?;
        self.client.batch_execute(
            "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET LOCAL statement_timeout = '15000ms'; SET LOCAL lock_timeout = '15000ms'"
        ).await?;
        let result = async {
            let row = self.client.query_opt(
                "select c.relkind::text, c.relispartition, c.relpersistence::text,
                        exists(select 1 from pg_catalog.pg_inherits i where i.inhrelid=c.oid or i.inhparent=c.oid)
                   from pg_catalog.pg_class c
                   join pg_catalog.pg_namespace n on n.oid=c.relnamespace
                  where n.nspname=$1 and c.relname=$2", &[schema, &node.name]
            ).await?.context("object not found")?;
            let kind: String = row.get(0);
            let partition: bool = row.get(1);
            let persistence: String = row.get(2);
            let inherited: bool = row.get(3);
            match node.kind {
                ObjectKind::Table => anyhow::ensure!(kind == "r" && !partition && !inherited && persistence != "t", "DDL Diff supports ordinary and unlogged PostgreSQL tables; temporary, foreign, inherited and partitioned tables are not supported."),
                ObjectKind::View => anyhow::ensure!(matches!(kind.as_str(), "v" | "m") && persistence != "t", "The selected object is no longer a persistent PostgreSQL view."),
                _ => unreachable!(),
            }
            let mut sql = self.ddl(node).await?;
            let coverage = if node.kind == ObjectKind::Table {
                // The existing generator reconstructs only part of a table.
                // Preserve this visible persistence distinction explicitly.
                if persistence == "u" {
                    sql = sql.replacen("CREATE TABLE ", "CREATE UNLOGGED TABLE ", 1);
                }
                vec![
                    "Reconstructed PostgreSQL table DDL: columns, defaults, NOT NULL, constraints and non-constraint indexes; this is not a complete schema comparison.".into(),
                    "Identity/generated semantics, column collation, ownership, grants, triggers, policies/RLS, storage settings, tablespaces and comments are not fully represented.".into(),
                ]
            } else {
                vec![format!("PostgreSQL {} definition from pg_get_viewdef. Ownership, grants, view options and materialized-view indexes are not included.", if kind == "m" { "materialized view" } else { "view" })]
            };
            Ok((sql, coverage))
        }.await;
        match result {
            Ok(definition) => {
                self.client.batch_execute("COMMIT").await?;
                Ok(definition)
            }
            Err(error) => {
                let _ = self.client.batch_execute("ROLLBACK").await;
                Err(error)
            }
        }
    }

    pub async fn ddl(&mut self, node: &DbObject) -> Result<String> {
        let name = node.name.as_str();
        let schema = node
            .path
            .len()
            .checked_sub(2)
            .map(|i| node.path[i].as_str())
            .unwrap_or("public");
        match node.kind {
            ObjectKind::Schema => Ok(format!("CREATE SCHEMA {};", ident(name))),
            ObjectKind::Table => self.table_ddl(schema, name).await,
            ObjectKind::View => {
                let row = self
                    .client
                    .query_one(
                        "select c.relkind::text, pg_catalog.pg_get_viewdef(c.oid, true)
                           from pg_catalog.pg_class c
                           join pg_catalog.pg_namespace n on n.oid = c.relnamespace
                          where n.nspname = $1 and c.relname = $2",
                        &[&schema, &name],
                    )
                    .await?;
                let kind = if row.get::<_, String>(0) == "m" {
                    "MATERIALIZED VIEW"
                } else {
                    "OR REPLACE VIEW"
                };
                let body: String = row.get(1);
                Ok(format!(
                    "CREATE {kind} {}.{} AS\n{}",
                    ident(schema),
                    ident(name),
                    body.trim_end()
                ))
            }
            ObjectKind::Routine => {
                let row = self
                    .client
                    .query_one(
                        "select pg_catalog.pg_get_functiondef(p.oid)
                           from pg_catalog.pg_proc p
                           join pg_catalog.pg_namespace n on n.oid = p.pronamespace
                          where n.nspname = $1
                            and p.proname || '(' || pg_catalog.pg_get_function_identity_arguments(p.oid) || ')' = $2",
                        &[&schema, &name],
                    )
                    .await?;
                Ok(format!("{};", row.get::<_, String>(0).trim_end()))
            }
            ObjectKind::Sequence => {
                let row = self
                    .client
                    .query_one(
                        "select start_value, increment_by, min_value, max_value, cache_size, cycle
                           from pg_catalog.pg_sequences where schemaname = $1 and sequencename = $2",
                        &[&schema, &name],
                    )
                    .await?;
                let cycle = if row.get::<_, bool>(5) { " CYCLE" } else { "" };
                Ok(format!(
                    "CREATE SEQUENCE {}.{}\n    START {} INCREMENT {} MINVALUE {} MAXVALUE {} CACHE {}{cycle};",
                    ident(schema),
                    ident(name),
                    row.get::<_, i64>(0),
                    row.get::<_, i64>(1),
                    row.get::<_, i64>(2),
                    row.get::<_, i64>(3),
                    row.get::<_, i64>(4),
                ))
            }
            _ => anyhow::bail!("no DDL for {:?}", node.kind),
        }
    }

    /// Rebuilds CREATE TABLE from the catalog, the same sources pg_dump uses.
    async fn table_ddl(&self, schema: &str, table: &str) -> Result<String> {
        let oid: u32 = self
            .client
            .query_one(
                "select c.oid from pg_catalog.pg_class c
                   join pg_catalog.pg_namespace n on n.oid = c.relnamespace
                  where n.nspname = $1 and c.relname = $2",
                &[&schema, &table],
            )
            .await?
            .get(0);
        let cols = self
            .client
            .query(
                "select a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod), a.attnotnull,
                        pg_catalog.pg_get_expr(d.adbin, d.adrelid)
                   from pg_catalog.pg_attribute a
                   left join pg_catalog.pg_attrdef d on d.adrelid = a.attrelid and d.adnum = a.attnum
                  where a.attrelid = $1 and a.attnum > 0 and not a.attisdropped
                  order by a.attnum",
                &[&oid],
            )
            .await?;
        let mut lines: Vec<String> = cols
            .iter()
            .map(|r| {
                let mut line = format!(
                    "    {} {}",
                    ident(&r.get::<_, String>(0)),
                    r.get::<_, String>(1)
                );
                if let Some(def) = r.get::<_, Option<String>>(3) {
                    line.push_str(&format!(" DEFAULT {def}"));
                }
                if r.get::<_, bool>(2) {
                    line.push_str(" NOT NULL");
                }
                line
            })
            .collect();
        let cons = self
            .client
            .query(
                "select conname, pg_catalog.pg_get_constraintdef(oid, true)
                   from pg_catalog.pg_constraint
                  where conrelid = $1 and contype in ('p', 'u', 'f', 'c', 'x')
                  order by contype = 'p' desc, contype = 'f', conname",
                &[&oid],
            )
            .await?;
        lines.extend(cons.iter().map(|r| {
            format!(
                "    CONSTRAINT {} {}",
                ident(&r.get::<_, String>(0)),
                r.get::<_, String>(1)
            )
        }));
        let mut out = format!(
            "CREATE TABLE {}.{} (\n{}\n);",
            ident(schema),
            ident(table),
            lines.join(",\n")
        );

        // Indexes that do not back a constraint (those are already in the table body).
        let idx = self
            .client
            .query(
                "select pg_catalog.pg_get_indexdef(i.indexrelid)
                   from pg_catalog.pg_index i
                  where i.indrelid = $1
                    and not exists (select 1 from pg_catalog.pg_constraint c where c.conindid = i.indexrelid)
                  order by i.indexrelid",
                &[&oid],
            )
            .await?;
        for r in &idx {
            out.push_str(&format!("\n\n{};", r.get::<_, String>(0)));
        }
        Ok(out)
    }

    async fn schemas(&self, path: &[String]) -> Result<Vec<DbObject>> {
        let rows = self
            .client
            .query(
                "select n.nspname
                   from pg_catalog.pg_namespace n
                  where n.nspname not like 'pg\\_toast%' and n.nspname not like 'pg\\_temp%'
                  order by n.nspname in ('pg_catalog', 'information_schema'), n.nspname",
                &[],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| DbObject::new(ObjectKind::Schema, r.get::<_, String>(0), path))
            .collect())
    }

    async fn schema_objects(&self, path: &[String]) -> Result<Vec<DbObject>> {
        let schema = path.last().context("schema path")?;
        let rels = self
            .client
            .query(
                "select c.relname, c.relkind::text, c.reltuples::float8
                   from pg_catalog.pg_class c
                   join pg_catalog.pg_namespace n on n.oid = c.relnamespace
                  where n.nspname = $1 and c.relkind in ('r', 'p', 'f', 'v', 'm', 'S')
                  order by c.relname",
                &[schema],
            )
            .await?;
        let (mut tables, mut views, mut seqs) = (Vec::new(), Vec::new(), Vec::new());
        for r in &rels {
            let name: String = r.get(0);
            let relkind: String = r.get(1);
            match relkind.as_str() {
                "S" => seqs.push(DbObject::new(ObjectKind::Sequence, name, path)),
                "v" | "m" => {
                    let detail = if relkind == "m" { "materialized" } else { "" };
                    views.push(DbObject::new(table_kind(true), name, path).with_detail(detail));
                }
                _ => {
                    let est: f64 = r.get(2);
                    let detail = if est >= 0.0 {
                        format!("~{} rows", est as i64)
                    } else {
                        String::new()
                    };
                    tables.push(DbObject::new(ObjectKind::Table, name, path).with_detail(detail));
                }
            }
        }

        let routines = self
            .client
            .query(
                "select p.proname || '(' || pg_catalog.pg_get_function_identity_arguments(p.oid) || ')',
                        coalesce(pg_catalog.pg_get_function_result(p.oid), 'procedure')
                   from pg_catalog.pg_proc p
                   join pg_catalog.pg_namespace n on n.oid = p.pronamespace
                  where n.nspname = $1
                  order by p.proname",
                &[schema],
            )
            .await?;
        let routines = routines
            .iter()
            .map(|r| {
                DbObject::new(ObjectKind::Routine, r.get::<_, String>(0), path)
                    .with_detail(r.get::<_, String>(1))
            })
            .collect();

        Ok(schema_folders(
            path,
            vec![
                ("tables", tables),
                ("views", views),
                ("routines", routines),
                ("sequences", seqs),
            ],
        ))
    }

    async fn table_details(&self, path: &[String]) -> Result<Vec<DbObject>> {
        let [.., schema, table] = path else {
            anyhow::bail!("table path")
        };
        let rel = self
            .client
            .query_one(
                "select c.oid from pg_catalog.pg_class c
                   join pg_catalog.pg_namespace n on n.oid = c.relnamespace
                  where n.nspname = $1 and c.relname = $2",
                &[schema, table],
            )
            .await?;
        let oid: u32 = rel.get(0);

        let columns = self
            .client
            .query(
                "select a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod), a.attnotnull,
                        pg_catalog.pg_get_expr(d.adbin, d.adrelid)
                   from pg_catalog.pg_attribute a
                   left join pg_catalog.pg_attrdef d on d.adrelid = a.attrelid and d.adnum = a.attnum
                  where a.attrelid = $1 and a.attnum > 0 and not a.attisdropped
                  order by a.attnum",
                &[&oid],
            )
            .await?;
        let columns = columns
            .iter()
            .map(|r| {
                let mut detail: String = r.get(1);
                if r.get::<_, bool>(2) {
                    detail.push_str(" not null");
                }
                if let Some(def) = r.get::<_, Option<String>>(3) {
                    detail.push_str(&format!(" = {def}"));
                }
                DbObject::new(ObjectKind::Column, r.get::<_, String>(0), path).with_detail(detail)
            })
            .collect();

        let keys = self
            .client
            .query(
                "select conname, pg_catalog.pg_get_constraintdef(oid)
                   from pg_catalog.pg_constraint
                  where conrelid = $1 and contype in ('p', 'u', 'f', 'c', 'x')
                  order by contype = 'p' desc, conname",
                &[&oid],
            )
            .await?;
        let keys = keys
            .iter()
            .map(|r| {
                DbObject::new(ObjectKind::Key, r.get::<_, String>(0), path)
                    .with_detail(r.get::<_, String>(1))
            })
            .collect();

        let indexes = self
            .client
            .query(
                "select i.relname, pg_catalog.pg_get_indexdef(i.oid)
                   from pg_catalog.pg_index x
                   join pg_catalog.pg_class i on i.oid = x.indexrelid
                  where x.indrelid = $1
                  order by i.relname",
                &[&oid],
            )
            .await?;
        let indexes = indexes
            .iter()
            .map(|r| {
                let def: String = r.get(1);
                let cols = def
                    .split_once(" USING ")
                    .map_or(def.as_str(), |(_, rest)| rest)
                    .to_string();
                DbObject::new(ObjectKind::Index, r.get::<_, String>(0), path).with_detail(cols)
            })
            .collect();

        Ok(table_folders(path, columns, keys, indexes))
    }
}

async fn collect_rows(
    stream: tokio_postgres::RowStream,
    page: Page,
    exact_columns: &mut [bool],
) -> Result<(Vec<Vec<Value>>, bool)> {
    use futures_util::StreamExt;
    let mut stream = std::pin::pin!(stream);
    let mut rows = Vec::new();
    let mut seen = 0usize;
    while let Some(row) = stream.next().await {
        seen += 1;
        if seen <= page.skip {
            row?;
            continue;
        }
        if rows.len() == page.limit {
            // Dropping the stream lets the connection discard the remaining rows.
            return Ok((rows, true));
        }
        rows.push(row_values(&row?, exact_columns));
    }
    Ok((rows, false))
}

fn row_values(row: &Row, exact_columns: &mut [bool]) -> Vec<Value> {
    (0..row.len())
        .map(|i| match row.try_get::<_, PgValue>(i) {
            Ok(v) => {
                exact_columns[i] &= v.1;
                v.0
            }
            Err(e) => {
                exact_columns[i] = false;
                Value::Text(format!("<{e}>"))
            }
        })
        .collect()
}

/// Decodes any PostgreSQL value from the binary protocol.
struct PgValue(Value, bool);

impl<'a> FromSql<'a> for PgValue {
    fn from_sql(ty: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn Error + Sync + Send>> {
        let mut exact = true;
        let v = match *ty {
            Type::BOOL => Value::Bool(bool::from_sql(ty, raw)?),
            Type::INT2 => Value::Int(i16::from_sql(ty, raw)? as i64),
            Type::INT4 => Value::Int(i32::from_sql(ty, raw)? as i64),
            Type::INT8 => Value::Int(i64::from_sql(ty, raw)?),
            Type::OID => Value::Int(u32::from_sql(ty, raw)? as i64),
            Type::FLOAT4 => Value::Float(f32::from_sql(ty, raw)? as f64),
            Type::FLOAT8 => Value::Float(f64::from_sql(ty, raw)?),
            Type::NUMERIC => match rust_decimal::Decimal::from_sql(ty, raw) {
                Ok(d) => {
                    // Decimal's decoder may round while returning Ok. Seven
                    // base-10000 groups and scale <= 28 form a safe exact subset.
                    exact = raw.len() >= 8
                        && u16::from_be_bytes([raw[0], raw[1]]) <= 7
                        && u16::from_be_bytes([raw[6], raw[7]]) <= 28;
                    Value::Decimal(d.to_string())
                }
                // NaN, infinity or more digits than Decimal holds.
                Err(_) => {
                    exact = false;
                    Value::Text("<numeric>".into())
                }
            },
            Type::TIMESTAMP => Value::Text(chrono::NaiveDateTime::from_sql(ty, raw)?.to_string()),
            Type::TIMESTAMPTZ => {
                Value::Text(chrono::DateTime::<chrono::Utc>::from_sql(ty, raw)?.to_rfc3339())
            }
            Type::DATE => Value::Text(chrono::NaiveDate::from_sql(ty, raw)?.to_string()),
            Type::TIME => Value::Text(chrono::NaiveTime::from_sql(ty, raw)?.to_string()),
            Type::JSON | Type::JSONB => {
                let text = if *ty == Type::JSONB {
                    match raw.split_first() {
                        Some((1, text)) => text,
                        _ => return Err("unsupported JSONB encoding version".into()),
                    }
                } else {
                    raw
                };
                // JSON numbers can exceed both f64 and u64 precision. Retain
                // the server's text for exact keys and optimistic predicates.
                Value::Text(std::str::from_utf8(text)?.to_owned())
            }
            Type::UUID => Value::Text(uuid::Uuid::from_sql(ty, raw)?.to_string()),
            Type::BYTEA => Value::Bytes(raw.to_vec()),
            Type::INET | Type::CIDR => Value::Text(
                std::net::IpAddr::from_sql(ty, raw)
                    .map(|ip| ip.to_string())
                    .unwrap_or_else(|_| hex(raw)),
            ),
            Type::INTERVAL => Value::Text(interval(raw)),
            Type::BOOL_ARRAY => array(Vec::<Option<bool>>::from_sql(ty, raw)?),
            Type::INT2_ARRAY => array(Vec::<Option<i16>>::from_sql(ty, raw)?),
            Type::INT4_ARRAY => array(Vec::<Option<i32>>::from_sql(ty, raw)?),
            Type::INT8_ARRAY => array(Vec::<Option<i64>>::from_sql(ty, raw)?),
            Type::FLOAT8_ARRAY => array(Vec::<Option<f64>>::from_sql(ty, raw)?),
            Type::TEXT_ARRAY | Type::VARCHAR_ARRAY | Type::NAME_ARRAY => {
                array(Vec::<Option<String>>::from_sql(ty, raw)?)
            }
            // PostGIS sends EWKB; keep the bytes for the Geo Viewer.
            _ if matches!(ty.name(), "geometry" | "geography") => Value::Bytes(raw.to_vec()),
            _ => match std::str::from_utf8(raw) {
                // text, varchar, char, name, xml, enums and citext are sent as UTF-8.
                Ok(s) => Value::Text(s.to_string()),
                Err(_) => Value::Text(format!("<{}: {}>", ty.name(), hex(raw))),
            },
        };
        exact &= editable_pg_type(
            ty.name(),
            matches!(ty.kind(), tokio_postgres::types::Kind::Enum(_)),
        );
        Ok(PgValue(v, exact))
    }

    fn from_sql_null(_: &Type) -> Result<Self, Box<dyn Error + Sync + Send>> {
        Ok(PgValue(Value::Null, true))
    }

    fn accepts(_: &Type) -> bool {
        true
    }
}

fn array<T: std::fmt::Display>(items: Vec<Option<T>>) -> Value {
    let parts: Vec<String> = items
        .into_iter()
        .map(|v| v.map_or("NULL".into(), |v| v.to_string()))
        .collect();
    Value::Text(format!("{{{}}}", parts.join(",")))
}

fn hex(raw: &[u8]) -> String {
    raw.iter().map(|b| format!("{b:02x}")).collect()
}

/// Binary interval: microseconds (i64), days (i32), months (i32).
fn interval(raw: &[u8]) -> String {
    if raw.len() != 16 {
        return hex(raw);
    }
    let micros = i64::from_be_bytes(raw[0..8].try_into().unwrap());
    let days = i32::from_be_bytes(raw[8..12].try_into().unwrap());
    let months = i32::from_be_bytes(raw[12..16].try_into().unwrap());
    let mut parts = Vec::new();
    if months != 0 {
        parts.push(format!("{} years {} mons", months / 12, months % 12));
    }
    if days != 0 {
        parts.push(format!("{days} days"));
    }
    let secs = micros / 1_000_000;
    parts.push(format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    ));
    parts.join(" ")
}

/// tokio-postgres prints only "db error"; the Output log needs SQLSTATE,
/// message, detail, hint and position.
pub fn describe(e: anyhow::Error) -> anyhow::Error {
    let Some(db) = e
        .downcast_ref::<tokio_postgres::Error>()
        .and_then(|e| e.as_db_error())
    else {
        return e;
    };
    let mut out = format!("[{}] {}: {}", db.code().code(), db.severity(), db.message());
    if let Some(detail) = db.detail() {
        out.push_str(&format!("\nDetail: {detail}"));
    }
    if let Some(hint) = db.hint() {
        out.push_str(&format!("\nHint: {hint}"));
    }
    if let Some(tokio_postgres::error::ErrorPosition::Original(p)) = db.position() {
        out.push_str(&format!("\nPosition: {p}"));
    }
    anyhow::anyhow!(out)
}

/// Quotes an identifier only when PostgreSQL would otherwise fold or reject it.
fn ident(name: &str) -> String {
    let plain = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if plain {
        name.to_string()
    } else {
        super::quote_ident(name, '"')
    }
}

// Arrays, domain binary encodings, intervals and spatial values need their own
// lossless round trip before they can participate in optimistic updates.
fn editable_pg_type(name: &str, is_enum: bool) -> bool {
    is_enum
        || matches!(
            name,
            "bool"
                | "int2"
                | "int4"
                | "int8"
                | "oid"
                | "float4"
                | "float8"
                | "numeric"
                | "text"
                | "varchar"
                | "bpchar"
                | "name"
                | "citext"
                | "uuid"
                | "date"
                | "time"
                | "timestamp"
                | "timestamptz"
                | "bytea"
                | "jsonb"
        )
}

#[derive(Debug)]
struct PgInput<'a>(&'a Value);

/// Shared with the encoder: reject client-side failures before Execute without
/// allocating a second serialized copy of potentially large key values.
pub(super) fn validate_explain_params(params: &[Value]) -> Result<()> {
    for param in params {
        validate_input(param)?;
    }
    Ok(())
}

fn validate_input(value: &Value) -> Result<()> {
    anyhow::ensure!(
        !matches!(value, Value::Text(text) | Value::Decimal(text) if text.contains('\0')),
        "PostgreSQL text parameters cannot contain NUL"
    );
    Ok(())
}

impl tokio_postgres::types::ToSql for PgInput<'_> {
    fn to_sql(
        &self,
        _: &Type,
        out: &mut bytes::BytesMut,
    ) -> Result<tokio_postgres::types::IsNull, Box<dyn Error + Send + Sync>> {
        use tokio_postgres::types::IsNull;
        match self.0 {
            Value::Null => return Ok(IsNull::Yes),
            Value::Text(text) | Value::Decimal(text) => {
                validate_input(self.0).map_err(|error| -> Box<dyn Error + Send + Sync> { error.into() })?;
                out.extend_from_slice(text.as_bytes());
            }
            Value::Bytes(bytes) => {
                out.extend_from_slice(b"\\x");
                const HEX: &[u8; 16] = b"0123456789abcdef";
                for byte in bytes {
                    out.extend_from_slice(&[HEX[(byte >> 4) as usize], HEX[(byte & 15) as usize]]);
                }
            }
            Value::Float(value) if value.is_infinite() => {
                out.extend_from_slice(if value.is_sign_positive() {
                    b"Infinity"
                } else {
                    b"-Infinity"
                });
            }
            value => out.extend_from_slice(value.to_string().as_bytes()),
        }
        Ok(IsNull::No)
    }
    fn accepts(_: &Type) -> bool {
        true
    }
    fn encode_format(&self, _: &Type) -> tokio_postgres::types::Format {
        tokio_postgres::types::Format::Text
    }
    tokio_postgres::types::to_sql_checked!();
}

#[cfg(test)]
mod mutation_tests {
    use super::*;
    use tokio_postgres::types::{IsNull, ToSql};

    #[test]
    #[ignore = "manual release CPU measurement; no database connection"]
    fn benchmark_explain_parameter_preflight() {
        for bytes in [32, 1_000_000] {
            let params = vec![Value::Text("x".repeat(bytes)), Value::Int(7)];
            let start = std::time::Instant::now();
            for _ in 0..10_000 {
                validate_explain_params(std::hint::black_box(&params)).unwrap();
            }
            eprintln!("PostgreSQL Explain preflight, {bytes} text bytes + int: {:.3} us/request (10000 repetitions)", start.elapsed().as_secs_f64() * 100.0);
        }
    }

    #[test]
    fn text_binding_preserves_literal_content_without_sql_escaping() {
        let value = Value::Text("O'Reilly \\ snow 雪; DROP TABLE ignored".into());
        let mut out = bytes::BytesMut::new();
        assert!(matches!(
            PgInput(&value).to_sql(&Type::TEXT, &mut out).unwrap(),
            IsNull::No
        ));
        assert_eq!(&out[..], value.to_string().as_bytes());
        assert!(matches!(
            PgInput(&value).encode_format(&Type::UUID),
            tokio_postgres::types::Format::Text
        ));
    }

    #[test]
    fn bytea_null_and_nul_have_unambiguous_parameter_encodings() {
        let mut out = bytes::BytesMut::new();
        PgInput(&Value::Bytes(vec![0, 39, 92, 255]))
            .to_sql(&Type::BYTEA, &mut out)
            .unwrap();
        assert_eq!(&out[..], b"\\x00275cff");
        out.clear();
        assert!(matches!(
            PgInput(&Value::Null).to_sql(&Type::TEXT, &mut out).unwrap(),
            IsNull::Yes
        ));
        assert!(out.is_empty());
        assert!(
            PgInput(&Value::Text("invalid\0text".into()))
                .to_sql(&Type::TEXT, &mut out)
                .is_err()
        );
    }

    fn numeric(scale: u16, digits: &[u16]) -> Vec<u8> {
        let mut raw = Vec::new();
        for header in [digits.len() as u16, 0, 0, scale] {
            raw.extend_from_slice(&header.to_be_bytes());
        }
        for digit in digits {
            raw.extend_from_slice(&digit.to_be_bytes());
        }
        raw
    }

    #[test]
    fn rounded_numeric_is_never_an_exact_original_value() {
        let raw = numeric(29, &[1, 2345, 6789, 1234, 5678, 9123, 4567, 8912, 3000]);
        assert!(rust_decimal::Decimal::from_sql(&Type::NUMERIC, &raw).is_ok());
        assert!(!PgValue::from_sql(&Type::NUMERIC, &raw).unwrap().1);
        let raw = numeric(4, &[123, 4567]);
        let decoded = PgValue::from_sql(&Type::NUMERIC, &raw).unwrap();
        assert!(decoded.1);
        assert_eq!(decoded.0, Value::Decimal("123.4567".into()));
    }

    #[test]
    fn jsonb_preserves_numbers_beyond_machine_precision() {
        let text = r#"{"integer":123456789012345678901234567890,"fraction":0.123456789012345678901234567890}"#;
        let mut raw = vec![1];
        raw.extend_from_slice(text.as_bytes());
        let decoded = PgValue::from_sql(&Type::JSONB, &raw).unwrap();
        assert_eq!(decoded.0, Value::Text(text.into()));
        assert!(decoded.1);
        let json = PgValue::from_sql(&Type::JSON, text.as_bytes()).unwrap();
        assert_eq!(json.0, Value::Text(text.into()));
        assert!(!json.1); // PostgreSQL JSON has no equality operator.
        assert!(PgValue::from_sql(&Type::JSONB, &[2, b'{', b'}']).is_err());
    }
}

#[cfg(test)]
mod explain_connection_tests {
    use super::ConnectionTask;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn dropping_session_aborts_an_inflight_connection_task() {
        struct OnDrop(Option<oneshot::Sender<()>>);
        impl Drop for OnDrop {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(());
                }
            }
        }
        let (started, ready) = oneshot::channel();
        let (dropped, closed) = oneshot::channel();
        let task = ConnectionTask(tokio::spawn(async move {
            let _guard = OnDrop(Some(dropped));
            started.send(()).unwrap();
            std::future::pending::<Result<(), tokio_postgres::Error>>().await
        }));
        ready.await.unwrap();
        drop(task);
        closed.await.unwrap();
    }
}
