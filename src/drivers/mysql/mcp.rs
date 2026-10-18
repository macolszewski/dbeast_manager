//! MySQL strict reads require InnoDB identity metadata and explicit read locks.

use super::{MyConn, Opts, OptsBuilder, open_socket};
use crate::mcp::executor::{
    MAX_COLUMNS, MAX_WIRE_BYTES, Result, Rows, binary_cell, error, failed, identity_matches,
    qualified, text_cell, unsupported,
};
use crate::mcp::{ExecutionOutput, Operation, TableRef};
use crate::model::{DataSource, DriverKind};
use mysql_async::prelude::*;
use mysql_async::{Row, Value};
use serde_json::json;

/// mysql_async's normal Drop drains pending results in a detached recycler
/// (`cleanup_for_pool` in 0.37), which reads an abandoned query's whole tail
/// or waits on an unresponsive server without limit. Every socket, MCP and
/// application alike, instead closes after at most a second of `disconnect`;
/// a drain still running then is cut off by dropping the stream.
pub(super) struct ManagedConn {
    inner: Option<mysql_async::Conn>,
    /// Opened for an MCP execution, which cancels through [`McpCancel`].
    mcp: bool,
    execution_lease: Option<std::sync::Arc<dyn Send + Sync>>,
}
impl ManagedConn {
    async fn connect(
        opts: Opts,
        execution_lease: Option<std::sync::Arc<dyn Send + Sync>>,
    ) -> anyhow::Result<Self> {
        Self::connect_with_timeout(opts, execution_lease, std::time::Duration::from_secs(15)).await
    }

    async fn connect_with_timeout(
        opts: Opts,
        execution_lease: Option<std::sync::Arc<dyn Send + Sync>>,
        deadline: std::time::Duration,
    ) -> anyhow::Result<Self> {
        // The native constructor must never run SQL: without pending results or
        // a transaction, its cancellation recycler closes the socket directly.
        // Keep the lease in native Opts too, which outlives the native stream,
        // including when the deadline drops a partially constructed connection.
        let options_lease = execution_lease.clone();
        let opts = OptsBuilder::from_opts(opts)
            .prefer_socket(false)
            .max_allowed_packet(Some(MAX_WIRE_BYTES))
            .wait_timeout(Some(30))
            .init(Vec::<String>::new())
            .setup(Vec::<String>::new())
            .after_connect(move |_| {
                let lease = options_lease.clone();
                Box::pin(async move {
                    drop(lease);
                    Ok(())
                })
            })
            .into();
        // Caller cancellation cannot remove the constructor's own deadline or
        // release capacity while a partially opened native socket still lives.
        tokio::spawn(async move {
            let connection = tokio::time::timeout(deadline, open_socket(opts)).await??;
            Ok(Self::bounded(connection, execution_lease))
        })
        .await?
    }

    pub(super) async fn close(mut self) {
        if let Some(connection) = self.inner.take() {
            let _ =
                tokio::time::timeout(std::time::Duration::from_secs(1), connection.disconnect())
                    .await;
        }
    }
    pub(super) fn ordinary(connection: mysql_async::Conn) -> Self {
        Self {
            inner: Some(connection),
            mcp: false,
            execution_lease: None,
        }
    }
    fn bounded(
        connection: mysql_async::Conn,
        execution_lease: Option<std::sync::Arc<dyn Send + Sync>>,
    ) -> Self {
        Self {
            inner: Some(connection),
            mcp: true,
            execution_lease,
        }
    }
}

pub struct McpCancel {
    opts: Opts,
    connection_id: u32,
    execution_lease: Option<std::sync::Arc<dyn Send + Sync>>,
}
impl McpCancel {
    pub(crate) async fn cancel(self) -> anyhow::Result<()> {
        let mut connection = ManagedConn::connect(self.opts, self.execution_lease).await?;
        connection
            .query_drop(format!("KILL QUERY {}", self.connection_id))
            .await?;
        connection.close().await;
        Ok(())
    }
}
impl std::ops::Deref for ManagedConn {
    type Target = mysql_async::Conn;
    fn deref(&self) -> &Self::Target {
        self.inner.as_ref().expect("live MySQL connection")
    }
}
impl std::ops::DerefMut for ManagedConn {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.inner.as_mut().expect("live MySQL connection")
    }
}
impl Drop for ManagedConn {
    fn drop(&mut self) {
        if let Some(connection) = self.inner.take() {
            // Also the guard for MCP cancellation during setup. The normal MCP
            // path awaits close_mcp before releasing its concurrency permit.
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let execution_lease = self.execution_lease.take();
                runtime.spawn(async move {
                    let _execution_lease = execution_lease;
                    let _ = tokio::time::timeout(
                        std::time::Duration::from_secs(1),
                        connection.disconnect(),
                    )
                    .await;
                });
            }
        }
    }
}

