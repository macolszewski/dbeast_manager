//! The MCP executor owns a fresh connection for each approved operation.

use super::{Connection, Endpoint, Kind, NEXT_CONNECTION_EPOCH, mysql, postgres, sqlite, tunnel};
use crate::mcp::executor::{Result, failed};
use crate::mcp::{ExecutionOutput, Operation};
use crate::model::{DataSource, DriverKind};
use tokio_util::sync::CancellationToken;

impl Connection {
    pub(crate) async fn close_mcp(self) {
        let Self { kind, _tunnel, .. } = self;
        if let Kind::MySql(connection) = kind {
            connection.close_mcp().await;
        }
    }

    pub(crate) async fn open_mcp(
        ds: &DataSource,
        strict: bool,
        execution_lease: Option<std::sync::Arc<dyn Send + Sync>>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !ds.credentials_unavailable,
            "Saved credentials are unavailable"
        );
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let tunnel = if ds.uses_ssh() {
                Some(tunnel::open(ds).await?)
            } else {
                None
            };
            let endpoint = match &tunnel {
                Some(tunnel) => Endpoint {
                    host: "127.0.0.1".into(),
                    port: tunnel.local_port,
                    tunneled: true,
                },
                None => Endpoint {
                    host: ds.host.clone(),
                    port: ds.port,
                    tunneled: false,
                },
            };
            let kind = match ds.driver {
                DriverKind::Postgres => Kind::Postgres(Box::new(
                    postgres::PgConn::open_mcp(ds, &endpoint, strict, execution_lease).await?,
                )),
                DriverKind::MySql => Kind::MySql(
                    mysql::MyConn::open_mcp(ds, &endpoint, strict, execution_lease).await?,
                ),
                DriverKind::Sqlite => {
                    Kind::Sqlite(sqlite::LiteConn::open_mcp(ds, strict, execution_lease).await?)
                }
            };
            Ok(Self {
                kind,
                epoch: NEXT_CONNECTION_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                in_tx: false,
                _tunnel: tunnel,
            })
        })
        .await
        .map_err(|_| anyhow::anyhow!("MCP connection setup timed out"))?
    }

    pub(crate) async fn execute_mcp(
        &mut self,
        operation: &Operation,
        sql: &str,
        expected: Option<&str>,
        cancellation: CancellationToken,
        read_plan: Option<&crate::mcp::executor::sql_policy::ReadPlan>,
    ) -> Result<ExecutionOutput> {
        let result = match &mut self.kind {
            Kind::Postgres(connection) => {
                connection
                    .execute_mcp(operation, sql, expected, read_plan)
                    .await
            }
            Kind::MySql(connection) => {
                connection
                    .execute_mcp(operation, sql, expected, read_plan)
                    .await
            }
            Kind::Sqlite(connection) => {
                connection
                    .execute_mcp(
                        operation.clone(),
                        sql.to_owned(),
                        expected.map(str::to_owned),
                        cancellation,
                        read_plan.map(|plan| plan.tables.clone()),
                    )
                    .await
            }
        };
        result.map_err(|mut error| {
            // A failed command can already have effects through routines, an
            // implicit commit or transport loss. Never promise rollback/retry.
            if operation.is_full_sql() {
                error.code = "outcome_unknown";
                error.message = "The SQL did not complete normally. Its effects may have committed; do not retry automatically.";
                error.outcome_unknown = true;
            }
            error
        })
    }
}

pub(crate) fn list_preview(source: &DataSource) -> String {
    match source.driver {
        DriverKind::Postgres => postgres::mcp::LIST_SQL.to_owned(),
        DriverKind::MySql => format!(
            "{}\n-- Parameter: the shared connection's current database",
            mysql::mcp::LIST_SQL
        ),
        DriverKind::Sqlite => sqlite::mcp::LIST_SQL.to_owned(),
    }
}

pub(crate) fn json_output(
    value: serde_json::Value,
    identity: Option<String>,
) -> Result<ExecutionOutput> {
    if serde_json::to_vec(&value).map_err(failed)?.len() > crate::mcp::MAX_RESULT_BYTES - 1024 {
        return Err(crate::mcp::executor::error(
            "limit_exceeded",
            "The metadata result exceeds the response limit.",
        ));
    }
    Ok(ExecutionOutput {
        value,
        table_identity: identity,
    })
}
