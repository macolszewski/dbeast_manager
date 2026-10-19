use super::{ServerInfo, schema_folders, table_folders, table_kind};
use crate::model::{
    BoundQuery, BoundUpdate, Column, ColumnOrigin, DataSource, DbObject, ForeignKey,
    ForeignKeyColumn, ObjectKind, OriginColumn, OriginTable, Page, PhysicalColumn, PhysicalTable,
    QueryResult, RelatedDirection, RelatedMetadata, ResolvedContext, ResultSource, SchemaSelection,
    TableEditMetadata, TableMeta, Value,
};
use anyhow::{Context, Result};
use mysql_async::consts::{ColumnType, StatusFlags};
use mysql_async::prelude::*;
use mysql_async::{Conn, Opts, OptsBuilder, Row};
pub(super) mod graph;
pub(super) mod mcp;

/// schema, table, table type, column, column type
type MetaRow = (String, String, String, Option<String>, Option<String>);

pub struct MyConn {
    conn: mcp::ManagedConn,
    opts: Opts,
    initial_database: Option<String>,
    tls_mode: crate::model::TlsMode,
}

pub(super) struct PreparedExplain {
    pub(super) statement: mysql_async::Statement,
    pub(super) context: ResolvedContext,
}

/// MySQL has no protocol-level cancel: the running query is killed from a
/// second connection.
pub struct MyCancel {
    opts: Opts,
    connection_id: u32,
}

// Keep setup cancellable; the driver caches its native connector in these options.
async fn open_socket(opts: Opts) -> Result<Conn> {
    Conn::new(opts).await.map_err(|error| match error {
        mysql_async::Error::Io(mysql_async::IoError::Tls(_)) => anyhow::anyhow!(
            "TLS setup failed. Check the CA certificate, server hostname, client PKCS#12 identity and its password."),
        error => error.into(),
    })
}

impl MyCancel {
    /// Callers bound this with a timeout; dropping it then closes the cancel
    /// socket instead of leaving a detached task to finish the exchange.
    pub async fn cancel(self) -> Result<()> {
        let mut conn = mcp::ManagedConn::ordinary(open_socket(self.opts).await?);
        conn.query_drop(format!("KILL QUERY {}", self.connection_id))
            .await?;
        conn.close().await;
        Ok(())
    }
}

impl MyConn {
    pub async fn open(ds: &DataSource, endpoint: &super::Endpoint) -> Result<Self> {
        let mut builder = OptsBuilder::default()
            .ip_or_hostname(endpoint.host.clone())
            .tcp_port(endpoint.port)
            .user(Some(ds.user.clone()))
            .pass(Some(ds.password.clone()))
            .client_found_rows(true)
            .prefer_socket(false);
        if !ds.database.is_empty() {
            builder = builder.db_name(Some(ds.database.clone()));
        }
        if let Some(material) = super::tls::prepare(ds).await? {
            // Preserve the database hostname when TCP targets the local SSH forward.
            builder = builder.ssl_opts(Some(material.mysql.with_danger_tls_hostname_override(Some(ds.host.clone()))));
        }
        let opts: Opts = builder.into();
        let mut conn = mcp::ManagedConn::ordinary(open_socket(opts.clone()).await?);
        if ds.read_only {
            conn.query_drop("SET SESSION TRANSACTION READ ONLY").await?;
        }
        let initial_database = conn
            .query_first::<(Option<String>,), _>("select database()")
            .await?
            .and_then(|row| row.0);
        Ok(Self {
            conn,
            opts,
            initial_database,
            tls_mode: ds.tls.mode,
        })
    }

    pub fn cancel_handle(&self) -> Option<MyCancel> {
        Some(MyCancel {
            opts: self.opts.clone(),
            connection_id: self.conn.id(),
        })
    }

    pub async fn server_info(&mut self) -> Result<ServerInfo> {
        let version: Option<String> = self.conn.query_first("select version()").await?;
        let cipher: Option<(String, String)> = self.conn.query_first("SHOW SESSION STATUS LIKE 'Ssl_cipher'").await?;
        let encrypted = cipher.is_some_and(|(_, cipher)| !cipher.is_empty());
        anyhow::ensure!(self.opts.ssl_opts().is_none() || encrypted, "Server did not confirm TLS encryption");
        Ok(ServerInfo {
            product: "MySQL".into(),
            version: version.unwrap_or_default(),
            transport: if encrypted { self.tls_mode.transport() } else { "Unencrypted" }.into(),
        })
    }

    pub async fn execute(&mut self, sql: &str, page: Page) -> Result<QueryResult> {
        let result = self.conn.query_iter(sql).await?;
        Self::read_result(result, page).await
    }

    /// Runs a console statement. Outside a transaction, a plain SELECT that
    /// qualifies (see [`page_hint_offset`]) gets an optimizer hint that ends
    /// its result one row after the page, so the server stops sending rows
    /// instead of the client reading the rest of a large result off the wire.
    async fn execute_paged(&mut self, sql: &str, page: Page) -> Result<QueryResult> {
        let rows = page.skip.saturating_add(page.limit).saturating_add(1);
        let autocommit = self.conn.last_ok_packet().is_some_and(|packet| {
            let flags = packet.status_flags();
            flags.contains(StatusFlags::SERVER_STATUS_AUTOCOMMIT)
                && !flags.contains(StatusFlags::SERVER_STATUS_IN_TRANS)
        });
        match page_hint_offset(sql).filter(|_| autocommit && page.limit != usize::MAX) {
            Some(at) => {
                let hinted = format!(
                    "{} /*+ SET_VAR(sql_select_limit = {rows}) */{}",
                    &sql[..at],
                    &sql[at..]
                );
                self.execute(&hinted, page).await
            }
            None => self.execute(sql, page).await,
        }
    }