/// Static reasons, one per cause, so the agent can tell why a strict read was
/// refused without seeing anything it did not send.
const MISSING_TABLE: &str = "Strict read could not find this table in the source database, or the account cannot see it.";
const NOT_A_TABLE: &str = "Strict MySQL read supports physical tables only. Views and system views are not supported.";
const NOT_INNODB: &str = "Strict MySQL read supports InnoDB tables only. This table uses another storage engine.";
const PARTITIONED: &str = "Strict MySQL read does not support partitioned tables.";
const TOO_MANY_COLUMNS: &str = "Strict read supports tables with at most 64 columns. The whole table is checked, including columns the query does not select.";
const UNSUPPORTED_COLUMN_TYPE: &str = "Strict MySQL read does not support a column type in this table, for example spatial types such as POINT or GEOMETRY. The whole table is checked, including columns the query does not select.";
const GENERATED_COLUMN: &str = "Strict MySQL read does not support tables with generated columns. The whole table is checked, including columns the query does not select.";

/// Refusal for a row of `information_schema.TABLES` that is not a plain InnoDB table.
fn table_kind_error(row: Option<(String, Option<String>)>) -> Option<crate::mcp::ExecutionError> {
    let message = match row {
        None => MISSING_TABLE,
        Some((kind, _)) if kind != "BASE TABLE" => NOT_A_TABLE,
        Some((_, engine)) if engine.as_deref() != Some("InnoDB") => NOT_INNODB,
        Some(_) => return None,
    };
    Some(error("unsupported_safe_read", message))
}

pub(crate) const LIST_SQL: &str = "SELECT TABLE_SCHEMA,TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA=? AND TABLE_TYPE='BASE TABLE' AND ENGINE='InnoDB' ORDER BY TABLE_NAME LIMIT 501";

impl MyConn {
    pub(crate) async fn open_mcp(
        ds: &DataSource,
        endpoint: &crate::drivers::Endpoint,
        strict: bool,
        execution_lease: Option<std::sync::Arc<dyn Send + Sync>>,
    ) -> anyhow::Result<Self> {
        let mut builder = OptsBuilder::default()
            .ip_or_hostname(endpoint.host.clone())
            .tcp_port(endpoint.port)
            .user(Some(ds.user.clone()))
            .pass(Some(ds.password.clone()))
            .client_found_rows(true)
            .prefer_socket(false)
            .max_allowed_packet(Some(MAX_WIRE_BYTES))
            // Supplying settings avoids SQL during the native constructor.
            // Every query, including cancellation, starts under ManagedConn.
            .wait_timeout(Some(30))
            .stmt_cache_size(0);
        if !ds.database.is_empty() {
            builder = builder.db_name(Some(ds.database.clone()));
        }
        if let Some(material) = crate::drivers::tls::prepare(ds).await? {
            builder = builder.ssl_opts(Some(
                material
                    .mysql
                    .with_danger_tls_hostname_override(Some(ds.host.clone())),
            ));
        }
        let opts: Opts = builder.into();
        let mut conn = ManagedConn::connect(opts.clone(), execution_lease).await?;
        conn.query_drop("SET SESSION max_execution_time=30000")
            .await?;
        conn.query_drop("SET SESSION lock_wait_timeout=3").await?;
        if strict {
            conn.query_drop("SET SESSION sql_mode='ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ZERO_IN_DATE,NO_ZERO_DATE,ERROR_FOR_DIVISION_BY_ZERO,NO_ENGINE_SUBSTITUTION,NO_BACKSLASH_ESCAPES,IGNORE_SPACE'").await?;
            conn.query_drop("SET SESSION TRANSACTION READ ONLY").await?;
            conn.query_drop("SET autocommit=0").await?;
        }
        Ok(Self {
            conn,
            opts,
            initial_database: if ds.database.is_empty() {
                None
            } else {
                Some(ds.database.clone())
            },
            tls_mode: ds.tls.mode,
        })
    }

