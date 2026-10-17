//! PostgreSQL strict reads lock out schema changes through catalog validation
//! and execution in the same read-only transaction.

use super::{CONTEXT_SQL, Config, ConnectionTask, PgConn};
use crate::mcp::executor::{
    MAX_COLUMNS, MAX_WIRE_BYTES, Result, Rows, error, failed, identity_matches, qualified,
    text_cell,
};
use crate::mcp::{ExecutionOutput, Operation, TableRef};
use crate::model::{DataSource, DriverKind};
use futures_util::TryStreamExt;
use serde_json::json;
use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_postgres::tls::MakeTlsConnect;

/// Identity signature and column names with type names of a proved relation.
type RelationProof<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<(String, Vec<(String, String)>)>> + Send + 'a>,
>;

/// Signature of a proved relation and its columns.
type Proof = (String, Vec<(String, String)>);

/// Relations proved while answering one request. Their locks last until the
/// transaction ends, so a relation reached through several views is proved
/// once; the count bounds the whole tree, not only its depth.
#[derive(Default)]
struct ProofMemo {
    done: std::sync::Mutex<std::collections::HashMap<(String, String), Proof>>,
    count: std::sync::atomic::AtomicUsize,
}

const MAX_PROVED_RELATIONS: usize = 64;

/// pg_catalog routines that accept a whole row as their only required argument.
const ROW_ROUTINES_SQL: &str = "SELECT DISTINCT p.proname::text FROM pg_catalog.pg_proc p WHERE p.pronamespace='pg_catalog'::pg_catalog.regnamespace AND p.prokind<>'p' AND p.pronargs>=1 AND p.pronargs-p.pronargdefaults<=1 AND p.proargtypes[0] IN ('pg_catalog.record'::pg_catalog.regtype,'pg_catalog.\"any\"'::pg_catalog.regtype,'pg_catalog.anyelement'::pg_catalog.regtype,'pg_catalog.anycompatible'::pg_catalog.regtype,'pg_catalog.anynonarray'::pg_catalog.regtype,'pg_catalog.anycompatiblenonarray'::pg_catalog.regtype)";

pub(crate) const LIST_SQL: &str = "SELECT n.nspname AS schema, c.relname AS name FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE (c.relkind='v' OR c.relkind IN ('r','m') AND c.relpersistence='p' AND c.relam=2) AND NOT c.relispartition AND NOT c.relrowsecurity AND NOT c.relforcerowsecurity AND n.nspname NOT LIKE 'pg\\_%' ESCAPE '\\' AND n.nspname <> 'information_schema' AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_inherits i WHERE i.inhrelid=c.oid OR i.inhparent=c.oid) ORDER BY n.nspname,c.relname LIMIT 501";

/// Cap ingress before PostgreSQL's frame decoder can buffer a huge row. This
/// includes TLS records and catalog traffic; exceeding it closes this one-use
/// connection. No unchecked server length is used to reserve application memory.
struct BoundedStream<S> {
    inner: S,
    remaining: usize,
}

impl<S: AsyncRead + Unpin> AsyncRead for BoundedStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if output.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if self.remaining == 0 {
            return Poll::Ready(Err(io::Error::other("MCP wire limit exceeded")));
        }
        let amount = output.remaining().min(self.remaining);
        let mut part = ReadBuf::new(&mut output.initialize_unfilled()[..amount]);
        match Pin::new(&mut self.inner).poll_read(cx, &mut part) {
            Poll::Ready(Ok(())) => {
                let count = part.filled().len();
                output.advance(count);
                self.remaining -= count;
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}
impl<S: AsyncWrite + Unpin> AsyncWrite for BoundedStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, bytes)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

pub struct McpCancel {
    pub(super) token: tokio_postgres::CancelToken,
    pub(super) host: String,
    pub(super) port: u16,
    pub(super) server_name: String,
    pub(super) tls: Option<native_tls::TlsConnector>,
}
impl McpCancel {
    pub(crate) async fn cancel(self) -> anyhow::Result<()> {
        let socket = tokio::net::TcpStream::connect((self.host.as_str(), self.port)).await?;
        if let Some(tls) = self.tls {
            let connector = <postgres_native_tls::MakeTlsConnector as MakeTlsConnect<
                tokio::net::TcpStream,
            >>::make_tls_connect(
                &mut postgres_native_tls::MakeTlsConnector::new(tls),
                &self.server_name,
            )?;
            self.token.cancel_query_raw(socket, connector).await?;
        } else {
            self.token
                .cancel_query_raw(socket, tokio_postgres::NoTls)
                .await?;
        }
        Ok(())
    }
}