    async fn read_result<P: mysql_async::prelude::Protocol>(
        mut result: mysql_async::QueryResult<'_, '_, P>,
        page: Page,
    ) -> Result<QueryResult> {
        let origins = column_origins(result.columns_ref());
        // Text protocol FLOAT rendering does not guarantee its full binary precision.
        let exact_columns = result
            .columns_ref()
            .iter()
            .map(|column| column.column_type() != ColumnType::MYSQL_TYPE_FLOAT)
            .collect();
        let columns: Vec<Column> = result
            .columns_ref()
            .iter()
            .map(|c| Column {
                name: c.name_str().into_owned(),
                type_name: type_name(c.column_type()),
            })
            .collect();
        if columns.is_empty() {
            let affected = result.affected_rows();
            result.drop_result().await?;
            return Ok(QueryResult {
                affected: Some(affected),
                ..Default::default()
            });
        }

        // Charset 63 marks binary data; TEXT columns share the BLOB type codes.
        let types: Vec<(ColumnType, bool, u8)> = result
            .columns_ref()
            .iter()
            .map(|c| (c.column_type(), c.character_set() == 63, c.decimals()))
            .collect();
        let mut rows = Vec::new();
        let mut truncated = false;
        let mut seen = 0usize;
        let mut bytes = 0usize;
        while let Some(row) = result.next().await? {
            seen += 1;
            if seen <= page.skip {
                continue;
            }
            if page.is_full(rows.len(), bytes) {
                truncated = true;
                break;
            }
            let row = row_values(row, &types);
            bytes += crate::model::row_bytes(&row);
            rows.push(row);
        }
        // The text protocol has no partial fetch: the rest must be read off the wire.
        result.drop_result().await?;
        Ok(QueryResult {
            columns,
            rows,
            truncated,
            source: Some(ResultSource {
                session_epoch: 0,
                columns: origins,
                exact_columns,
            }),
            ..Default::default()
        })
    }

    pub(super) async fn prepare_explain(&mut self, sql: &str) -> Result<PreparedExplain> {
        let context = self.resolved_context().await?;
        let statement = self.conn.prep(sql).await?;
        Ok(PreparedExplain { statement, context })
    }

    pub(super) async fn execute_explain(
        &mut self,
        prepared: PreparedExplain,
        params: &[Value],
        expected: Option<&ResolvedContext>,
    ) -> Result<QueryResult> {
        super::check_expected_context(Some(&prepared.context), expected)?;
        anyhow::ensure!(usize::from(prepared.statement.num_params()) == params.len(), "EXPLAIN parameter count mismatch.");
        let result = self.conn.exec_iter(prepared.statement, mysql_async::Params::Positional(bind_values(params))).await?;
        let mut data = Self::read_result(result, Page::ALL).await?;
        data.context = Some(prepared.context);
        Ok(data)
    }

    pub async fn insert_text_rows(&mut self, target: &super::InsertTarget, rows: &[Vec<Option<String>>]) -> Result<()> {
        let width = target.columns.len().max(1);
        // The protocol allows 65535 placeholders per statement.
        let per_statement = (60_000 / width).max(1);
        let prefix = format!("INSERT INTO {} ({}) VALUES ", target.qualified('`'), target.column_list('`'));
        let tuple = format!("({})", vec!["?"; width].join(", "));
        for chunk in rows.chunks(per_statement) {
            let sql = format!("{prefix}{}", vec![tuple.as_str(); chunk.len()].join(", "));
            let params: Vec<mysql_async::Value> = chunk
                .iter()
                .flat_map(|row| {
                    row.iter().map(|v| match v {
                        Some(text) => mysql_async::Value::Bytes(text.clone().into_bytes()),
                        None => mysql_async::Value::NULL,
                    })
                })
                .collect();
            self.conn.exec_drop(sql, mysql_async::Params::Positional(params)).await?;
        }
        Ok(())
    }

    pub async fn stream_rows(
        &mut self,
        query: &BoundQuery,
        tx: tokio::sync::mpsc::Sender<super::RowBatch>,
    ) -> Result<u64> {
        if query.params.is_empty() {
            let result = self.conn.query_iter(query.sql.as_str()).await?;
            Self::stream_result(result, tx).await
        } else {
            let result = self
                .conn
                .exec_iter(
                    query.sql.as_str(),
                    mysql_async::Params::Positional(bind_values(&query.params)),
                )
                .await?;
            Self::stream_result(result, tx).await
        }
    }

    async fn stream_result<P: mysql_async::prelude::Protocol>(
        mut result: mysql_async::QueryResult<'_, '_, P>,
        tx: tokio::sync::mpsc::Sender<super::RowBatch>,
    ) -> Result<u64> {
        let columns: Vec<Column> = result
            .columns_ref()
            .iter()
            .map(|c| Column {
                name: c.name_str().into_owned(),
                type_name: type_name(c.column_type()),
            })
            .collect();
        anyhow::ensure!(!columns.is_empty(), "The statement returns no rows.");
        let types: Vec<(ColumnType, bool, u8)> = result
            .columns_ref()
            .iter()
            .map(|c| (c.column_type(), c.character_set() == 63, c.decimals()))
            .collect();
        tx.send(super::RowBatch::Origins(column_origins(result.columns_ref()))).await.map_err(|_| super::stopped())?;
        tx.send(super::RowBatch::Columns(columns)).await.map_err(|_| super::stopped())?;
        let mut batcher = super::Batcher::new();
        let mut total = 0u64;
        while let Some(row) = result.next().await? {
            if let Some(full) = batcher.push(row_values(row, &types)) {
                total += full.len() as u64;
                if tx.send(super::RowBatch::Rows(full)).await.is_err() {
                    // The text protocol has no partial fetch; drain before reporting.
                    result.drop_result().await?;
                    return Err(super::stopped());
                }
            }
        }
        if let Some(rest) = batcher.rest() {
            total += rest.len() as u64;
            tx.send(super::RowBatch::Rows(rest)).await.map_err(|_| super::stopped())?;
        }
        Ok(total)
    }

    pub async fn execute_bound_query(
        &mut self,
        query: &BoundQuery,
        page: Page,
        expected: Option<&ResolvedContext>,
    ) -> Result<QueryResult> {
        let context = self.resolved_context().await?;
        super::check_expected_context(Some(&context), expected)?;
        let result = self
            .conn
            .exec_iter(
                &query.sql,
                mysql_async::Params::Positional(bind_values(&query.params)),
            )
            .await?;
        let mut result = Self::read_result(result, page).await?;
        anyhow::ensure!(result.has_result_set(), "A related query must return rows.");
        result.context = Some(context);
        Ok(result)
    }