    pub(crate) async fn close_mcp(mut self) {
        if let Some(connection) = self.conn.inner.take() {
            let _ =
                tokio::time::timeout(std::time::Duration::from_secs(1), connection.disconnect())
                    .await;
        }
    }

    pub(crate) fn mcp_cancel_handle(&self) -> Option<McpCancel> {
        self.conn.mcp.then(|| McpCancel {
            opts: self.opts.clone(),
            connection_id: self.conn.id(),
            execution_lease: self.conn.execution_lease.clone(),
        })
    }

    async fn mcp_table(
        &mut self,
        table: &TableRef,
        expected: Option<&str>,
        acquire_lock: bool,
    ) -> Result<(String, Vec<(String, String)>)> {
        self.mcp_table_unpinned(table, expected, acquire_lock)
            .await
            .map_err(|failure| crate::mcp::executor::pinned_table_error(failure, expected))
    }

    async fn mcp_table_unpinned(
        &mut self,
        table: &TableRef,
        expected: Option<&str>,
        acquire_lock: bool,
    ) -> Result<(String, Vec<(String, String)>)> {
        let parameters = (&table.schema, &table.name);
        // Avoid opening known unsupported objects. Post-lock validation below is
        // authoritative; the server/engine administrator is a trusted boundary.
        let row: Option<(String, Option<String>)> = self.conn.exec_first("SELECT TABLE_TYPE,ENGINE FROM information_schema.TABLES WHERE TABLE_SCHEMA=? AND TABLE_NAME=?", parameters).await.map_err(failed)?;
        if let Some(refusal) = table_kind_error(row) {
            return Err(refusal);
        }
        // START TRANSACTION would release LOCK TABLES. The session READ ONLY +
        // autocommit=0 defaults apply to the transaction under this read lock.
        if acquire_lock {
            self.conn.query_drop(format!("LOCK TABLES {} READ",qualified(table,DriverKind::MySql))).await.map_err(|_| error("unsupported_safe_read", "Strict MySQL read requires LOCK TABLES and access to physical table identity metadata."))?;
        }
        let row: Option<(String,Option<String>)> = self.conn.exec_first("SELECT TABLE_TYPE,ENGINE FROM information_schema.TABLES WHERE TABLE_SCHEMA=? AND TABLE_NAME=?", parameters).await.map_err(failed)?;
        if let Some(refusal) = table_kind_error(row) {
            return Err(refusal);
        }
        let partitions: Option<u64> = self.conn.exec_first("SELECT COUNT(*) FROM information_schema.PARTITIONS WHERE TABLE_SCHEMA=? AND TABLE_NAME=? AND PARTITION_NAME IS NOT NULL", parameters).await.map_err(failed)?;
        if partitions != Some(0) {
            return Err(error("unsupported_safe_read", PARTITIONED));
        }
        // TABLE_ID survives rename and changes on replacement. CREATE_TIME and
        // SHOW CREATE alone cannot distinguish two equally defined tables.
        let innodb_name = format!("{}/{}", table.schema, table.name);
        let physical: Option<u64> = self.conn.exec_first("SELECT TABLE_ID FROM information_schema.INNODB_TABLES WHERE NAME=?", (&innodb_name,)).await.map_err(|_| error("unsupported_safe_read", "Strict MySQL read requires the global PROCESS privilege to read physical table identity metadata."))?;
        let physical = physical.ok_or_else(|| error("unsupported_safe_read", "Strict MySQL read could not find the InnoDB identity of this table."))?;
        let rows: Vec<Row> = self.conn.exec("SELECT COLUMN_NAME,DATA_TYPE,COLUMN_TYPE,EXTRA,GENERATION_EXPRESSION,ORDINAL_POSITION,COLLATION_NAME FROM information_schema.COLUMNS WHERE TABLE_SCHEMA=? AND TABLE_NAME=? ORDER BY ORDINAL_POSITION LIMIT 65", parameters).await.map_err(failed)?;
        if rows.is_empty() {
            return Err(error("unsupported_safe_read", MISSING_TABLE));
        }
        if rows.len() > MAX_COLUMNS {
            return Err(error("unsupported_safe_read", TOO_MANY_COLUMNS));
        }
        let mut identity = format!("mysql:{innodb_name}:{physical}");
        let mut columns = Vec::with_capacity(rows.len());
        for row in rows {
            let name: String = row.get(0).ok_or_else(unsupported)?;
            let kind: String = row.get(1).ok_or_else(unsupported)?;
            let type_name: String = row.get(2).ok_or_else(unsupported)?;
            let extra: String = row.get(3).ok_or_else(unsupported)?;
            let generated: String = row.get(4).ok_or_else(unsupported)?;
            let ordinal: u64 = row.get(5).ok_or_else(unsupported)?;
            let collation: Option<String> = row.get(6).ok_or_else(unsupported)?;
            if ![
                "tinyint",
                "smallint",
                "mediumint",
                "int",
                "bigint",
                "decimal",
                "float",
                "double",
                "bit",
                "char",
                "varchar",
                "binary",
                "varbinary",
                "tinytext",
                "text",
                "mediumtext",
                "longtext",
                "tinyblob",
                "blob",
                "mediumblob",
                "longblob",
                "date",
                "time",
                "datetime",
                "timestamp",
                "year",
                "json",
                "enum",
                "set",
            ]
            .contains(&kind.as_str())
            {
                return Err(error("unsupported_safe_read", UNSUPPORTED_COLUMN_TYPE));
            }
            if !generated.is_empty()
                || extra.contains("VIRTUAL GENERATED")
                || extra.contains("STORED GENERATED")
            {
                return Err(error("unsupported_safe_read", GENERATED_COLUMN));
            }
            identity.push_str(&format!(
                ":{ordinal}:{name}:{type_name}:{extra}:{collation:?}"
            ));
            columns.push((name, type_name));
        }
        identity_matches(&identity, expected)?;
        Ok((identity, columns))
    }

