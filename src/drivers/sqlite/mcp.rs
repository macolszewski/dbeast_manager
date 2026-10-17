//! Read-only SQLite handles plus a continuously installed authorization hook.

use super::LiteConn;
use crate::mcp::executor::{
    EXECUTION_TIMEOUT, MAX_COLUMNS, Result, Rows, binary_cell, error, failed, identity_matches,
    text_cell, unsupported,
};
use crate::mcp::{ExecutionOutput, Operation, TableRef};
use crate::model::DataSource;
use rusqlite::{
    Connection, OpenFlags,
    config::DbConfig,
    hooks::{AuthAction, AuthContext, Authorization},
    limits::Limit,
    types::ValueRef,
};
use serde_json::json;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

pub(crate) const LIST_SQL: &str = "SELECT 'main' AS schema,name FROM pragma_table_list WHERE schema='main' AND type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name LIMIT 501";

impl LiteConn {
    pub(crate) async fn open_mcp(
        source: &DataSource,
        strict: bool,
        execution_lease: Option<Arc<dyn Send + Sync>>,
    ) -> anyhow::Result<Self> {
        let file = source.file.clone();
        let (connection, execution_lease) = tokio::task::spawn_blocking(
            move || -> anyhow::Result<(Connection, Option<Arc<dyn Send + Sync>>)> {
                // Do not accept URI options or create a missing database. Only the
                // path of the already shared source is used, never MCP input.
                let flags = if strict {
                    OpenFlags::SQLITE_OPEN_READ_ONLY
                } else {
                    OpenFlags::SQLITE_OPEN_READ_WRITE
                };
                let connection =
                    Connection::open_with_flags(file, flags | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
                connection.busy_timeout(std::time::Duration::from_secs(3))?;
                connection.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
                connection.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)?;
                connection.set_db_config(DbConfig::SQLITE_DBCONFIG_DQS_DML, false)?;
                connection.set_db_config(DbConfig::SQLITE_DBCONFIG_DQS_DDL, false)?;
                connection.set_limit(
                    Limit::SQLITE_LIMIT_LENGTH,
                    crate::mcp::MAX_RESULT_BYTES as i32,
                )?;
                connection.set_limit(
                    Limit::SQLITE_LIMIT_SQL_LENGTH,
                    crate::mcp::MAX_SQL_BYTES as i32,
                )?;
                connection.set_limit(Limit::SQLITE_LIMIT_COLUMN, 128)?;
                connection.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0)?;
                connection.set_limit(Limit::SQLITE_LIMIT_EXPR_DEPTH, 100)?;
                connection.set_limit(Limit::SQLITE_LIMIT_COMPOUND_SELECT, 16)?;
                connection.set_limit(Limit::SQLITE_LIMIT_VDBE_OP, 100_000)?;
                if strict {
                    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_VIEW, false)?;
                    connection.execute_batch("PRAGMA query_only=ON; BEGIN")?;
                }
                // The tuple drops the native connection before the lease if the
                // awaiting request disappeared during blocking file I/O.
                Ok((connection, execution_lease))
            },
        )
        .await??;
        let interrupt = Arc::new(connection.get_interrupt_handle());
        Ok(Self {
            conn: Arc::new(Mutex::new(connection)),
            interrupt,
            mcp_execution_lease: execution_lease,
        })
    }

    pub(crate) async fn execute_mcp(
        &self,
        operation: Operation,
        sql: String,
        expected: Option<String>,
        cancellation: CancellationToken,
        read_tables: Option<Vec<TableRef>>,
    ) -> Result<ExecutionOutput> {
        let connection = self.conn.clone();
        let execution_lease = self.mcp_execution_lease.clone();
        tokio::task::spawn_blocking(move || {
            // Keep capacity through cancellation and native handle destruction,
            // including a busy handler or an OS call that cannot be interrupted.
            let _execution_lease=execution_lease;
            let connection_handle=connection;
            let connection = connection_handle.lock().map_err(failed)?;
            let deadline = std::time::Instant::now()+EXECUTION_TIMEOUT;
            let progress_cancel = cancellation.clone();
            connection.progress_handler(1000,Some(move || progress_cancel.is_cancelled() || std::time::Instant::now()>=deadline)).map_err(failed)?;
            if cancellation.is_cancelled() {return Err(error("cancelled","The request was cancelled."));}
            if matches!(operation,Operation::ListTables{..}) {
                let mut statement = connection.prepare(LIST_SQL).map_err(failed)?;
                let rows = statement.query_map([],|row|row.get::<_,String>(1)).map_err(failed)?;
                let mut tables = Vec::new();
                let mut truncated = false;
                for row in rows {
                    if tables.len()==crate::mcp::MAX_ROWS as usize {truncated=true;break;}
                    tables.push(json!({"schema":"main","name":row.map_err(failed)?}));
                }
                return crate::drivers::mcp::json_output(json!({"tables":tables,"truncated":truncated,"note":"Each table is verified again before describing or reading it."}),None);
            }
            let identity = if let Some(tables)=read_tables {
                let mut identity=None;
                for table in &tables {
                    let (verified,_)=table_metadata(&connection,table,if tables.len()==1 {expected.as_deref()} else {None})?;
                    if tables.len()==1 {identity=Some(verified);}
                }
                install_read_sql_authorizer(&connection,tables,cancellation.clone())?;
                identity
            } else if let Some(table) = operation.table() {
                let (identity,columns) = table_metadata(&connection,table,expected.as_deref())?;
                if matches!(operation,Operation::DescribeTable{..}) {
                    let columns=columns.into_iter().map(|(name,kind)|json!({"name":name,"type":kind})).collect::<Vec<_>>();
                    return crate::drivers::mcp::json_output(json!({"table":table,"columns":columns}),Some(identity));
                }
                if let Operation::ReadTable{columns:selected,..}=&operation {
                    if !selected.iter().all(|column|columns.iter().any(|(name,_)|name==column)){return Err(error("invalid_input","A selected column is unavailable."));}
                    install_strict_authorizer(&connection,table.name.clone(),selected.clone(),cancellation.clone())?;
                }
                Some(identity)
            } else {
                // Full SQL still cannot escape the shared file through ATTACH,
                // VACUUM INTO or load an extension into the application process.
                connection.authorizer(Some(move |context:AuthContext<'_>| {
                    if cancellation.is_cancelled() {return Authorization::Deny;}
                    if context.database_name.is_some_and(|database|database!="main") {return Authorization::Deny;}
                    match context.action {
                        AuthAction::Attach{..}|AuthAction::Detach{..}|AuthAction::CreateVtable{..}|AuthAction::DropVtable{..} => Authorization::Deny,
                        AuthAction::Function{function_name} if ["load_extension","readfile","writefile","edit","fts3_tokenizer"].iter().any(|name|function_name.eq_ignore_ascii_case(name)) => Authorization::Deny,
                        AuthAction::Pragma{..}|AuthAction::CreateTempTable{..}|AuthAction::CreateTempIndex{..}|AuthAction::CreateTempView{..}|AuthAction::CreateTempTrigger{..} => Authorization::Deny,
                        _=>Authorization::Allow,
                    }
                })).map_err(failed)?;
                None
            };
            // rusqlite's prepare/execute additionally rejects trailing statements.
            let mut statement=connection.prepare(&sql).map_err(failed)?;
            let mut output=Rows::new(statement.column_names().into_iter().map(str::to_owned).collect())?;
            if statement.column_count()==0 {
                let affected=statement.execute([]).map_err(failed)?;
                return Ok(output.finish(identity,Some(affected as u64)));
            }
            let count=statement.column_count();
            let mut cursor=statement.query([]).map_err(failed)?;
            while let Some(row)=cursor.next().map_err(failed)? {
                let cells=(0..count).map(|index|match row.get_ref(index).map_err(failed)? {
                    ValueRef::Null=>Ok(serde_json::Value::Null),
                    ValueRef::Integer(value)=>text_cell(Some(&value.to_string())),
                    ValueRef::Real(value)=>text_cell(Some(&value.to_string())),
                    ValueRef::Text(bytes)=>text_cell(Some(std::str::from_utf8(bytes).map_err(failed)?)),
                    ValueRef::Blob(bytes)=>binary_cell(bytes),
                }).collect::<Result<Vec<_>>>()?;
                if !output.push(cells)? {
                    if operation.is_full_sql(){return Err(failed("The bounded result did not include command completion"));}
                    break;
                }
            }
            Ok(output.finish(identity,None))
        }).await.map_err(failed)?
    }
}