    async fn persistent_related_table(&mut self, schema: &str, table: &str) -> Result<()> {
        let qualified = format!(
            "{}.{}",
            super::quote_ident(schema, '`'),
            super::quote_ident(table, '`')
        );
        let row: Row = self
            .conn
            .query_first(format!("SHOW CREATE TABLE {qualified}"))
            .await?
            .ok_or_else(|| anyhow::anyhow!("The related table no longer exists."))?;
        let ddl: String = row
            .get_opt::<String, _>(1)
            .transpose()?
            .ok_or_else(|| anyhow::anyhow!("The related table cannot be inspected."))?;
        let mut words = ddl.split_whitespace();
        anyhow::ensure!(
            words
                .next()
                .is_some_and(|word| word.eq_ignore_ascii_case("CREATE"))
                && words
                    .next()
                    .is_some_and(|word| word.eq_ignore_ascii_case("TABLE")),
            "Related Rows opens a separate session and supports persistent ordinary MySQL tables, not temporary tables or views."
        );
        Ok(())
    }

    pub async fn foreign_keys(
        &mut self,
        origin: &OriginTable,
        direction: RelatedDirection,
    ) -> Result<RelatedMetadata> {
        let OriginTable::Named { schema, table } = origin else {
            anyhow::bail!("The result does not contain MySQL table provenance.");
        };
        self.persistent_related_table(schema, table).await?;
        let filter = match direction {
            RelatedDirection::Referenced => "table_schema=? and table_name=?",
            RelatedDirection::Referencing => {
                "referenced_table_schema=? and referenced_table_name=?"
            }
        };
        // Joining multiple INFORMATION_SCHEMA views can make MySQL spend tens
        // of seconds optimizing even a tiny schema. Keep this lookup narrow and
        // indexable, then inspect each distinct table once. Its catalog collation
        // may be case-insensitive, so exact identity is checked after retrieval.
        let sql = format!(
            "select constraint_name,table_schema,table_name,column_name,
            referenced_table_schema,referenced_table_name,referenced_column_name,ordinal_position
            from information_schema.key_column_usage
            where referenced_table_name is not null and {filter}
            order by table_schema,table_name,constraint_name,ordinal_position"
        );
        type RelatedRow = (String, String, String, String, String, String, String, u64);
        let rows: Vec<RelatedRow> = self.conn.exec(sql, (schema, table)).await?;
        let mut keys: Vec<ForeignKey> = Vec::new();
        for (name, ss, st, sc, ts, tt, tc, ordinal) in rows {
            let selected = match direction {
                RelatedDirection::Referenced => (&ss, &st),
                RelatedDirection::Referencing => (&ts, &tt),
            };
            if selected != (schema, table) {
                continue;
            }
            if keys.last().is_none_or(|key| {
                key.name != name || key.source.schema != ss || key.source.name != st
            }) {
                keys.push(ForeignKey {
                    catalog_id: name.clone(),
                    name,
                    source: PhysicalTable {
                        schema: ss,
                        name: st,
                        oid: None,
                    },
                    target: PhysicalTable {
                        schema: ts,
                        name: tt,
                        oid: None,
                    },
                    columns: Vec::new(),
                });
            }
            let key = keys.last_mut().unwrap();
            anyhow::ensure!(
                ordinal == key.columns.len() as u64 + 1,
                "The foreign key's column order is incomplete."
            );
            key.columns.push(ForeignKeyColumn {
                source_origin: OriginColumn::Named(sc.clone()),
                source_name: sc,
                source_type: String::new(),
                target_origin: OriginColumn::Named(tc.clone()),
                target_name: tc,
                target_type: String::new(),
            });
        }
        // Catalogs never expose temporary shadows. Check each referenced name
        // against this session before using permanent-table metadata.
        let mut types = std::collections::HashMap::new();
        for key in &keys {
            for related in [&key.source, &key.target] {
                let identity = (related.schema.clone(), related.name.clone());
                if types.contains_key(&identity) {
                    continue;
                }
                if (&related.schema, &related.name) != (schema, table) {
                    self.persistent_related_table(&related.schema, &related.name)
                        .await?;
                }
                let rows: Vec<(String, String, String, String)> = self.conn.exec(
                    "select table_schema,table_name,column_name,data_type from information_schema.columns \
                     where table_schema=? and table_name=?",
                    (&related.schema, &related.name)).await?;
                let columns: std::collections::HashMap<_, _> = rows
                    .into_iter()
                    .filter(|(schema, table, _, _)| {
                        schema == &related.schema && table == &related.name
                    })
                    .map(|(_, _, column, ty)| (column, ty))
                    .collect();
                types.insert(identity, columns);
            }
        }
        for key in &mut keys {
            let source = &types[&(key.source.schema.clone(), key.source.name.clone())];
            let target = &types[&(key.target.schema.clone(), key.target.name.clone())];
            for column in &mut key.columns {
                column.source_type = source
                    .get(&column.source_name)
                    .ok_or_else(|| {
                        anyhow::anyhow!("A foreign key source column is absent from the catalog.")
                    })?
                    .clone();
                column.target_type = target
                    .get(&column.target_name)
                    .ok_or_else(|| {
                        anyhow::anyhow!("A foreign key target column is absent from the catalog.")
                    })?
                    .clone();
            }
        }
        Ok(RelatedMetadata {
            table: PhysicalTable {
                schema: schema.clone(),
                name: table.clone(),
                oid: None,
            },
            keys,
        })
    }

    pub async fn execute_control(&mut self, sql: &str) -> Result<()> {
        Ok(self.conn.query_drop(sql).await?)
    }