    pub(crate) async fn execute_mcp(
        &mut self,
        operation: &Operation,
        sql: &str,
        expected: Option<&str>,
        read_plan: Option<&crate::mcp::executor::sql_policy::ReadPlan>,
    ) -> Result<ExecutionOutput> {
        if matches!(operation, Operation::ListTables { .. }) {
            let database = self.initial_database.as_deref().ok_or_else(unsupported)?;
            let rows: Vec<(String, String)> = self
                .conn
                .exec(LIST_SQL, (database,))
                .await
                .map_err(failed)?;
            let truncated = rows.len() > crate::mcp::MAX_ROWS as usize;
            let tables = rows
                .into_iter()
                .take(crate::mcp::MAX_ROWS as usize)
                .map(|(schema, name)| json!({"schema":schema,"name":name}))
                .collect::<Vec<_>>();
            return crate::drivers::mcp::json_output(
                json!({"tables":tables,"truncated":truncated,"note":"Each table is verified again before describing or reading it."}),
                None,
            );
        }
        let identity = if let Some(plan) = read_plan {
            if !plan.mysql_locks.is_empty() {
                for table in &plan.tables {
                    let kind:Option<(String,Option<String>)>=self.conn.exec_first("SELECT TABLE_TYPE,ENGINE FROM information_schema.TABLES WHERE TABLE_SCHEMA=? AND TABLE_NAME=?",(&table.schema,&table.name)).await.map_err(|failure|crate::mcp::executor::pinned_table_error(failed(failure),expected))?;
                    if let Some(refusal) = table_kind_error(kind) {
                        return Err(crate::mcp::executor::pinned_table_error(refusal, expected));
                    }
                }
                let locks = plan
                    .mysql_locks
                    .iter()
                    .map(|(table, alias)| {
                        format!(
                            "{}{} READ",
                            qualified(table, DriverKind::MySql),
                            alias.as_ref().map_or_else(String::new, |alias| format!(
                                " AS {}",
                                crate::mcp::executor::quote(alias, DriverKind::MySql)
                            ))
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                self.conn
                    .query_drop(format!("LOCK TABLES {locks}"))
                    .await
                    .map_err(|_| {
                        crate::mcp::executor::pinned_table_error(error(
                            "unsupported_safe_read",
                            "Read SQL requires LOCK TABLES privileges for every referenced table.",
                        ),expected)
                    })?;
            }
            let mut identity = None;
            for table in &plan.tables {
                let (verified, _) = self
                    .mcp_table(
                        table,
                        if plan.tables.len() == 1 {
                            expected
                        } else {
                            None
                        },
                        false,
                    )
                    .await?;
                if plan.tables.len() == 1 {
                    identity = Some(verified);
                }
            }
            identity
        } else if let Some(table) = operation.table() {
            let (identity, columns) = self.mcp_table(table, expected, true).await?;
            if matches!(operation, Operation::DescribeTable { .. }) {
                let columns = columns
                    .into_iter()
                    .map(|(name, type_name)| json!({"name":name,"type":type_name}))
                    .collect::<Vec<_>>();
                return crate::drivers::mcp::json_output(
                    json!({"table":table,"columns":columns}),
                    Some(identity),
                );
            }
            if let Operation::ReadTable {
                columns: selected, ..
            } = operation
                && !selected
                    .iter()
                    .all(|column| columns.iter().any(|(name, _)| name == column))
            {
                return Err(error("invalid_input", "A selected column is unavailable."));
            }
            Some(identity)
        } else {
            None
        };
        // COM_STMT_PREPARE independently disallows stacked statements and LOCAL
        // INFILE. No local infile handler is installed on this connection.
        let statement = self.conn.prep(sql).await.map_err(failed)?;
        let mut result = self.conn.exec_iter(statement, ()).await.map_err(failed)?;
        let columns = result.columns_ref();
        let binary: Vec<bool> = columns
            .iter()
            .map(|column| {
                column.character_set() == 63
                    && matches!(
                        column.column_type(),
                        mysql_async::consts::ColumnType::MYSQL_TYPE_BLOB
                            | mysql_async::consts::ColumnType::MYSQL_TYPE_TINY_BLOB
                            | mysql_async::consts::ColumnType::MYSQL_TYPE_MEDIUM_BLOB
                            | mysql_async::consts::ColumnType::MYSQL_TYPE_LONG_BLOB
                            | mysql_async::consts::ColumnType::MYSQL_TYPE_VAR_STRING
                            | mysql_async::consts::ColumnType::MYSQL_TYPE_STRING
                            | mysql_async::consts::ColumnType::MYSQL_TYPE_BIT
                    )
            })
            .collect();
        let mut output = Rows::new(
            columns
                .iter()
                .map(|column| column.name_str().into_owned())
                .collect(),
        )?;
        let affected = if columns.is_empty() {
            Some(result.affected_rows())
        } else {
            None
        };
        while let Some(row) = result.next().await.map_err(failed)? {
            let cells = row.unwrap().into_iter().enumerate().map(|(index,value)|match value {
                Value::NULL => Ok(serde_json::Value::Null),
                Value::Bytes(bytes) if binary[index] => binary_cell(&bytes),
                Value::Bytes(bytes) => text_cell(Some(std::str::from_utf8(&bytes).map_err(failed)?)),
                Value::Int(value) => text_cell(Some(&value.to_string())),
                Value::UInt(value) => text_cell(Some(&value.to_string())),
                Value::Float(value) => text_cell(Some(&value.to_string())),
                Value::Double(value) => text_cell(Some(&value.to_string())),
                Value::Date(year,month,day,hour,minute,second,micros) => text_cell(Some(&format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}.{micros:06}"))),
                Value::Time(negative,days,hour,minute,second,micros) => text_cell(Some(&format!("{}{days} {hour:02}:{minute:02}:{second:02}.{micros:06}",if negative {"-"} else {""}))),
            }).collect::<Result<Vec<_>>>()?;
            if !output.push(cells)? {
                if operation.is_full_sql() {
                    return Err(failed(
                        "The bounded result did not include command completion",
                    ));
                }
                break;
            }
        }
        // Do not drain an attacker-sized tail. This disposable connection closes
        // when the executor returns; no result is returned to a shared pool.
        if operation.is_full_sql() && !result.is_empty() {
            return Err(failed("Additional command results were not completed"));
        }
        Ok(output.finish(identity, affected))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn cancelled_constructor_closes_a_stalled_peer_and_releases_capacity() {
        let server = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = server.local_addr().unwrap().port();
        let lease = std::sync::Arc::new(());
        let weak = std::sync::Arc::downgrade(&lease);
        let opts: Opts = OptsBuilder::default()
            .ip_or_hostname("127.0.0.1")
            .tcp_port(port)
            .into();
        let request = tokio::spawn(ManagedConn::connect_with_timeout(
            opts,
            Some(lease.clone()),
            std::time::Duration::from_millis(100),
        ));
        let (mut socket, _) = server.accept().await.unwrap();
        request.abort();
        let _ = request.await;
        drop(lease);
        assert!(
            weak.upgrade().is_some(),
            "the unfinished native handshake must keep capacity"
        );
        // The peer remains open and sends no greeting. The constructor must
        // terminate its own socket, even after its caller has disappeared.
        let mut byte = [0];
        let read = tokio::time::timeout(std::time::Duration::from_secs(2), socket.read(&mut byte))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read, 0);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while weak.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}