fn table_metadata(
    connection: &Connection,
    table: &TableRef,
    expected: Option<&str>,
) -> Result<(String, Vec<(String, String)>)> {
    table_metadata_unpinned(connection, table, expected)
        .map_err(|failure| crate::mcp::executor::pinned_table_error(failure, expected))
}

fn table_metadata_unpinned(
    connection: &Connection,
    table: &TableRef,
    expected: Option<&str>,
) -> Result<(String, Vec<(String, String)>)> {
    // Reading the schema starts the read transaction and pins its schema/data
    // snapshot. In rollback journal mode it also prevents DDL until close.
    // WAL readers continue to see the same physical snapshot across a rename.
    let version: i64 = connection
        .query_row("PRAGMA main.schema_version", [], |row| row.get(0))
        .map_err(failed)?;
    let info: Option<(String, i64, String)> = connection
        .query_row(
            "SELECT type,rootpage,sql FROM main.sqlite_schema WHERE name=?1",
            [&table.name],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(failed)?;
    let (kind, root, definition) = info.ok_or_else(unsupported)?;
    if kind != "table" || root <= 0 {
        return Err(unsupported());
    }
    let kind: String = connection
        .query_row(
            "SELECT type FROM pragma_table_list WHERE schema='main' AND name=?1",
            [&table.name],
            |row| row.get(0),
        )
        .map_err(failed)?;
    if kind != "table" {
        return Err(unsupported());
    }
    let mut statement = connection
        .prepare("SELECT name,type,hidden FROM pragma_table_xinfo(?1,'main') ORDER BY cid LIMIT 65")
        .map_err(failed)?;
    let rows = statement
        .query_map([&table.name], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .map_err(failed)?;
    let mut columns = Vec::new();
    for row in rows {
        let (name, kind, hidden) = row.map_err(failed)?;
        if hidden != 0 || columns.len() == MAX_COLUMNS {
            return Err(unsupported());
        }
        columns.push((name, kind));
    }
    if columns.is_empty() {
        return Err(unsupported());
    }
    // A database-wide schema version deliberately invalidates approvals after
    // any DDL, including drop/recreate with the same SQL and recycled root page.
    let identity = format!("sqlite:{version}:{root}:{definition}");
    identity_matches(&identity, expected)?;
    Ok((identity, columns))
}

use rusqlite::OptionalExtension;

fn install_strict_authorizer(
    connection: &Connection,
    table: String,
    columns: Vec<String>,
    cancellation: CancellationToken,
) -> Result<()> {
    connection
        .authorizer(Some(move |context: AuthContext<'_>| {
            if cancellation.is_cancelled() || context.accessor.is_some() {
                return Authorization::Deny;
            }
            match context.action {
                AuthAction::Select => Authorization::Allow,
                // SQLite additionally requests a table-level read with an empty
                // column name for rowid-only scans. The verified table still has
                // to match; this does not grant another column or relation.
                AuthAction::Read {
                    table_name,
                    column_name,
                } if context.database_name == Some("main")
                    && table_name == table
                    && (column_name.is_empty()
                        || columns.iter().any(|name| name == column_name)) =>
                {
                    Authorization::Allow
                }
                _ => Authorization::Deny,
            }
        }))
        .map_err(failed)
}

fn install_read_sql_authorizer(
    connection: &Connection,
    tables: Vec<TableRef>,
    cancellation: CancellationToken,
) -> Result<()> {
    connection
        .authorizer(Some(move |context: AuthContext<'_>| {
            if cancellation.is_cancelled() {
                return Authorization::Deny;
            }
            match context.action {
                AuthAction::Select => Authorization::Allow,
                AuthAction::Read { table_name, .. }
                    if context.database_name == Some("main")
                        && tables
                            .iter()
                            .any(|table| table.name.eq_ignore_ascii_case(table_name)) =>
                {
                    Authorization::Allow
                }
                AuthAction::Function { function_name }
                    if crate::mcp::executor::sql_policy::SAFE_FUNCTIONS
                        .contains(&function_name.to_ascii_lowercase().as_str())
                        || function_name.eq_ignore_ascii_case("like") =>
                {
                    Authorization::Allow
                }
                _ => Authorization::Deny,
            }
        }))
        .map_err(failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn detached_blocking_worker_keeps_its_execution_lease() {
        let native = Connection::open_in_memory().unwrap();
        let interrupt = Arc::new(native.get_interrupt_handle());
        let connection = Arc::new(Mutex::new(native));
        let lease = Arc::new(());
        let weak = Arc::downgrade(&lease);
        let lite = LiteConn {
            conn: connection.clone(),
            interrupt,
            mcp_execution_lease: Some(lease.clone()),
        };
        let (locked, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _guard = connection.lock().unwrap();
            let _ = locked.send(());
            let _ = wait.recv();
        });
        ready.await.unwrap();
        let request = tokio::spawn(async move {
            lite.execute_mcp(
                Operation::Sql {
                    source_id: "fixture".into(),
                    sql: "SELECT 1".into(),
                },
                "SELECT 1".into(),
                None,
                CancellationToken::new(),
                None,
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while Arc::strong_count(&lease) < 3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        request.abort();
        let _ = request.await;
        drop(lease);
        assert!(
            weak.upgrade().is_some(),
            "a detached native worker still owns capacity"
        );
        release.send(()).unwrap();
        holder.join().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while weak.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn authorizer_remains_in_place_and_blocks_functions_other_tables_and_writes() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("CREATE TABLE allowed(id INTEGER PRIMARY KEY); CREATE TABLE forbidden(id INTEGER); INSERT INTO allowed VALUES(1)").unwrap();
        install_strict_authorizer(
            &connection,
            "allowed".into(),
            vec!["id".into()],
            CancellationToken::new(),
        )
        .unwrap();
        let value: i64 = connection
            .query_row("SELECT id FROM main.allowed", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, 1);
        for sql in [
            "SELECT id FROM forbidden",
            "SELECT abs(id) FROM allowed",
            "DELETE FROM allowed",
            "ATTACH ':memory:' AS other",
            "PRAGMA query_only=off",
        ] {
            assert!(connection.prepare(sql).is_err(), "{sql}");
        }
    }
}