impl PgConn {
    pub(crate) async fn open_mcp(
        ds: &DataSource,
        endpoint: &crate::drivers::Endpoint,
        strict: bool,
        execution_lease: Option<std::sync::Arc<dyn Send + Sync>>,
    ) -> anyhow::Result<Self> {
        let mut config = Config::new();
        config
            .user(&ds.user)
            .password(&ds.password)
            .application_name("DBeast MCP");
        if !ds.database.is_empty() {
            config.dbname(&ds.database);
        }
        config.ssl_mode(if ds.use_tls {
            tokio_postgres::config::SslMode::Require
        } else {
            tokio_postgres::config::SslMode::Disable
        });
        // No user-controlled search_path resolution while verifying catalog data.
        config.options("-c search_path=pg_catalog -c standard_conforming_strings=on -c bytea_output=hex -c statement_timeout=30000 -c lock_timeout=3000 -c idle_in_transaction_session_timeout=30000");
        let tls = crate::drivers::tls::prepare(ds)
            .await?
            .and_then(|material| material.connector);
        let socket =
            tokio::net::TcpStream::connect((endpoint.host.as_str(), endpoint.port)).await?;
        socket.set_nodelay(true)?;
        let socket = BoundedStream {
            inner: socket,
            remaining: MAX_WIRE_BYTES,
        };
        let (client, connection_task) = if let Some(tls) = &tls {
            let connector = <postgres_native_tls::MakeTlsConnector as MakeTlsConnect<
                BoundedStream<tokio::net::TcpStream>,
            >>::make_tls_connect(
                &mut postgres_native_tls::MakeTlsConnector::new(tls.clone()),
                &ds.host,
            )?;
            let (client, connection) = config.connect_raw(socket, connector).await?;
            (
                client,
                ConnectionTask(tokio::spawn(async move {
                    let _execution_lease = execution_lease;
                    connection.await
                })),
            )
        } else {
            let (client, connection) = config.connect_raw(socket, tokio_postgres::NoTls).await?;
            (
                client,
                ConnectionTask(tokio::spawn(async move {
                    let _execution_lease = execution_lease;
                    connection.await
                })),
            )
        };
        if strict {
            client.batch_execute("BEGIN READ ONLY; SET LOCAL row_security=off; SET LOCAL enable_indexscan=off; SET LOCAL enable_indexonlyscan=off; SET LOCAL enable_bitmapscan=off; SET LOCAL constraint_exclusion=off").await?;
        }
        let context_statement = client.prepare(CONTEXT_SQL).await?;
        Ok(Self {
            client,
            _connection_task: connection_task,
            tls,
            tls_mode: ds.tls.mode,
            context_statement,
            initial_search_path: "pg_catalog".into(),
            update_statements: Default::default(),
            mcp_endpoint: Some((endpoint.host.clone(), endpoint.port, ds.host.clone())),
        })
    }

    async fn mcp_table(
        &self,
        table: &TableRef,
        expected: Option<&str>,
    ) -> Result<(String, Vec<(String, String)>)> {
        self.mcp_table_unpinned(table, expected)
            .await
            .map_err(|failure| crate::mcp::executor::pinned_table_error(failure, expected))
    }

    async fn mcp_table_unpinned(
        &self,
        table: &TableRef,
        expected: Option<&str>,
    ) -> Result<(String, Vec<(String, String)>)> {
        let memo = ProofMemo::default();
        let (signature, columns) = self.mcp_relation(table, 0, &memo).await?;
        identity_matches(&signature, expected)?;
        Ok((signature, columns))
    }

