use super::{ServerInfo, quote_ident, schema_folders, table_folders, table_kind};
use crate::model::{
    BoundQuery, BoundUpdate, Column, ColumnOrigin, DataSource, DbObject, ForeignKey,
    ForeignKeyColumn, ObjectKind, OriginColumn, OriginTable, Page, PhysicalColumn, PhysicalTable,
    QueryResult, RelatedDirection, RelatedMetadata, ResolvedContext, ResultSource, SchemaSelection,
    SQLITE_ROWID, TableEditMetadata, TableMeta, Value,
};
use anyhow::{Context, Result};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, InterruptHandle, OpenFlags};
use std::sync::{Arc, Mutex};
pub(super) mod graph;
pub(super) mod mcp;

/// rusqlite is synchronous, so every call runs on the blocking thread pool.
pub struct LiteConn {
    conn: Arc<Mutex<Connection>>,
    interrupt: Arc<InterruptHandle>,
    mcp_execution_lease: Option<Arc<dyn Send + Sync>>,
}

impl LiteConn {
    pub async fn open(ds: &DataSource) -> Result<Self> {
        let file = ds.file.clone();
        let read_only = ds.read_only;
        let conn = tokio::task::spawn_blocking(move || -> Result<Connection> {
            let flags = if read_only {
                OpenFlags::SQLITE_OPEN_READ_ONLY
            } else {
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
            };
            Ok(Connection::open_with_flags(
                &file,
                flags | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?)
        })
        .await??;
        let interrupt = Arc::new(conn.get_interrupt_handle());
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            interrupt,
            mcp_execution_lease: None,
        })
    }

    pub fn interrupt_handle(&self) -> Arc<InterruptHandle> {
        self.interrupt.clone()
    }

    async fn with<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || f(&conn.lock().unwrap())).await?
    }

    pub async fn server_info(&mut self) -> Result<ServerInfo> {
        Ok(ServerInfo {
            product: "SQLite".into(),
            version: rusqlite::version().to_string(),
            transport: "Local file".into(),
        })
    }

    pub async fn execute(&mut self, sql: &str, page: Page) -> Result<QueryResult> {
        self.execute_values(sql, Vec::new(), page).await
    }

    async fn execute_values(
        &self,
        sql: &str,
        params: Vec<rusqlite::types::Value>,
        page: Page,
    ) -> Result<QueryResult> {
        let sql = sql.to_string();
        self.with(move |conn| {
            let mut stmt = conn.prepare(&sql)?;
            if stmt.column_count() == 0 {
                let affected = stmt.execute(rusqlite::params_from_iter(&params))?;
                return Ok(QueryResult {
                    affected: Some(affected as u64),
                    ..Default::default()
                });
            }
            let columns: Vec<Column> = stmt
                .columns()
                .iter()
                .map(|c| Column {
                    name: c.name().to_string(),
                    type_name: c.decl_type().unwrap_or("").to_lowercase(),
                })
                .collect();
            let origins = column_origins(&stmt);
            let n = columns.len();
            let mut exact_columns = vec![true; n];
            let mut rows = Vec::new();
            let mut truncated = false;
            let mut seen = 0usize;
            let mut cursor = stmt.query(rusqlite::params_from_iter(&params))?;
            while let Some(row) = cursor.next()? {
                seen += 1;
                if seen <= page.skip {
                    continue;
                }
                if rows.len() == page.limit {
                    truncated = true;
                    break;
                }
                rows.push(
                    (0..n)
                        .map(|i| {
                            let raw = row.get_ref_unwrap(i);
                            if let ValueRef::Text(bytes) = raw {
                                exact_columns[i] &= std::str::from_utf8(bytes).is_ok();
                            }
                            value(raw)
                        })
                        .collect(),
                );
            }
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
        })
        .await
    }

    pub async fn insert_text_rows(&self, target: &super::InsertTarget, rows: &[Vec<Option<String>>]) -> Result<()> {
        let sql = format!(
            "INSERT INTO {} ({}) VALUES ({})",
            target.qualified('"'),
            target.column_list('"'),
            vec!["?"; target.columns.len()].join(", ")
        );
        let rows = rows.to_vec();
        self.with(move |conn| {
            let mut insert = conn.prepare_cached(&sql)?;
            for row in &rows {
                insert.execute(rusqlite::params_from_iter(row.iter().map(|v| v.as_deref())))?;
            }
            Ok(())
        })
        .await
    }

    pub async fn stream_rows(
        &self,
        query: &BoundQuery,
        tx: tokio::sync::mpsc::Sender<super::RowBatch>,
    ) -> Result<u64> {
        let sql = query.sql.clone();
        let params = bind_values(&query.params);
        self.with(move |conn| {
            let mut stmt = conn.prepare(&sql)?;
            anyhow::ensure!(stmt.column_count() > 0, "The statement returns no rows.");
            let columns: Vec<Column> = stmt
                .columns()
                .iter()
                .map(|c| Column {
                    name: c.name().to_string(),
                    type_name: c.decl_type().unwrap_or("").to_lowercase(),
                })
                .collect();
            let n = columns.len();
            tx.blocking_send(super::RowBatch::Origins(column_origins(&stmt))).map_err(|_| super::stopped())?;
            tx.blocking_send(super::RowBatch::Columns(columns)).map_err(|_| super::stopped())?;
            let mut cursor = stmt.query(rusqlite::params_from_iter(&params))?;
            let mut batch = Vec::with_capacity(super::STREAM_BATCH);
            let mut total = 0u64;
            while let Some(row) = cursor.next()? {
                batch.push((0..n).map(|i| value(row.get_ref_unwrap(i))).collect());
                if batch.len() == super::STREAM_BATCH {
                    total += batch.len() as u64;
                    let full = std::mem::replace(&mut batch, Vec::with_capacity(super::STREAM_BATCH));
                    tx.blocking_send(super::RowBatch::Rows(full)).map_err(|_| super::stopped())?;
                }
            }
            total += batch.len() as u64;
            if !batch.is_empty() {
                tx.blocking_send(super::RowBatch::Rows(batch)).map_err(|_| super::stopped())?;
            }
            Ok(total)
        })
        .await
    }

    pub async fn execute_bound_query(
        &self,
        query: &BoundQuery,
        page: Page,
        expected: Option<&ResolvedContext>,
    ) -> Result<QueryResult> {
        let context = self.resolved_context();
        super::check_expected_context(Some(&context), expected)?;
        let mut data = self
            .execute_values(&query.sql, bind_values(&query.params), page)
            .await?;
        anyhow::ensure!(data.has_result_set(), "A related query must return rows.");
        data.context = Some(context);
        Ok(data)
    }

    pub async fn foreign_keys(
        &self,
        origin: &OriginTable,
        direction: RelatedDirection,
    ) -> Result<RelatedMetadata> {
        let OriginTable::Named { schema, table } = origin else {
            anyhow::bail!("The result does not contain SQLite table provenance.");
        };
        anyhow::ensure!(
            schema == "main",
            "Related Rows currently supports only the main database of a persistent SQLite file, not temporary or attached tables."
        );
        let table = table.clone();
        self.with(move |conn| related_metadata(conn, &table, direction))
            .await
    }

    pub async fn execute_control(&self, sql: &str) -> Result<()> {
        let sql = sql.to_string();
        self.with(move |conn| Ok(conn.execute_batch(&sql)?)).await
    }

    pub async fn table_edit_metadata(&self, origin: &OriginTable) -> Result<TableEditMetadata> {
        let OriginTable::Named { schema, table } = origin else {
            anyhow::bail!("The result does not contain SQLite table provenance.");
        };
        let (schema, table) = (schema.clone(), table.clone());
        self.with(move |conn| {
            let (kind, without_rowid): (String, bool) = conn.query_row(
                "select type, wr from pragma_table_list where schema=?1 and name=?2",
                [&schema, &table], |row| Ok((row.get(0)?, row.get(1)?)))?;
            anyhow::ensure!(kind == "table", "Only ordinary SQLite tables can be edited from a console result.");
            let mut statement = conn.prepare(
                "select cid, name, type, \"notnull\", pk, hidden from pragma_table_xinfo(?1,?2) order by cid")?;
            let rows: Vec<(i32, String, String, bool, u32, u32)> = statement.query_map(
                [&table, &schema], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)))?
                .collect::<Result<_, _>>()?;
            let has_pk_index: bool = conn.query_row(
                "select exists(select 1 from pragma_index_list(?1,?2) where origin='pk')",
                [&table, &schema], |row| row.get(0))?;
            let mut key: Vec<_> = rows.iter().filter(|row| row.4 > 0).map(|row| (row.4, row.1.clone())).collect();
            key.sort_unstable_by_key(|(position, _)| *position);
            // The rowid pseudo-column is shadowed by a real column of the same name.
            let rowid_usable = !without_rowid
                && !rows.iter().any(|row| ["rowid", "_rowid_", "oid"].iter().any(|name| row.1.eq_ignore_ascii_case(name)));
            let mut columns: Vec<PhysicalColumn> = rows.into_iter().map(|(ordinal, name, ty, notnull, pk, hidden)| PhysicalColumn {
                name, ordinal: (ordinal + 1) as u32,
                nullable: !(notnull || (pk > 0 && (without_rowid || (!has_pk_index && ty.eq_ignore_ascii_case("INTEGER"))))),
                generated: hidden != 0, update_default: false, comparable: true,
            }).collect();
            let mut primary_key: Vec<String> = key.into_iter().map(|(_, name)| name).collect();
            // Legacy SQLite allows NULL in a non-INTEGER primary key, so such a
            // key cannot identify a row. A rowid always can.
            let key_usable = !primary_key.is_empty()
                && primary_key.iter().all(|name| columns.iter().any(|c| &c.name == name && !c.nullable));
            if !key_usable && rowid_usable {
                primary_key = vec![SQLITE_ROWID.into()];
                columns.push(PhysicalColumn {
                    name: SQLITE_ROWID.into(), ordinal: 0, nullable: false,
                    generated: true, update_default: false, comparable: true,
                });
            }
            Ok(TableEditMetadata {
                table: PhysicalTable { schema, name: table, oid: None }, columns,
                primary_key, transactional: true,
            })
        }).await
    }

    pub async fn execute_bound_update(&self, update: &BoundUpdate) -> Result<u64> {
        let sql = update.sql.clone();
        let params = bind_values(&update.params);
        self.with(move |conn| Ok(conn.execute(&sql, rusqlite::params_from_iter(params))? as u64))
            .await
    }

    pub fn resolved_context(&self) -> ResolvedContext {
        ResolvedContext {
            database: None,
            search_path: None,
            schemas: vec!["main".into()],
            temporary_schema: None,
        }
    }

    pub async fn has_open_transaction(&self) -> Result<bool> {
        self.with(|conn| Ok(!conn.is_autocommit())).await
    }

    pub async fn change_schema(&self, selection: &SchemaSelection) -> Result<ResolvedContext> {
        anyhow::ensure!(
            matches!(selection, SchemaSelection::Default)
                || matches!(selection, SchemaSelection::Named(name) if name == "main"),
            "SQLite does not support changing the default schema. Use a qualified schema.table name."
        );
        Ok(self.resolved_context())
    }

    pub async fn completion_meta(&mut self) -> Result<Vec<TableMeta>> {
        self.with(|conn| {
            let mut rows = Vec::new();
            let schemas: Vec<String> = conn
                .prepare("select name from pragma_database_list order by seq")?
                .query_map([], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            for schema in schemas {
                let sql = format!(
                    "select m.name, m.type = 'view', p.name, p.type
                       from {}.sqlite_master m
                       left join pragma_table_info(m.name, ?1) p
                      where m.type in ('table', 'view') and m.name not like 'sqlite\\_%' escape '\\'
                      order by m.name, p.cid",
                    quote_ident(&schema, '"')
                );
                let mut stmt = conn.prepare(&sql)?;
                let found = stmt.query_map([&schema], |r| {
                    Ok((
                        schema.clone(),
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get::<_, Option<String>>(3)?.map(|t| t.to_lowercase()),
                    ))
                })?;
                for row in found {
                    rows.push(row?);
                }
            }
            Ok(super::group_meta(rows.into_iter()))
        })
        .await
    }

    pub async fn primary_key(&mut self, schema: &str, table: &str) -> Result<Vec<String>> {
        let (schema, table) = (schema.to_string(), table.to_string());
        self.with(move |conn| {
            let mut stmt = conn
                .prepare("select name from pragma_table_info(?1, ?2) where pk > 0 order by pk")?;
            let cols = stmt
                .query_map([&table, &schema], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            Ok(cols)
        })
        .await
    }

    pub async fn ddl_for_comparison(&mut self, node: &DbObject) -> Result<(String, Vec<String>)> {
        anyhow::ensure!(
            matches!(node.kind, ObjectKind::Table | ObjectKind::View),
            "DDL Diff supports tables and views only."
        );
        anyhow::ensure!(
            node.path.len() == 2 && node.path[0] == "main",
            "DDL Diff supports persistent SQLite main objects only; temporary and attached databases are not supported."
        );
        self.with(|conn| {
            let databases: Vec<(String, String)> = conn
                .prepare("pragma database_list")?
                .query_map([], |row| Ok((row.get(1)?, row.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?;
            anyhow::ensure!(
                databases
                    .iter()
                    .any(|(name, file)| name == "main" && !file.is_empty()),
                "DDL Diff requires a persistent SQLite file; in-memory databases are not supported."
            );
            anyhow::ensure!(
                databases
                    .iter()
                    .all(|(name, _)| name == "main" || name == "temp"),
                "DDL Diff does not reproduce SQLite ATTACH; open the database file directly."
            );
            Ok(())
        })
        .await?;
        let sql = self.ddl(node).await?;
        let coverage = if node.kind == ObjectKind::Table {
            "SQLite stored CREATE TABLE plus its explicit index and trigger definitions. Automatic indexes with no stored SQL are represented by the table constraints."
        } else {
            "SQLite stored CREATE VIEW definition. INSTEAD OF triggers are not included."
        };
        Ok((sql, vec![coverage.into()]))
    }

    /// SQLite keeps the original CREATE text; indexes and triggers of a table are appended.
    pub async fn ddl(&mut self, node: &DbObject) -> Result<String> {
        let schema = node
            .path
            .len()
            .checked_sub(2)
            .map_or("main".to_string(), |i| node.path[i].clone());
        let name = node.name.clone();
        let kind = node.kind;
        self.with(move |conn| {
            if kind == ObjectKind::Schema {
                return Ok(format!("-- attached database {name}"));
            }
            let expected_kind = match kind {
                ObjectKind::Table => "table",
                ObjectKind::View => "view",
                ObjectKind::Index => "index",
                _ => anyhow::bail!("no DDL for {kind:?}"),
            };
            // Triggers have a separate namespace. Prove the base object and
            // read its associated definitions in the same statement snapshot.
            let sql = format!(
                "select sql, name = ?1 and type = ?3 from {}.sqlite_master
                  where sql is not null and ((name = ?1 and type = ?3) or (tbl_name = ?1 and type in ('index', 'trigger') and ?2))
                  order by type = 'table' or type = 'view' desc, type, name",
                quote_ident(&schema, '"')
            );
            let mut stmt = conn.prepare(&sql)?;
            let parts: Vec<(String, bool)> = stmt
                .query_map(rusqlite::params![name, kind == ObjectKind::Table, expected_kind], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()?;
            anyhow::ensure!(parts.iter().any(|(_, base)| *base), "object not found with the selected type");
            Ok(parts.into_iter().map(|(sql, _)| format!("{sql};")).collect::<Vec<_>>().join("\n\n"))
        })
        .await
    }

    pub async fn children(
        &mut self,
        kind: Option<ObjectKind>,
        path: &[String],
    ) -> Result<Vec<DbObject>> {
        let path = path.to_vec();
        self.with(move |conn| match kind {
            None => {
                let mut stmt =
                    conn.prepare("select name from pragma_database_list order by seq")?;
                let names = stmt
                    .query_map([], |r| r.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(names
                    .into_iter()
                    .map(|n| DbObject::new(ObjectKind::Schema, n, &path))
                    .collect())
            }
            Some(ObjectKind::Schema) => schema_objects(conn, &path),
            Some(ObjectKind::Table | ObjectKind::View) => table_details(conn, &path),
            _ => Ok(Vec::new()),
        })
        .await
    }
}

fn bind_values(values: &[Value]) -> Vec<rusqlite::types::Value> {
    values
        .iter()
        .map(|value| match value {
            Value::Null => rusqlite::types::Value::Null,
            Value::Bool(value) => rusqlite::types::Value::Integer(i64::from(*value)),
            Value::Int(value) => rusqlite::types::Value::Integer(*value),
            Value::Float(value) => rusqlite::types::Value::Real(*value),
            Value::Decimal(value) | Value::Text(value) => {
                rusqlite::types::Value::Text(value.clone())
            }
            Value::Bytes(value) => rusqlite::types::Value::Blob(value.clone()),
        })
        .collect()
}

fn related_metadata(
    conn: &Connection,
    selected: &str,
    direction: RelatedDirection,
) -> Result<RelatedMetadata> {
    let databases: Vec<(String, String)> = conn
        .prepare("select name,file from pragma_database_list")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?;
    anyhow::ensure!(
        databases
            .iter()
            .any(|(name, file)| name == "main" && !file.is_empty()),
        "Related Rows requires a persistent SQLite file; in-memory and temporary databases cannot be opened in a separate session."
    );
    anyhow::ensure!(
        databases
            .iter()
            .all(|(name, _)| name == "main" || name == "temp"),
        "Related Rows cannot reproduce SQLite ATTACH in a separate session. Use a connection to the file directly."
    );
    let canonical_table = |name: &str| -> Result<String> {
        let (name, kind): (String, String) = conn.query_row(
            "select name,type from pragma_table_list where schema='main' and name=?1 collate nocase",
            [name], |row| Ok((row.get(0)?, row.get(1)?)))?;
        anyhow::ensure!(
            kind == "table" && !name.starts_with("sqlite_"),
            "Related Rows requires ordinary persistent SQLite tables."
        );
        Ok(name)
    };
    let selected = canonical_table(selected)?;
    let tables = match direction {
        RelatedDirection::Referenced => vec![selected.clone()],
        RelatedDirection::Referencing => conn.prepare("select name from pragma_table_list where schema='main' and type='table' and name not glob 'sqlite_*' order by name")?
            .query_map([], |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?,
    };
    type TableColumns = Arc<Vec<(String, String, u32)>>;
    let mut columns: std::collections::HashMap<String, TableColumns> =
        std::collections::HashMap::new();
    let mut read_columns = |table: &str| -> Result<TableColumns> {
        if let Some(columns) = columns.get(table) {
            return Ok(columns.clone());
        }
        let rows = conn
            .prepare("select name,type,pk from pragma_table_xinfo(?1,'main') order by cid")?
            .query_map([table], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        let rows = Arc::new(rows);
        columns.insert(table.to_string(), rows.clone());
        Ok(rows)
    };
    let physical = |name: String| PhysicalTable {
        schema: "main".into(),
        name,
        oid: None,
    };
    let mut keys = Vec::new();
    let mut foreign_keys = conn.prepare("select id,seq,\"table\",\"from\",\"to\" from pragma_foreign_key_list(?1,'main') order by id,seq")?;
    for source in tables {
        type FkRow = (i64, i64, String, String, Option<String>);
        let rows: Vec<FkRow> = foreign_keys
            .query_map([&source], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })?
            .collect::<Result<_, _>>()?;
        let mut cursor = 0;
        while cursor < rows.len() {
            let start = cursor;
            let id = rows[start].0;
            while cursor < rows.len() && rows[cursor].0 == id {
                cursor += 1;
            }
            let parts = &rows[start..cursor];
            if direction == RelatedDirection::Referencing
                && !parts[0].2.eq_ignore_ascii_case(&selected)
            {
                continue;
            }
            let target = if direction == RelatedDirection::Referencing {
                selected.clone()
            } else {
                canonical_table(&parts[0].2)?
            };
            let source_columns = read_columns(&source)?;
            let target_columns = read_columns(&target)?;
            let implicit = parts.iter().all(|part| part.4.is_none());
            anyhow::ensure!(
                implicit || parts.iter().all(|part| part.4.is_some()),
                "The SQLite foreign key mixes implicit and explicit target columns."
            );
            let mut pk: Vec<_> = target_columns
                .iter()
                .filter(|column| column.2 > 0)
                .collect();
            pk.sort_unstable_by_key(|column| column.2);
            anyhow::ensure!(
                !implicit || pk.len() == parts.len(),
                "The SQLite foreign key does not match the referenced composite primary key."
            );
            let mut key = ForeignKey {
                name: format!("FK {id}"),
                catalog_id: id.to_string(),
                source: physical(source.clone()),
                target: physical(target),
                columns: Vec::with_capacity(parts.len()),
            };
            for (index, part) in parts.iter().enumerate() {
                anyhow::ensure!(
                    part.1 == index as i64 && part.2.eq_ignore_ascii_case(&parts[0].2),
                    "Invalid SQLite foreign key column order."
                );
                let sc = source_columns
                    .iter()
                    .find(|column| column.0.eq_ignore_ascii_case(&part.3))
                    .ok_or_else(|| {
                        anyhow::anyhow!("A foreign key source column no longer exists.")
                    })?;
                let target_name = if implicit {
                    pk[index].0.as_str()
                } else {
                    part.4.as_deref().unwrap()
                };
                let tc = target_columns
                    .iter()
                    .find(|column| column.0.eq_ignore_ascii_case(target_name))
                    .ok_or_else(|| {
                        anyhow::anyhow!("A foreign key target column no longer exists.")
                    })?;
                key.columns.push(ForeignKeyColumn {
                    source_name: sc.0.clone(),
                    source_origin: OriginColumn::Named(sc.0.clone()),
                    source_type: sc.1.clone(),
                    target_name: tc.0.clone(),
                    target_origin: OriginColumn::Named(tc.0.clone()),
                    target_type: tc.1.clone(),
                });
            }
            keys.push(key);
        }
    }
    Ok(RelatedMetadata {
        table: physical(selected),
        keys,
    })
}

fn column_origins(stmt: &rusqlite::Statement<'_>) -> Vec<Option<ColumnOrigin>> {
    stmt.columns_with_metadata()
        .iter()
        .map(|column| {
            Some(ColumnOrigin {
                table: OriginTable::Named {
                    schema: column.database_name()?.to_string(),
                    table: column.table_name()?.to_string(),
                },
                column: OriginColumn::Named(column.origin_name()?.to_string()),
            })
        })
        .collect()
}

fn value(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::Int(i),
        ValueRef::Real(f) => Value::Float(f),
        ValueRef::Text(t) => Value::Text(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::Bytes(b.to_vec()),
    }
}

fn schema_objects(conn: &Connection, path: &[String]) -> Result<Vec<DbObject>> {
    let schema = path.last().context("schema path")?;
    let sql = format!(
        "select name, type from {}.sqlite_master where type in ('table', 'view') and name not like 'sqlite\\_%' escape '\\' order by name",
        quote_ident(schema, '"')
    );
    let mut stmt = conn.prepare(&sql)?;
    let rels = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let (mut tables, mut views) = (Vec::new(), Vec::new());
    for rel in rels {
        let (name, ty) = rel?;
        let is_view = ty == "view";
        let obj = DbObject::new(table_kind(is_view), name, path);
        if is_view {
            views.push(obj)
        } else {
            tables.push(obj)
        }
    }
    Ok(schema_folders(
        path,
        vec![("tables", tables), ("views", views)],
    ))
}

fn table_details(conn: &Connection, path: &[String]) -> Result<Vec<DbObject>> {
    let [.., schema, table] = path else {
        anyhow::bail!("table path")
    };
    let mut stmt = conn.prepare("select name, type, \"notnull\", dflt_value, pk from pragma_table_xinfo(?1, ?2) order by cid")?;
    let columns = stmt
        .query_map([table, schema], |r| {
            let mut detail = r.get::<_, String>(1)?.to_lowercase();
            if r.get::<_, bool>(2)? {
                detail.push_str(" not null");
            }
            if let Some(d) = r.get::<_, Option<String>>(3)? {
                detail.push_str(&format!(" = {d}"));
            }
            if r.get::<_, i64>(4)? > 0 {
                detail.push_str(" primary key");
            }
            Ok(DbObject::new(ObjectKind::Column, r.get::<_, String>(0)?, path).with_detail(detail))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut stmt = conn.prepare("select distinct \"table\", group_concat(\"from\") from pragma_foreign_key_list(?1, ?2) group by id")?;
    let keys = stmt
        .query_map([table, schema], |r| {
            let target: String = r.get(0)?;
            let cols: String = r.get(1)?;
            Ok(DbObject::new(ObjectKind::Key, format!("fk_{target}"), path)
                .with_detail(format!("({cols}) -> {target}")))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut stmt = conn
        .prepare("select name, \"unique\", origin from pragma_index_list(?1, ?2) order by name")?;
    let indexes = stmt
        .query_map([table, schema], |r| {
            let unique = if r.get::<_, bool>(1)? { "unique " } else { "" };
            let origin: String = r.get(2)?;
            Ok(
                DbObject::new(ObjectKind::Index, r.get::<_, String>(0)?, path)
                    .with_detail(format!("{unique}{origin}")),
            )
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(table_folders(path, columns, keys, indexes))
}