    pub async fn table_edit_metadata(&mut self, origin: &OriginTable) -> Result<TableEditMetadata> {
        let OriginTable::Named { schema, table } = origin else {
            anyhow::bail!("The result does not contain MySQL table provenance.");
        };
        let qualified = format!(
            "{}.{}",
            super::quote_ident(schema, '`'),
            super::quote_ident(table, '`')
        );
        // SHOW resolves in this session, including a temporary table shadowing
        // information_schema's permanent table. Never grant edits from the latter.
        let definition: Row = self
            .conn
            .query_first(format!("SHOW CREATE TABLE {qualified}"))
            .await?
            .ok_or_else(|| anyhow::anyhow!("The result's source table no longer exists."))?;
        let ddl: String = definition
            .get_opt::<String, _>(1)
            .transpose()?
            .ok_or_else(|| anyhow::anyhow!("The source table cannot be inspected."))?;
        let mut words = ddl.split_whitespace();
        anyhow::ensure!(
            words
                .next()
                .is_some_and(|word| word.eq_ignore_ascii_case("CREATE"))
                && words
                    .next()
                    .is_some_and(|word| word.eq_ignore_ascii_case("TABLE")),
            "Temporary tables and views are read-only in console results; their session metadata cannot be verified safely."
        );
        let engine: Option<(Option<String>, String)> = self.conn.exec_first(
            "select engine, table_type from information_schema.tables where BINARY table_schema=BINARY ? and BINARY table_name=BINARY ?",
            (schema, table)).await?;
        let (engine, kind) = engine
            .ok_or_else(|| anyhow::anyhow!("The source table is absent from the catalog."))?;
        anyhow::ensure!(
            kind == "BASE TABLE"
                && engine
                    .as_deref()
                    .is_some_and(|engine| engine.eq_ignore_ascii_case("InnoDB")),
            "Only transactional InnoDB tables can be edited from MySQL console results."
        );
        type EditColumn = (String, u32, String, String, String);
        let rows: Vec<EditColumn> = self.conn.exec(
            "select column_name, ordinal_position, is_nullable, extra, data_type \
             from information_schema.columns where BINARY table_schema=BINARY ? and BINARY table_name=BINARY ? order by ordinal_position",
            (schema, table)).await?;
        let columns = rows
            .into_iter()
            .map(|(name, ordinal, nullable, extra, ty)| {
                let extra = extra.to_ascii_lowercase();
                let generated =
                    extra.contains("virtual generated") || extra.contains("stored generated");
                PhysicalColumn {
                    name,
                    ordinal,
                    nullable: nullable == "YES",
                    generated,
                    auto_increment: extra.contains("auto_increment"),
                    update_default: !generated,
                    comparable: !matches!(
                        ty.as_str(),
                        "json"
                            | "geometry"
                            | "point"
                            | "linestring"
                            | "polygon"
                            | "multipoint"
                            | "multilinestring"
                            | "multipolygon"
                            | "geometrycollection"
                            | "float"
                    ),
                }
            })
            .collect();
        let primary_key = self.primary_key(Some(schema), table).await?;
        Ok(TableEditMetadata {
            table: PhysicalTable {
                schema: schema.clone(),
                name: table.clone(),
                oid: None,
            },
            columns,
            primary_key,
            transactional: true,
        })
    }

    pub async fn execute_bound_update(&mut self, update: &BoundUpdate) -> Result<u64> {
        let params = bind_values(&update.params);
        let result = self
            .conn
            .exec_iter(&update.sql, mysql_async::Params::Positional(params))
            .await?;
        let affected = result.affected_rows();
        result.drop_result().await?;
        Ok(affected)
    }

    /// `bounded` lets a plain SELECT end on the server after the page; it is
    /// off for SQL that already carries the application's own LIMIT.
    pub async fn execute_with_context(
        &mut self,
        sql: &str,
        page: Page,
        expected: Option<&ResolvedContext>,
        bounded: bool,
    ) -> Result<QueryResult> {
        // mysql_async does not negotiate CLIENT_SESSION_TRACK. Reading DATABASE
        // before user SQL is necessary: USE, procedures and SQL can change it.
        let context = self.resolved_context().await?;
        super::check_expected_context(Some(&context), expected)?;
        // The context query just refreshed the session's transaction status.
        let mut result = if bounded {
            self.execute_paged(sql, page).await?
        } else {
            self.execute(sql, page).await?
        };
        if result.has_result_set() {
            result.context = Some(context);
        }
        Ok(result)
    }

    pub async fn resolved_context(&mut self) -> Result<ResolvedContext> {
        let database = self
            .conn
            .query_first::<(Option<String>,), _>("select database()")
            .await?
            .and_then(|row| row.0);
        Ok(ResolvedContext {
            schemas: database.iter().cloned().collect(),
            database,
            search_path: None,
            temporary_schema: None,
        })
    }

    pub async fn schema_names(&mut self) -> Result<Vec<String>> {
        Ok(self
            .conn
            .query("select schema_name from information_schema.schemata order by schema_name")
            .await?)
    }

    /// Transaction status from the last OK packet, without a round trip.
    /// An error packet carries no status, so after an error use
    /// [`Self::has_open_transaction`].
    pub fn last_known_in_transaction(&self) -> bool {
        self.conn.last_ok_packet().is_some_and(|packet| {
            packet
                .status_flags()
                .contains(StatusFlags::SERVER_STATUS_IN_TRANS)
        })
    }

    pub async fn has_open_transaction(&mut self) -> Result<bool> {
        // A fresh read refreshes protocol status even after manually typed SQL.
        self.resolved_context().await?;
        Ok(self.last_known_in_transaction())
    }

    pub async fn change_schema(&mut self, selection: &SchemaSelection) -> Result<ResolvedContext> {
        let database = match selection {
            SchemaSelection::Default => self.initial_database.clone(),
            SchemaSelection::Named(name) => Some(name.clone()),
        };
        self.use_database(database.as_deref()).await?;
        self.resolved_context().await
    }

    pub async fn apply_resolved_context(&mut self, context: &ResolvedContext) -> Result<()> {
        self.use_database(context.database.as_deref()).await
    }