    /// Proves that reading a relation cannot run user code. Tables and
    /// materialized views are read from storage; a view is expanded by the
    /// planner, so its rule is checked and every relation it reads is proved
    /// recursively. The signature covers the whole tree.
    fn mcp_relation<'a>(
        &'a self,
        table: &'a TableRef,
        depth: usize,
        memo: &'a ProofMemo,
    ) -> RelationProof<'a> {
        Box::pin(async move {
            let key = (table.schema.clone(), table.name.clone());
            if let Some(proof) = memo.done.lock().unwrap().get(&key).cloned() {
                return Ok(proof);
            }
            if memo.count.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= MAX_PROVED_RELATIONS {
                return Err(error(
                    "unsupported_safe_read",
                    "The read depends on more than 64 tables and views.",
                ));
            }
            let proof = self.mcp_relation_uncached(table, depth, memo).await?;
            memo.done.lock().unwrap().insert(key, proof.clone());
            Ok(proof)
        })
    }

    fn mcp_relation_uncached<'a>(
        &'a self,
        table: &'a TableRef,
        depth: usize,
        memo: &'a ProofMemo,
    ) -> RelationProof<'a> {
        Box::pin(async move {
        if depth > 8 {
            return Err(error(
                "unsupported_safe_read",
                "Views nested more than eight levels deep are not supported.",
            ));
        }
        // Validate *after* locking so a concurrent rename or replacement cannot
        // switch the checked object. LOCK does not run SELECT expressions, but
        // PostgreSQL cannot LOCK a materialized view. For one, TABLESAMPLE takes
        // an ACCESS SHARE lock during parse analysis and rejects views before
        // their rule is expanded, so no definition is ever planned or run.
        let qualified_name = qualified(table, DriverKind::Postgres);
        let expected_kind: Option<String> = self
            .client
            .query_opt("SELECT c.relkind::text FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=$1 AND c.relname=$2", &[&table.schema, &table.name])
            .await
            .map_err(failed)?
            .map(|row| row.get(0));
        match expected_kind.as_deref() {
            None => {
                return Err(error(
                    "unsupported_safe_read",
                    "The table does not exist or is not visible to this connection.",
                ));
            }
            Some("r" | "m" | "v") => {}
            Some(_) => {
                return Err(error(
                    "unsupported_safe_read",
                    "Only tables, views and materialized views are supported; foreign and partitioned tables, sequences and indexes are not.",
                ));
            }
        }
        let lock_sql = if expected_kind.as_deref() == Some("m") {
            format!("SELECT FROM ONLY {qualified_name} TABLESAMPLE SYSTEM (0) LIMIT 0")
        } else {
            format!("LOCK TABLE ONLY {qualified_name} IN SHARE UPDATE EXCLUSIVE MODE")
        };
        self.client
            .batch_execute(&lock_sql)
            .await
            .map_err(|failure| match failure.code() {
                Some(&tokio_postgres::error::SqlState::UNDEFINED_TABLE)
                | Some(&tokio_postgres::error::SqlState::INVALID_SCHEMA_NAME) => error(
                    "unsupported_safe_read",
                    "The table does not exist or is not visible to this connection.",
                ),
                Some(&tokio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE) => error(
                    "unsupported_safe_read",
                    "Read-only PostgreSQL access needs permission to lock the table in SHARE UPDATE EXCLUSIVE mode (UPDATE, DELETE, TRUNCATE or MAINTAIN privilege, or table ownership).",
                ),
                Some(&tokio_postgres::error::SqlState::READ_ONLY_SQL_TRANSACTION) => error(
                    "unsupported_safe_read",
                    "Read-only PostgreSQL access requires a primary server; this connection points to a standby.",
                ),
                _ => error(
                    "unsupported_safe_read",
                    "The table could not be locked for verification; it may be busy with schema maintenance.",
                ),
            })?;
        let table_row = self.client.query_opt("SELECT c.oid,c.relkind::text,c.relpersistence::text,c.relam,c.relispartition,c.relrowsecurity,c.relforcerowsecurity,EXISTS (SELECT 1 FROM pg_catalog.pg_inherits i WHERE i.inhrelid=c.oid OR i.inhparent=c.oid),c.xmin::text,pg_catalog.current_database() FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=$1 AND c.relname=$2", &[&table.schema,&table.name]).await.map_err(failed)?.ok_or_else(|| error("unsupported_safe_read", "The table does not exist or is not visible to this connection."))?;
        let oid: u32 = table_row.get(0);
        let relkind: String = table_row.get(1);
        if expected_kind.as_deref() != Some(relkind.as_str()) {
            return Err(error(
                "unsupported_safe_read",
                "The object changed while it was being verified. Retry the request.",
            ));
        }
        if !matches!(relkind.as_str(), "r" | "m" | "v") {
            return Err(error(
                "unsupported_safe_read",
                "Only tables, views and materialized views are supported; foreign and partitioned tables, sequences and indexes are not.",
            ));
        }
        let view_signature = if relkind == "v" {
            self.mcp_view_dependencies(oid, depth, memo).await?
        } else {
            String::new()
        };
        if relkind != "v" && (table_row.get::<_, String>(2) != "p" || table_row.get::<_, u32>(3) != 2) {
            return Err(error(
                "unsupported_safe_read",
                "Temporary, unlogged and non-heap tables are not supported.",
            ));
        }
        if table_row.get::<_, bool>(4) || table_row.get::<_, bool>(7) {
            return Err(error(
                "unsupported_safe_read",
                "Partitions and tables with inheritance are not supported.",
            ));
        }
        if table_row.get::<_, bool>(5) || table_row.get::<_, bool>(6) {
            return Err(error(
                "unsupported_safe_read",
                "Tables with row-level security are not supported.",
            ));
        }
        // Planning loads index definitions and may constant-fold their
        // expressions and predicates, so every routine, operator, type and
        // collation they reference must be built in. Objects created by initdb
        // have OIDs below FirstNormalObjectId (16384); extension and user
        // objects never do. pg_depend lists exactly what each expression uses.
        let unsafe_index: bool = self.client.query_one("SELECT EXISTS(SELECT 1 FROM pg_catalog.pg_index i JOIN pg_catalog.pg_class ic ON ic.oid=i.indexrelid WHERE i.indrelid=$1 AND (ic.relam >= 16384 OR EXISTS(SELECT 1 FROM pg_catalog.unnest(i.indclass) AS cl(opc) WHERE cl.opc >= 16384) OR ((i.indexprs IS NOT NULL OR i.indpred IS NOT NULL) AND EXISTS(SELECT 1 FROM pg_catalog.pg_depend d WHERE d.classid='pg_catalog.pg_class'::pg_catalog.regclass AND d.objid=i.indexrelid AND d.refobjid >= 16384 AND NOT (d.refclassid='pg_catalog.pg_class'::pg_catalog.regclass AND d.refobjid=$1)))))", &[&oid]).await.map_err(failed)?.get(0);
        if unsafe_index {
            return Err(error(
                "unsupported_safe_read",
                "The table has an index that uses an extension or user-defined access method, operator class, function or type.",
            ));
        }
        // Same proof for CHECK predicates, which the optimizer may also fold.
        let unsafe_check: bool = self.client.query_one("SELECT EXISTS(SELECT 1 FROM pg_catalog.pg_constraint c JOIN pg_catalog.pg_depend d ON d.classid='pg_catalog.pg_constraint'::pg_catalog.regclass AND d.objid=c.oid WHERE c.conrelid=$1 AND c.contype='c' AND d.refobjid >= 16384 AND NOT (d.refclassid='pg_catalog.pg_class'::pg_catalog.regclass AND d.refobjid=$1))", &[&oid]).await.map_err(failed)?.get(0);
        if unsafe_check {
            return Err(error(
                "unsupported_safe_read",
                "The table has a CHECK constraint that uses an extension or user-defined function, operator or type.",
            ));
        }
        let rows = self.client.query("SELECT a.attname,t.typname,a.atttypid,a.attgenerated::text,a.attnum,a.atttypmod,a.attcollation,t.typtype::text FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_type t ON t.oid=a.atttypid WHERE a.attrelid=$1 AND a.attnum>0 AND NOT a.attisdropped ORDER BY a.attnum LIMIT 65", &[&oid]).await.map_err(failed)?;
        if rows.is_empty() {
            return Err(error("unsupported_safe_read", "The table has no columns."));
        }
        if rows.len() > MAX_COLUMNS {
            return Err(error(
                "unsupported_safe_read",
                "Tables with more than 64 columns are not supported.",
            ));
        }
        let mut columns = Vec::with_capacity(rows.len());
        let mut signature = format!(
            "pg:{}:{oid}:{}{view_signature}",
            table_row.get::<_, String>(9),
            table_row.get::<_, String>(8)
        );
        for row in rows {
            let type_oid: u32 = row.get(2);
            // Values are returned as text, so a type is safe when its output
            // and comparison routines are built in: base types (including
            // their arrays) created by initdb. Domains, composites, ranges,
            // enums and extension types fail closed.
            if type_oid >= 16384 || row.get::<_, String>(7) != "b" {
                return Err(error(
                    "unsupported_safe_read",
                    "A column uses a domain, enum, composite, range or extension type, which read-only access does not support.",
                ));
            }
            if !row.get::<_, String>(3).is_empty() {
                return Err(error(
                    "unsupported_safe_read",
                    "Tables with generated columns are not supported.",
                ));
            }
            let name: String = row.get(0);
            signature.push_str(&format!(
                ":{}:{type_oid}:{}:{}:{}",
                row.get::<_, i16>(4),
                row.get::<_, i32>(5),
                row.get::<_, u32>(6),
                name
            ));
            columns.push((name, row.get(1)));
        }
        Ok((signature, columns))
        })
    }

    /// Everything a view's rule uses, from pg_depend. Routines must be built in
    /// and IMMUTABLE or STABLE (no side effects); operators, types and
    /// collations must be built in; relations are proved like any read.
    async fn mcp_view_dependencies(&self, view: u32, depth: usize, memo: &ProofMemo) -> Result<String> {
        let rows = self.client.query("SELECT d.refclassid::pg_catalog.regclass::text, d.refobjid, COALESCE((SELECT p.provolatile::text FROM pg_catalog.pg_proc p WHERE d.refclassid='pg_catalog.pg_proc'::pg_catalog.regclass AND p.oid=d.refobjid),''), COALESCE((SELECT n.nspname FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE d.refclassid='pg_catalog.pg_class'::pg_catalog.regclass AND c.oid=d.refobjid),''), COALESCE((SELECT c.relname FROM pg_catalog.pg_class c WHERE d.refclassid='pg_catalog.pg_class'::pg_catalog.regclass AND c.oid=d.refobjid),'') FROM pg_catalog.pg_rewrite r JOIN pg_catalog.pg_depend d ON d.classid='pg_catalog.pg_rewrite'::pg_catalog.regclass AND d.objid=r.oid WHERE r.ev_class=$1 AND NOT (d.refclassid='pg_catalog.pg_class'::pg_catalog.regclass AND d.refobjid=$1) GROUP BY 1,2,3,4,5 ORDER BY 1,2 LIMIT 257", &[&view]).await.map_err(failed)?;
        if rows.len() > 256 {
            return Err(error(
                "unsupported_safe_read",
                "The view references too many objects.",
            ));
        }
        // pg_depend omits pinned (built-in) objects, so it cannot show that a
        // view calls a volatile built-in such as random() or a server signal
        // function. The stored rule tree names every call explicitly.
        let volatile: bool = self.client.query_one("WITH ids AS (SELECT m[1] AS kind, m[2]::pg_catalog.oid AS id FROM pg_catalog.pg_rewrite r, pg_catalog.regexp_matches(r.ev_action::text, ':(funcid|opfuncid|aggfnoid|winfnoid|opno) ([0-9]+)', 'g') AS m WHERE r.ev_class=$1) SELECT EXISTS(SELECT 1 FROM ids LEFT JOIN pg_catalog.pg_operator o ON ids.kind='opno' AND o.oid=ids.id LEFT JOIN pg_catalog.pg_proc p ON p.oid = CASE WHEN ids.kind='opno' THEN o.oprcode ELSE ids.id END WHERE ids.id <> 0 AND (p.oid IS NULL OR p.oid >= 16384 OR p.provolatile NOT IN ('i','s') OR (ids.kind='opno' AND o.oid >= 16384)))", &[&view]).await.map_err(failed)?.get(0);
        if volatile {
            return Err(error(
                "unsupported_safe_read",
                "The view calls a user-defined, extension or volatile function.",
            ));
        }
        let mut signature = String::new();
        for row in rows {
            let class: String = row.get(0);
            let oid: u32 = row.get(1);
            match class.as_str() {
                "pg_class" => {
                    let table = TableRef {
                        schema: row.get(3),
                        name: row.get(4),
                    };
                    let (proof, _) = self.mcp_relation(&table, depth + 1, memo).await?;
                    signature.push_str(&format!("[{proof}]"));
                }
                "pg_proc" if oid < 16384 && matches!(row.get::<_, String>(2).as_str(), "i" | "s") => {}
                "pg_proc" => {
                    return Err(error(
                        "unsupported_safe_read",
                        "The view calls a user-defined, extension or volatile function.",
                    ));
                }
                _ if oid < 16384 => {}
                _ => {
                    return Err(error(
                        "unsupported_safe_read",
                        "The view uses a user-defined or extension operator, type or collation.",
                    ));
                }
            }
        }
        Ok(signature)
    }

    /// PostgreSQL resolves `rel.name` as `name(rel)` when `rel` has no column
    /// `name`. Names on proved relations must be real columns; other
    /// qualified names must not match a routine of this server that accepts a
    /// whole row, including routines newer than the static list.
    async fn mcp_attributes(
        &self,
        plan: &crate::mcp::executor::sql_policy::ReadPlan,
        proved: &[(&TableRef, Vec<(String, String)>)],
    ) -> Result<()> {
        let missing = plan.pg_columns.iter().any(|(table, name)| {
            !proved.iter().any(|(proved, columns)| {
                *proved == table && columns.iter().any(|(column, _)| column == name)
            })
        });
        if missing {
            return Err(error(
                "unsupported_safe_read",
                "A qualified name is not a column of its table. PostgreSQL would call it as a function.",
            ));
        }
        if plan.pg_row_attributes.is_empty() {
            return Ok(());
        }
        let routine: bool = self
            .client
            .query_one(
                &format!("SELECT EXISTS({ROW_ROUTINES_SQL} AND p.proname=ANY($1))"),
                &[&plan.pg_row_attributes],
            )
            .await
            .map_err(failed)?
            .get(0);
        if routine {
            return Err(error(
                "unsupported_safe_read",
                "This qualified name would call a PostgreSQL function on a whole row. Rename the column with an alias.",
            ));
        }
        Ok(())
    }

    pub(crate) async fn execute_mcp(
        &mut self,
        operation: &Operation,
        sql: &str,
        expected: Option<&str>,
        read_plan: Option<&crate::mcp::executor::sql_policy::ReadPlan>,
    ) -> Result<ExecutionOutput> {
        if matches!(operation, Operation::ListTables { .. }) {
            let rows = self.client.query(LIST_SQL, &[]).await.map_err(failed)?;
            let truncated = rows.len() > crate::mcp::MAX_ROWS as usize;
            let tables = rows
                .into_iter()
                .take(crate::mcp::MAX_ROWS as usize)
                .map(|row| json!({"schema":row.get::<_,String>(0),"name":row.get::<_,String>(1)}))
                .collect::<Vec<_>>();
            return crate::drivers::mcp::json_output(
                json!({"tables":tables,"truncated":truncated,"note":"Each table is verified again before describing or reading it."}),
                None,
            );
        }
        let identity = if let Some(plan) = read_plan {
            let mut identity = None;
            let mut proved = Vec::with_capacity(plan.tables.len());
            for table in &plan.tables {
                let (verified, columns) = self
                    .mcp_table(
                        table,
                        if plan.tables.len() == 1 {
                            expected
                        } else {
                            None
                        },
                    )
                    .await?;
                if plan.tables.len() == 1 {
                    identity = Some(verified);
                }
                proved.push((table, columns));
            }
            self.mcp_attributes(plan, &proved).await?;
            identity
        } else if let Some(table) = operation.table() {
            let (identity, columns) = self.mcp_table(table, expected).await?;
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
        // Parse the approved SQL once. Execute that exact prepared plan so a
        // concurrent schema change cannot silently change labels or value types.
        let prepared = self.client.prepare(sql).await.map_err(failed)?;
        self.mcp_execute_prepared(prepared, sql, identity, operation.is_full_sql())
            .await
    }

    async fn mcp_execute_prepared(
        &self,
        prepared: tokio_postgres::Statement,
        sql: &str,
        identity: Option<String>,
        full: bool,
    ) -> Result<ExecutionOutput> {
        if prepared.columns().len() > MAX_COLUMNS {
            return Err(error(
                "limit_exceeded",
                "The result has more than 64 columns.",
            ));
        }
        let mut output = Rows::new(
            prepared
                .columns()
                .iter()
                .map(|column| column.name().to_owned())
                .collect(),
        )?;
        if prepared.columns().is_empty() {
            let affected = self.client.execute(&prepared, &[]).await.map_err(failed)?;
            return Ok(output.finish(identity, Some(affected)));
        }
        let binary: Vec<bool> = prepared
            .columns()
            .iter()
            .map(|column| column.type_().oid() == 17)
            .collect();
        // tokio-postgres does not expose the protocol statement name. This
        // private connection has no agent-created statements, and ambiguity
        // fails closed. SQL EXECUTE preserves text precision while PostgreSQL
        // checks the cached plan's fixed result descriptor before execution.
        let names=self.client.query("SELECT name FROM pg_catalog.pg_prepared_statements WHERE statement=$1 AND NOT from_sql LIMIT 2",&[&sql]).await.map_err(failed)?;
        if names.len() != 1 {
            return Err(failed("The prepared statement identity is ambiguous"));
        }
        let name: String = names[0].get(0);
        let execution_sql = format!(
            "EXECUTE {}",
            crate::mcp::executor::quote(&name, DriverKind::Postgres)
        );
        let stream = self
            .client
            .simple_query_raw(&execution_sql)
            .await
            .map_err(failed)?;
        tokio::pin!(stream);
        let mut affected = None;
        let mut described = false;
        while let Some(message) = stream.try_next().await.map_err(failed)? {
            match message {
                tokio_postgres::SimpleQueryMessage::Row(row) => {
                    if !described || row.len() != prepared.columns().len() {
                        return Err(failed("The result schema changed during execution"));
                    }
                    let cells = (0..row.len())
                        .map(|column| {
                            postgres_cell(
                                row.get(column),
                                *binary.get(column).ok_or_else(|| {
                                    failed("The result schema changed during execution")
                                })?,
                            )
                        })
                        .collect::<Result<Vec<_>>>()?;
                    if !output.push(cells)? {
                        if full {
                            return Err(failed(
                                "The bounded result did not include command completion",
                            ));
                        }
                        break;
                    }
                }
                tokio_postgres::SimpleQueryMessage::CommandComplete(count) => {
                    affected = Some(count)
                }
                tokio_postgres::SimpleQueryMessage::RowDescription(columns) => {
                    if columns.len() != prepared.columns().len()
                        || !columns
                            .iter()
                            .zip(prepared.columns())
                            .all(|(actual, expected)| actual.name() == expected.name())
                    {
                        return Err(failed("The result schema changed during execution"));
                    }
                    described = true;
                }
                _ => {}
            }
        }
        Ok(output.finish(identity, affected))
    }
}