    async fn use_database(&mut self, database: Option<&str>) -> Result<()> {
        if let Some(database) = database {
            anyhow::ensure!(!database.is_empty(), "The database name must not be empty.");
            self.conn
                .query_drop(format!("USE {}", super::quote_ident(database, '`')))
                .await?;
        } else {
            let current = self.resolved_context().await?;
            anyhow::ensure!(
                current.database.is_none(),
                "Restoring a session with no default database requires an explicit reconnect; temporary objects would be lost."
            );
        }
        Ok(())
    }

    pub async fn children(
        &mut self,
        kind: Option<ObjectKind>,
        path: &[String],
    ) -> Result<Vec<DbObject>> {
        match kind {
            None => {
                let names: Vec<String> = self
                    .conn
                    .query("select schema_name from information_schema.schemata order by schema_name in ('mysql', 'information_schema', 'performance_schema', 'sys'), schema_name")
                    .await?;
                Ok(names
                    .into_iter()
                    .map(|n| DbObject::new(ObjectKind::Schema, n, path))
                    .collect())
            }
            Some(ObjectKind::Schema) => self.schema_objects(path).await,
            Some(ObjectKind::Table | ObjectKind::View) => self.table_details(path).await,
            _ => Ok(Vec::new()),
        }
    }

    /// User tables and views, with their columns when `columns` is set.
    pub async fn completion_meta(&mut self, columns: bool) -> Result<Vec<TableMeta>> {
        let sql = if columns {
            "select t.table_schema, t.table_name, t.table_type, c.column_name, c.column_type
               from information_schema.tables t
               left join information_schema.columns c
                      on c.table_schema = t.table_schema and c.table_name = t.table_name
              where t.table_schema not in ('mysql', 'information_schema', 'performance_schema', 'sys')
              order by t.table_schema, t.table_name, c.ordinal_position"
        } else {
            "select t.table_schema, t.table_name, t.table_type, null, null
               from information_schema.tables t
              where t.table_schema not in ('mysql', 'information_schema', 'performance_schema', 'sys')
              order by t.table_schema, t.table_name"
        };
        let mut rows = self.conn.query_iter(sql).await?;
        let mut out = super::MetaGrouper::default();
        while let Some(row) = rows.next().await? {
            let (s, t, ty, c, ct): MetaRow = mysql_async::from_row_opt(row)?;
            out.push(s, t, ty.contains("VIEW"), c, ct);
        }
        Ok(out.tables)
    }

    pub async fn primary_key(&mut self, schema: Option<&str>, table: &str) -> Result<Vec<String>> {
        let schema = match schema {
            Some(s) => s.to_string(),
            None => self
                .conn
                .query_first("select database()")
                .await?
                .unwrap_or_default(),
        };
        Ok(self
            .conn
            .exec(
                "select column_name from information_schema.key_column_usage
                  where BINARY table_schema = BINARY ? and BINARY table_name = BINARY ? and constraint_name = 'PRIMARY'
                  order by ordinal_position",
                (schema, table.to_string()),
            )
            .await?)
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
        // Keep catalog predicates indexable and validate exact identity after
        // fetching the narrow result, rather than joining information_schema.
        let matches: Vec<(String, String, String)> = self.conn.exec(
            "select table_schema,table_name,table_type from information_schema.tables where table_schema=? and table_name=?",
            (schema, &node.name),
        ).await?;
        let (_, _, kind) = matches
            .into_iter()
            .find(|(found_schema, found_name, _)| {
                found_schema == schema && found_name == &node.name
            })
            .context("object not found with the selected exact schema and name")?;
        let expected = if node.kind == ObjectKind::Table {
            "BASE TABLE"
        } else {
            "VIEW"
        };
        anyhow::ensure!(
            kind == expected,
            "The selected object is no longer a MySQL {expected}."
        );
        let sql = self.ddl(node).await?;
        let coverage = if node.kind == ObjectKind::Table {
            "MySQL SHOW CREATE TABLE definition; triggers and separate grants are not included. Server-provided AUTO_INCREMENT and formatting remain significant."
        } else {
            "MySQL SHOW CREATE VIEW definition; separate grants are not included. Server-provided DEFINER, qualification and formatting remain significant."
        };
        Ok((sql, vec![coverage.into()]))
    }

    pub async fn ddl(&mut self, node: &DbObject) -> Result<String> {
        let q = |s: &str| super::quote_ident(s, '`');
        let schema = node.path.len().checked_sub(2).map(|i| node.path[i].clone());
        let full = match &schema {
            Some(s) => format!("{}.{}", q(s), q(&node.name)),
            None => q(&node.name),
        };
        // SHOW CREATE returns the statement in the second column, or the third for routines.
        let (sql, col) = match node.kind {
            ObjectKind::Schema => (format!("SHOW CREATE DATABASE {}", q(&node.name)), 1),
            ObjectKind::Table => (format!("SHOW CREATE TABLE {full}"), 1),
            ObjectKind::View => (format!("SHOW CREATE VIEW {full}"), 1),
            ObjectKind::Routine if node.detail == "function" => {
                (format!("SHOW CREATE FUNCTION {full}"), 2)
            }
            ObjectKind::Routine => (format!("SHOW CREATE PROCEDURE {full}"), 2),
            _ => anyhow::bail!("no DDL for {:?}", node.kind),
        };
        let row: Row = self
            .conn
            .query_first(sql)
            .await?
            .context("object not found")?;
        if matches!(node.kind, ObjectKind::Table | ObjectKind::View) {
            let expected = if node.kind == ObjectKind::Table {
                "Create Table"
            } else {
                "Create View"
            };
            anyhow::ensure!(
                row.columns_ref()
                    .get(col)
                    .is_some_and(|column| column.name_str() == expected),
                "The object type changed while reading DDL; choose the object again."
            );
        }
        let ddl: Option<String> = row.get(col);
        Ok(format!(
            "{};",
            ddl.context("no definition (missing privileges?)")?
        ))
    }

    async fn schema_objects(&mut self, path: &[String]) -> Result<Vec<DbObject>> {
        let schema = path.last().context("schema path")?.clone();
        let rels: Vec<(String, String, Option<u64>)> = self
            .conn
            .exec(
                "select table_name, table_type, table_rows from information_schema.tables where table_schema = ? order by table_name",
                (schema.clone(),),
            )
            .await?;
        let (mut tables, mut views) = (Vec::new(), Vec::new());
        for (name, ty, est) in rels {
            let is_view = ty.contains("VIEW");
            let detail = est
                .filter(|_| !is_view)
                .map(|n| format!("~{n} rows"))
                .unwrap_or_default();
            let obj = DbObject::new(table_kind(is_view), name, path).with_detail(detail);
            if is_view {
                views.push(obj)
            } else {
                tables.push(obj)
            }
        }
        let routines: Vec<(String, String)> = self
            .conn
            .exec(
                "select routine_name, lower(routine_type) from information_schema.routines where routine_schema = ? order by routine_name",
                (schema,),
            )
            .await?;
        let routines = routines
            .into_iter()
            .map(|(n, t)| DbObject::new(ObjectKind::Routine, n, path).with_detail(t))
            .collect();
        Ok(schema_folders(
            path,
            vec![("tables", tables), ("views", views), ("routines", routines)],
        ))
    }

    async fn table_details(&mut self, path: &[String]) -> Result<Vec<DbObject>> {
        let [.., schema, table] = path else {
            anyhow::bail!("table path")
        };
        let params = (schema.clone(), table.clone());
        let cols: Vec<(String, String, String, Option<String>, String)> = self
            .conn
            .exec(
                "select column_name, column_type, is_nullable, column_default, extra
                   from information_schema.columns
                  where table_schema = ? and table_name = ? order by ordinal_position",
                params.clone(),
            )
            .await?;
        let columns = cols
            .into_iter()
            .map(|(name, ty, nullable, default, extra)| {
                let mut detail = ty;
                if nullable == "NO" {
                    detail.push_str(" not null");
                }
                if let Some(d) = default {
                    detail.push_str(&format!(" = {d}"));
                }
                if !extra.is_empty() {
                    detail.push_str(&format!(" {extra}"));
                }
                DbObject::new(ObjectKind::Column, name, path).with_detail(detail)
            })
            .collect();

        let keys: Vec<(String, String)> = self
            .conn
            .exec(
                "select constraint_name, lower(constraint_type) from information_schema.table_constraints
                  where table_schema = ? and table_name = ? order by constraint_type = 'PRIMARY KEY' desc, constraint_name",
                params.clone(),
            )
            .await?;
        let keys = keys
            .into_iter()
            .map(|(n, t)| DbObject::new(ObjectKind::Key, n, path).with_detail(t))
            .collect();

        let indexes: Vec<(String, String, i64)> = self
            .conn
            .exec(
                "select index_name, group_concat(column_name order by seq_in_index), min(non_unique)
                   from information_schema.statistics
                  where table_schema = ? and table_name = ? group by index_name order by index_name",
                params,
            )
            .await?;
        let indexes = indexes
            .into_iter()
            .map(|(n, cols, non_unique)| {
                let detail = if non_unique == 0 {
                    format!("unique ({cols})")
                } else {
                    format!("({cols})")
                };
                DbObject::new(ObjectKind::Index, n, path).with_detail(detail)
            })
            .collect();
        Ok(table_folders(path, columns, keys, indexes))
    }
}

fn type_name(t: ColumnType) -> String {
    format!("{t:?}")
        .trim_start_matches("MYSQL_TYPE_")
        .to_lowercase()
}

fn is_integer(t: ColumnType) -> bool {
    use ColumnType::*;
    matches!(
        t,
        MYSQL_TYPE_TINY
            | MYSQL_TYPE_SHORT
            | MYSQL_TYPE_LONG
            | MYSQL_TYPE_LONGLONG
            | MYSQL_TYPE_INT24
            | MYSQL_TYPE_YEAR
    )
}

fn bind_values(values: &[Value]) -> Vec<mysql_async::Value> {
    values
        .iter()
        .map(|value| match value {
            Value::Null => mysql_async::Value::NULL,
            Value::Bool(value) => mysql_async::Value::Int(i64::from(*value)),
            Value::Int(value) => mysql_async::Value::Int(*value),
            Value::Float(value) => mysql_async::Value::Double(*value),
            Value::Decimal(value) => value
                .parse::<u64>()
                .map(mysql_async::Value::UInt)
                .unwrap_or_else(|_| mysql_async::Value::Bytes(value.as_bytes().to_vec())),
            Value::Text(value) => mysql_async::Value::Bytes(value.as_bytes().to_vec()),
            Value::Bytes(value) => mysql_async::Value::Bytes(value.clone()),
        })
        .collect()
}

fn is_binary(t: ColumnType, binary_charset: bool) -> bool {
    use ColumnType::*;
    binary_charset
        && matches!(
            t,
            MYSQL_TYPE_BLOB
                | MYSQL_TYPE_TINY_BLOB
                | MYSQL_TYPE_MEDIUM_BLOB
                | MYSQL_TYPE_LONG_BLOB
                | MYSQL_TYPE_GEOMETRY
                | MYSQL_TYPE_BIT
                | MYSQL_TYPE_STRING
                | MYSQL_TYPE_VAR_STRING
                | MYSQL_TYPE_VARCHAR
        )
}

/// The text protocol returns most values as bytes; the column type decides
/// how to present them.
fn column_origins(columns: &[mysql_async::Column]) -> Vec<Option<ColumnOrigin>> {
    columns
        .iter()
        .map(|column| {
            let schema = std::str::from_utf8(column.schema_ref()).ok()?;
            let table = std::str::from_utf8(column.org_table_ref()).ok()?;
            let name = std::str::from_utf8(column.org_name_ref()).ok()?;
            if schema.is_empty() || table.is_empty() || name.is_empty() {
                return None;
            }
            Some(ColumnOrigin {
                table: OriginTable::Named {
                    schema: schema.into(),
                    table: table.into(),
                },
                column: OriginColumn::Named(name.into()),
            })
        })
        .collect()
}

fn row_values(row: Row, types: &[(ColumnType, bool, u8)]) -> Vec<Value> {
    row.unwrap()
        .into_iter()
        .zip(types)
        .map(|(v, &(t, bin, decimals))| decode_value(v, t, bin, decimals))
        .collect()
}