fn postgres_cell(value: Option<&str>, binary: bool) -> Result<serde_json::Value> {
    if !binary || value.is_none() {
        return text_cell(value);
    }
    let hex = value
        .unwrap()
        .strip_prefix("\\x")
        .ok_or_else(|| failed("Unexpected bytea encoding"))?;
    if hex.len() > crate::mcp::MAX_CELL_BYTES {
        return Err(error("limit_exceeded", "A result cell exceeds 64 KiB."));
    }
    if !hex.len().is_multiple_of(2) || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(failed("Invalid bytea encoding"));
    }
    Ok(json!({"binary_hex":hex}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn wire_reader_refuses_more_than_its_budget() {
        let (mut producer, consumer) = tokio::io::duplex(128);
        producer.write_all(&[42; 64]).await.unwrap();
        let mut bounded = BoundedStream {
            inner: consumer,
            remaining: 16,
        };
        let mut bytes = Vec::new();
        assert!(bounded.read_to_end(&mut bytes).await.is_err());
        assert_eq!(bytes, vec![42; 16]);
    }

    #[tokio::test]
    async fn physical_table_proof_blocks_inheritance_and_concurrent_indexes() {
        if tokio::net::TcpStream::connect(("127.0.0.1", 55432))
            .await
            .is_err()
        {
            eprintln!("skipping loopback PostgreSQL schema-lock fixture");
            return;
        }
        let mut source = DataSource::new(DriverKind::Postgres);
        source.host = "127.0.0.1".into();
        source.port = 55432;
        source.database = "dbeast".into();
        source.user = "dbeast".into();
        source.password = "dbeast".into();
        let (setup, connection) = tokio_postgres::connect(
            "host=127.0.0.1 port=55432 dbname=dbeast user=dbeast password=dbeast",
            tokio_postgres::NoTls,
        )
        .await
        .unwrap();
        let task = tokio::spawn(connection);
        let schema = format!("mcp_lock_{}", uuid::Uuid::new_v4().simple());
        setup.batch_execute(&format!("CREATE SCHEMA {schema}; CREATE TABLE {schema}.parent(id integer); CREATE TABLE {schema}.child(id integer); SET lock_timeout='200ms'")).await.unwrap();
        let strict = PgConn::open_mcp(
            &source,
            &crate::drivers::Endpoint {
                host: source.host.clone(),
                port: source.port,
                tunneled: false,
            },
            true,
            None,
        )
        .await
        .unwrap();
        strict
            .mcp_table(
                &TableRef {
                    schema: schema.clone(),
                    name: "parent".into(),
                },
                None,
            )
            .await
            .unwrap();
        for statement in [
            format!("ALTER TABLE {schema}.child INHERIT {schema}.parent"),
            format!("CREATE INDEX CONCURRENTLY probe ON {schema}.parent((id+1)) WHERE id>0"),
        ] {
            let failure = setup.batch_execute(&statement).await.unwrap_err();
            assert_eq!(
                failure.code(),
                Some(&tokio_postgres::error::SqlState::LOCK_NOT_AVAILABLE)
            );
        }
        // Ordinary application writes remain compatible with the proof lock.
        setup
            .batch_execute(&format!("INSERT INTO {schema}.parent VALUES(1)"))
            .await
            .unwrap();
        drop(strict);
        setup.batch_execute(&format!("SET lock_timeout='2s'; ALTER TABLE {schema}.child INHERIT {schema}.parent; DROP SCHEMA {schema} CASCADE")).await.unwrap();
        task.abort();
    }

    #[tokio::test]
    async fn django_style_tables_pass_and_user_code_still_fails_closed() {
        if tokio::net::TcpStream::connect(("127.0.0.1", 55432))
            .await
            .is_err()
        {
            eprintln!("skipping loopback PostgreSQL table proof fixture");
            return;
        }
        let mut source = DataSource::new(DriverKind::Postgres);
        source.host = "127.0.0.1".into();
        source.port = 55432;
        source.database = "dbeast".into();
        source.user = "dbeast".into();
        source.password = "dbeast".into();
        let (setup, connection) = tokio_postgres::connect(
            "host=127.0.0.1 port=55432 dbname=dbeast user=dbeast password=dbeast",
            tokio_postgres::NoTls,
        )
        .await
        .unwrap();
        let task = tokio::spawn(connection);
        let schema = format!("mcp_proof_{}", uuid::Uuid::new_v4().simple());
        // What Django typically creates: positive-integer CHECKs, varchar_pattern_ops
        // and upper() indexes, GIN on jsonb, uuid, inet and array columns.
        setup.batch_execute(&format!("
            CREATE SCHEMA {schema};
            CREATE TABLE {schema}.basket(
                id bigserial PRIMARY KEY, code varchar(32) NOT NULL, quantity integer NOT NULL CHECK (quantity >= 0),
                token uuid, meta jsonb, client inet, tags text[], created timestamptz NOT NULL DEFAULT now());
            CREATE INDEX ON {schema}.basket (code varchar_pattern_ops);
            CREATE INDEX ON {schema}.basket (upper(code)) WHERE quantity > 0;
            CREATE INDEX ON {schema}.basket USING gin (meta);
            CREATE FUNCTION {schema}.f(integer) RETURNS integer LANGUAGE sql IMMUTABLE AS 'SELECT $1';
            CREATE TYPE {schema}.mood AS ENUM ('ok');
            CREATE TABLE {schema}.user_check(id integer CHECK ({schema}.f(id) > 0));
            CREATE TABLE {schema}.user_index(id integer);
            CREATE INDEX ON {schema}.user_index (({schema}.f(id)));
            CREATE TABLE {schema}.user_predicate(id integer);
            CREATE INDEX ON {schema}.user_predicate (id) WHERE {schema}.f(id) > 0;
            CREATE TABLE {schema}.user_type(id integer, mood {schema}.mood);
            CREATE TABLE {schema}.generated(id integer, twice integer GENERATED ALWAYS AS (id * 2) STORED);
            CREATE VIEW {schema}.basket_view AS SELECT id, upper(code) AS code, now() AS seen FROM {schema}.basket WHERE quantity > 0;
            CREATE VIEW {schema}.nested_view AS SELECT b.id, count(*) AS n FROM {schema}.basket_view b JOIN {schema}.basket c ON c.id = b.id GROUP BY b.id;
            CREATE MATERIALIZED VIEW {schema}.basket_summary AS SELECT {schema}.f(quantity) AS q FROM {schema}.basket;
            CREATE VIEW {schema}.volatile_view AS SELECT id, random() AS r FROM {schema}.basket;
            CREATE VIEW {schema}.user_function_view AS SELECT {schema}.f(quantity) AS q FROM {schema}.basket;
            CREATE VIEW {schema}.unsafe_base_view AS SELECT id FROM {schema}.user_check;
            CREATE SEQUENCE {schema}.counter;
        ")).await.unwrap();
        let strict = PgConn::open_mcp(
            &source,
            &crate::drivers::Endpoint {
                host: source.host.clone(),
                port: source.port,
                tunneled: false,
            },
            true,
            None,
        )
        .await
        .unwrap();
        let table = |name: &str| TableRef {
            schema: schema.clone(),
            name: name.into(),
        };
        let (_, columns) = strict.mcp_table(&table("basket"), None).await.unwrap();
        assert_eq!(columns.len(), 8);
        // A materialized view is read from storage, so its definition may use
        // any function; a view is expanded and proved down to its tables.
        for name in ["basket_view", "nested_view", "basket_summary"] {
            strict.mcp_table(&table(name), None).await.unwrap();
        }
        for (name, reason) in [
            ("user_check", "CHECK constraint"),
            ("user_index", "index"),
            ("user_predicate", "index"),
            ("user_type", "enum"),
            ("generated", "generated columns"),
            ("volatile_view", "volatile function"),
            ("user_function_view", "volatile function"),
            ("unsafe_base_view", "CHECK constraint"),
            ("counter", "sequences"),
            ("missing", "does not exist"),
        ] {
            let failure = strict.mcp_table(&table(name), None).await.unwrap_err();
            assert_eq!(failure.code, "unsupported_safe_read", "{name}");
            assert!(failure.message.contains(reason), "{name}: {}", failure.message);
        }
        drop(strict);
        setup
            .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .await
            .unwrap();
        task.abort();
    }

    #[tokio::test]
    async fn qualified_names_never_call_whole_row_functions() {
        if tokio::net::TcpStream::connect(("127.0.0.1", 55432))
            .await
            .is_err()
        {
            eprintln!("skipping loopback PostgreSQL attribute notation fixture");
            return;
        }
        let mut source = DataSource::new(DriverKind::Postgres);
        source.host = "127.0.0.1".into();
        source.port = 55432;
        source.database = "dbeast".into();
        source.user = "dbeast".into();
        source.password = "dbeast".into();
        let (setup, connection) = tokio_postgres::connect(
            "host=127.0.0.1 port=55432 dbname=dbeast user=dbeast password=dbeast",
            tokio_postgres::NoTls,
        )
        .await
        .unwrap();
        let task = tokio::spawn(connection);
        let schema = format!("mcp_attr_{}", uuid::Uuid::new_v4().simple());
        setup
            .batch_execute(&format!("CREATE SCHEMA {schema}; CREATE TABLE {schema}.items(id integer, count integer, secret text); INSERT INTO {schema}.items VALUES (1, 7, 's')"))
            .await
            .unwrap();
        // The static policy list must cover every whole-row routine of this server.
        for row in setup.query(ROW_ROUTINES_SQL, &[]).await.unwrap() {
            let name: String = row.get(0);
            assert!(
                crate::mcp::executor::sql_policy::PG_ROW_ROUTINES.contains(&name.as_str()),
                "{name}"
            );
        }
        let read = |sql: String| {
            let source = source.clone();
            async move {
                crate::mcp::executor::execute(crate::mcp::ExecutionRequest {
                    operation: Operation::ReadSql {
                        source_id: source.id.clone(),
                        sql,
                    },
                    source,
                    expected_table_identity: None,
                    cancellation: Default::default(),
                    execution_lease: None,
                })
                .await
            }
        };
        let output = read(format!("SELECT t.count, t.id FROM {schema}.items t"))
            .await
            .unwrap_or_else(|failure| panic!("{}", failure.message));
        assert_eq!(output.value["rows"], json!([["7", "1"]]));
        for sql in [
            format!("SELECT t.row_to_json FROM {schema}.items t"),
            format!("SELECT {schema}.items.to_jsonb FROM {schema}.items"),
            format!("SELECT q.pg_typeof FROM (SELECT id FROM {schema}.items) q"),
        ] {
            let Err(failure) = read(sql.clone()).await else {
                panic!("{sql} must be refused");
            };
            assert_eq!(failure.code, "unsupported_safe_read", "{sql}");
        }
        setup
            .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .await
            .unwrap();
        task.abort();
    }

    #[tokio::test]
    async fn prepared_results_reject_schema_drift_without_repeating_writes() {
        if tokio::net::TcpStream::connect(("127.0.0.1", 55432))
            .await
            .is_err()
        {
            eprintln!("skipping loopback PostgreSQL prepared-plan fixture");
            return;
        }
        let mut source = DataSource::new(DriverKind::Postgres);
        source.host = "127.0.0.1".into();
        source.port = 55432;
        source.database = "dbeast".into();
        source.user = "dbeast".into();
        source.password = "dbeast".into();
        let (setup, connection) = tokio_postgres::connect(
            "host=127.0.0.1 port=55432 dbname=dbeast user=dbeast password=dbeast",
            tokio_postgres::NoTls,
        )
        .await
        .unwrap();
        let task = tokio::spawn(connection);
        let schema = format!("mcp_plan_{}", uuid::Uuid::new_v4().simple());
        setup
            .batch_execute(&format!("CREATE SCHEMA {schema}"))
            .await
            .unwrap();
        let mcp = PgConn::open_mcp(
            &source,
            &crate::drivers::Endpoint {
                host: source.host.clone(),
                port: source.port,
                tunneled: false,
            },
            false,
            None,
        )
        .await
        .unwrap();

        // Full access still executes DDL and DML using the original native plan.
        for sql in [
            format!("CREATE TABLE {schema}.items(id integer)"),
            format!("ALTER TABLE {schema}.items ADD COLUMN payload bytea"),
            format!("INSERT INTO {schema}.items VALUES(1,decode('0001ff','hex'))"),
        ] {
            let prepared = mcp.client.prepare(&sql).await.unwrap();
            mcp.mcp_execute_prepared(prepared, &sql, None, true)
                .await
                .unwrap();
        }
        let sql = format!(
            "INSERT INTO {schema}.items VALUES(2,decode('abcd','hex')) RETURNING id,payload"
        );
        let prepared = mcp.client.prepare(&sql).await.unwrap();
        let output = mcp
            .mcp_execute_prepared(prepared, &sql, None, true)
            .await
            .unwrap();
        assert_eq!(output.value["rows"], json!([["2", {"binary_hex":"abcd"}]]));
        for sql in ["SHOW server_version", "EXPLAIN SELECT 1", "VALUES (1)"] {
            let prepared = mcp.client.prepare(sql).await.unwrap();
            let output = mcp
                .mcp_execute_prepared(prepared, sql, None, true)
                .await
                .unwrap();
            assert!(!output.value["rows"].as_array().unwrap().is_empty());
        }

        // A change in width, names, or types must never return a row decoded
        // using an earlier descriptor, even when the new width is smaller.
        for (name, change) in [
            ("added", "ADD COLUMN extra text"),
            ("removed", "DROP COLUMN payload"),
            ("renamed", "RENAME COLUMN id TO renamed_id"),
            (
                "retyped",
                "ALTER COLUMN payload TYPE text USING payload::text",
            ),
        ] {
            setup
                .batch_execute(&format!(
                    "CREATE TABLE {schema}.{name}(id integer,payload bytea); INSERT INTO {schema}.{name} VALUES(1,decode('00','hex'))"
                ))
                .await
                .unwrap();
            let sql = format!("SELECT * FROM {schema}.{name}");
            let prepared = mcp.client.prepare(&sql).await.unwrap();
            setup
                .batch_execute(&format!("ALTER TABLE {schema}.{name} {change}"))
                .await
                .unwrap();
            assert!(
                mcp.mcp_execute_prepared(prepared, &sql, None, true)
                    .await
                    .is_err(),
                "schema change {name} must be rejected"
            );
        }

        let sql = format!("INSERT INTO {schema}.items(id) VALUES(3) RETURNING *");
        let prepared = mcp.client.prepare(&sql).await.unwrap();
        setup
            .batch_execute(&format!("ALTER TABLE {schema}.items ADD COLUMN extra text"))
            .await
            .unwrap();
        assert!(
            mcp.mcp_execute_prepared(prepared, &sql, None, true)
                .await
                .is_err()
        );
        let count: i64 = setup
            .query_one(&format!("SELECT count(*) FROM {schema}.items"), &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(
            count, 2,
            "a rejected RETURNING plan must not insert or retry"
        );
        drop(mcp);
        setup
            .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .await
            .unwrap();
        task.abort();
    }
}