fn decode_value(v: mysql_async::Value, t: ColumnType, bin: bool, decimals: u8) -> Value {
    match v {
        mysql_async::Value::NULL => Value::Null,
        mysql_async::Value::Int(i) => Value::Int(i),
        mysql_async::Value::UInt(u) => i64::try_from(u)
            .map(Value::Int)
            .unwrap_or_else(|_| Value::Decimal(u.to_string())),
        mysql_async::Value::Float(f) => Value::Float(f as f64),
        mysql_async::Value::Double(f) => Value::Float(f),
        mysql_async::Value::Bytes(b) => match String::from_utf8(b) {
            Ok(s) if is_integer(t) => match s.parse::<i64>() {
                Ok(value) => Value::Int(value),
                Err(_) if s.parse::<u64>().is_ok() => Value::Decimal(s),
                Err(_) => Value::Text(s),
            },
            Ok(s)
                if matches!(
                    t,
                    ColumnType::MYSQL_TYPE_DECIMAL | ColumnType::MYSQL_TYPE_NEWDECIMAL
                ) =>
            {
                Value::Decimal(s)
            }
            Ok(s)
                if matches!(
                    t,
                    ColumnType::MYSQL_TYPE_FLOAT | ColumnType::MYSQL_TYPE_DOUBLE
                ) =>
            {
                s.parse().map(Value::Float).unwrap_or(Value::Text(s))
            }
            Ok(s) if !is_binary(t, bin) => Value::Text(s),
            Ok(s) => Value::Bytes(s.into_bytes()),
            Err(e) => Value::Bytes(e.into_bytes()),
        },
        mysql_async::Value::Time(negative, days, hours, minutes, seconds, micros) => {
            // Binary TIME stores days separately. SQL literal formatting uses
            // three hour digits, unlike the server's text protocol; retain
            // its two-digit minimum and the column's fractional precision.
            let hours = u64::from(days) * 24 + u64::from(hours);
            let sign = if negative { "-" } else { "" };
            let mut text = format!("{sign}{hours:02}:{minutes:02}:{seconds:02}");
            append_fraction(&mut text, micros, decimals);
            Value::Text(text)
        }
        mysql_async::Value::Date(year, month, day, hours, minutes, seconds, micros) => {
            let mut text = format!("{year:04}-{month:02}-{day:02}");
            if !matches!(
                t,
                ColumnType::MYSQL_TYPE_DATE | ColumnType::MYSQL_TYPE_NEWDATE
            ) {
                use std::fmt::Write;
                write!(text, " {hours:02}:{minutes:02}:{seconds:02}").unwrap();
                append_fraction(&mut text, micros, decimals);
            }
            Value::Text(text)
        }
    }
}

fn append_fraction(text: &mut String, micros: u32, decimals: u8) {
    use std::fmt::Write;
    // 0..=6 is the server's temporal precision. An unspecified precision must
    // not discard microseconds, even for an expression without column metadata.
    let precision = if decimals <= 6 {
        decimals
    } else {
        u8::from(micros != 0) * 6
    };
    if precision > 0 {
        let fraction = micros / 10u32.pow(u32::from(6 - precision));
        write!(text, ".{fraction:0width$}", width = usize::from(precision)).unwrap();
    }
}

/// Built-in functions without side effects whose results do not depend on
/// how many rows the server produces.
const PAGE_HINT_FUNCTIONS: &[&str] = &[
    "abs", "ascii", "avg", "bit_length", "ceil", "ceiling", "char_length", "character_length",
    "coalesce", "concat", "concat_ws", "count", "curdate", "current_date", "current_time",
    "current_timestamp", "date", "date_add", "date_format", "date_sub", "datediff", "day",
    "dayofmonth", "dayofweek", "dense_rank", "field", "find_in_set", "first_value", "floor",
    "format", "greatest", "group_concat", "hex", "hour", "if", "ifnull", "instr", "json_array",
    "json_extract", "json_length", "json_object", "json_unquote", "lag", "last_value", "lcase",
    "lead", "least", "left", "length", "locate", "lower", "lpad", "ltrim", "max", "md5", "min",
    "minute", "mod", "month", "now", "nullif", "octet_length", "position", "pow", "power",
    "rank", "repeat", "replace", "reverse", "right", "round", "row_number", "rpad", "rtrim",
    "second", "sha1", "sha2", "sign", "sqrt", "str_to_date", "substr", "substring", "sum",
    "time", "timestampdiff", "trim", "truncate", "ucase", "unhex", "upper", "year",
];

/// Byte offset just after the leading SELECT keyword when `sql` is a single
/// plain SELECT whose result may end on the server after a page: no locking
/// clause, no INTO, no SQL_CALC_FOUND_ROWS, no variables, and only built-in
/// functions from [`PAGE_HINT_FUNCTIONS`], so producing fewer rows changes
/// nothing but the rows not read. Anything the parser and MySQL could read
/// differently (executable or hint comments, backslashes, double quotes,
/// `--` without a space) is refused, and such SQL keeps the full read.
fn page_hint_offset(sql: &str) -> Option<usize> {
    use sqlparser::ast::{Expr, Statement};
    use sqlparser::dialect::MySqlDialect;
    use sqlparser::tokenizer::{Token, Tokenizer};
    if sql.len() > 256 * 1024
        || sql.contains("/*!")
        || sql.contains("/*+")
        || sql.to_ascii_lowercase().contains("/*m!")
        || sql.contains(['\\', '"', '\0'])
    {
        return None;
    }
    let bytes = sql.as_bytes();
    if sql.ends_with("--")
        || bytes
            .windows(3)
            .any(|w| w[0] == b'-' && w[1] == b'-' && !w[2].is_ascii_whitespace())
    {
        return None;
    }
    // The statement must start with SELECT after whitespace and comments.
    let mut at = 0;
    loop {
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        if bytes[at..].starts_with(b"/*") {
            at += sql[at + 2..].find("*/")? + 4;
        } else if bytes[at..].starts_with(b"-- ") || bytes[at..].starts_with(b"#") {
            at += sql[at..].find('\n').map_or(sql.len() - at, |end| end + 1);
        } else {
            break;
        }
    }
    let keyword = sql.get(at..at + 6)?;
    if !keyword.eq_ignore_ascii_case("select")
        || bytes
            .get(at + 6)
            .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'$')
    {
        return None;
    }
    let dialect = MySqlDialect {};
    let tokens = Tokenizer::new(&dialect, sql).tokenize().ok()?;
    let refused = tokens.iter().any(|token| match token {
        Token::Word(word) => {
            word.value.starts_with('@')
                || (word.quote_style.is_none()
                    && ["into", "for", "lock", "sql_calc_found_rows", "procedure"]
                        .iter()
                        .any(|refused| word.value.eq_ignore_ascii_case(refused)))
        }
        Token::AtSign | Token::Assignment => true,
        other => other.to_string().contains('@'),
    });
    if refused {
        return None;
    }
    let statements = sqlparser::parser::Parser::new(&dialect)
        .with_recursion_limit(64)
        .try_with_sql(sql)
        .ok()?
        .parse_statements()
        .ok()?;
    let [Statement::Query(query)] = statements.as_slice() else {
        return None;
    };
    if !query.locks.is_empty() || query.for_clause.is_some() {
        return None;
    }
    let mut safe = true;
    let _ = sqlparser::ast::visit_expressions(query, |expr| {
        if let Expr::Function(function) = expr {
            let name = function.name.to_string().to_ascii_lowercase();
            safe &= PAGE_HINT_FUNCTIONS.contains(&name.as_str());
        }
        std::ops::ControlFlow::<()>::Continue(())
    });
    safe.then_some(at + 6)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_selects_end_on_the_server_after_their_page() {
        for (sql, offset) in [
            ("select * from big", 6),
            ("  SELECT id, lower(name) FROM t WHERE x IN (1, 2) ORDER BY id;", 8),
            ("-- note\nselect count(*) from t group by k", 14),
            ("/* c */ select a from t union all select b from u", 14),
            ("# note\nSELECT `for`, 'into' FROM t LIMIT 10", 13),
            ("select date_format(d, '%Y') from t join u on t.id = u.id", 6),
        ] {
            assert_eq!(page_hint_offset(sql), Some(offset), "{sql}");
        }
        for sql in [
            "with a as (select 1) select * from a",
            "(select 1) union (select 2)",
            "select * from t for update",
            "select * from t lock in share mode",
            "select * from t for share",
            "select a into @x from t",
            "select a from t into outfile '/tmp/x'",
            "select sql_calc_found_rows * from t",
            "select @x := a from t",
            "select @@version",
            "select sleep(1) from t",
            "select my_func(a) from t",
            "select db.f(a) from t",
            "select rand() from t",
            "select 1 --sleep(5)",
            "select \"a\" from t",
            "select 'it\\'s' from t",
            "select /*+ MAX_EXECUTION_TIME(1) */ * from t",
            "select /*! sleep(1) */ * from t",
            "select 1; delete from t",
            "insert into t select * from u",
            "update t set a = 1",
            "selectx from t",
            "select 'unterminated",
        ] {
            assert_eq!(page_hint_offset(sql), None, "{sql}");
        }
    }

    #[test]
    fn binary_time_matches_text_protocol_for_sign_large_hours_and_precision() {
        for (value, precision, expected) in [
            (
                mysql_async::Value::Time(false, 0, 3, 4, 5, 0),
                0,
                "03:04:05",
            ),
            (
                mysql_async::Value::Time(true, 0, 3, 4, 5, 0),
                0,
                "-03:04:05",
            ),
            (
                mysql_async::Value::Time(false, 34, 22, 59, 59, 123456),
                6,
                "838:59:59.123456",
            ),
            (
                mysql_async::Value::Time(true, 34, 22, 59, 59, 1),
                6,
                "-838:59:59.000001",
            ),
            (
                mysql_async::Value::Time(false, 1, 3, 4, 5, 120000),
                3,
                "27:04:05.120",
            ),
            (
                mysql_async::Value::Time(false, 0, 0, 0, 0, 0),
                6,
                "00:00:00.000000",
            ),
        ] {
            let binary = decode_value(value, ColumnType::MYSQL_TYPE_TIME, true, precision);
            let text = decode_value(
                mysql_async::Value::Bytes(expected.as_bytes().to_vec()),
                ColumnType::MYSQL_TYPE_TIME,
                true,
                precision,
            );
            assert_eq!(binary, Value::Text(expected.into()));
            assert_eq!(binary, text);
        }
    }

    #[test]
    fn binary_datetime_retains_midnight_and_declared_fractional_precision() {
        for (ty, value, precision, expected) in [
            (
                ColumnType::MYSQL_TYPE_DATE,
                mysql_async::Value::Date(2024, 1, 2, 0, 0, 0, 0),
                0,
                "2024-01-02",
            ),
            (
                ColumnType::MYSQL_TYPE_DATETIME,
                mysql_async::Value::Date(2024, 1, 2, 0, 0, 0, 0),
                0,
                "2024-01-02 00:00:00",
            ),
            (
                ColumnType::MYSQL_TYPE_DATETIME,
                mysql_async::Value::Date(2024, 1, 2, 3, 4, 5, 0),
                3,
                "2024-01-02 03:04:05.000",
            ),
            (
                ColumnType::MYSQL_TYPE_DATETIME,
                mysql_async::Value::Date(2024, 1, 2, 3, 4, 5, 123000),
                3,
                "2024-01-02 03:04:05.123",
            ),
            (
                ColumnType::MYSQL_TYPE_TIMESTAMP,
                mysql_async::Value::Date(2024, 1, 2, 3, 4, 5, 1),
                6,
                "2024-01-02 03:04:05.000001",
            ),
        ] {
            let binary = decode_value(value, ty, true, precision);
            let text = decode_value(
                mysql_async::Value::Bytes(expected.as_bytes().to_vec()),
                ty,
                true,
                precision,
            );
            assert_eq!(binary, Value::Text(expected.into()));
            assert_eq!(binary, text);
        }
    }
}
