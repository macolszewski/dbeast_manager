//! Disposable, bounded database execution behind the approval broker.
//!
//! Read SQL passes a conservative AST allowlist. The per-driver verifier locks
//! and validates every physical table before executing the SQL shown in the UI.

use super::{
    ExecutionError, ExecutionOutput, ExecutionRequest, MAX_CELL_BYTES, MAX_RESULT_BYTES, MAX_ROWS,
    MAX_SQL_BYTES, Operation,
};
use crate::drivers::Connection;
use crate::model::{DataSource, DriverKind};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
#[path = "sql_policy.rs"]
pub(crate) mod sql_policy;

pub fn read_sql_tables(source: &DataSource, sql: &str) -> Result<Vec<super::TableRef>> {
    Ok(sql_policy::analyze(source, sql)?.tables)
}

/// Target table of a write eligible for a standing table write share, and the
/// SQL regenerated from the checked AST that such a write executes.
pub fn table_write_target(source: &DataSource, sql: &str) -> Result<(super::TableRef, String)> {
    sql_policy::table_write_target(source, sql)
}

pub(crate) type Result<T> = std::result::Result<T, ExecutionError>;
pub(crate) const MAX_COLUMNS: usize = 64;
pub(crate) const MAX_WIRE_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const EXECUTION_TIMEOUT: Duration = Duration::from_secs(30);

impl std::fmt::Display for ExecutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message)
    }
}
impl std::error::Error for ExecutionError {}

pub(crate) fn error(code: &'static str, message: &'static str) -> ExecutionError {
    ExecutionError {
        code,
        message,
        outcome_unknown: false,
    }
}

pub(crate) fn unsupported() -> ExecutionError {
    error(
        "unsupported_safe_read",
        "Strict read requires a supported physical table and built-in scalar columns. Views, generated columns and executable schema objects are not supported.",
    )
}

pub(crate) fn failed(_: impl std::fmt::Display) -> ExecutionError {
    // Driver diagnostics may contain connection addresses, paths, credentials or
    // database values. They must never cross the MCP boundary.
    error(
        "execution_failed",
        "The database operation failed. Review the connection in DBeast.",
    )
}

pub(crate) fn identity_matches(actual: &str, expected: Option<&str>) -> Result<()> {
    if expected.is_some_and(|expected| expected != actual) {
        Err(error(
            "resource_unavailable",
            "The table changed. Request a new approval in DBeast.",
        ))
    } else {
        Ok(())
    }
}

pub(crate) fn identifier(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        Err(error(
            "invalid_input",
            "Identifiers must contain 1 to 256 bytes and no control characters.",
        ))
    } else {
        Ok(())
    }
}

pub(crate) fn dialect_identifier(value: &str, driver: DriverKind) -> Result<()> {
    identifier(value)?;
    // PostgreSQL truncates names in UTF-8 bytes even when quoted. A longer
    // alias would invalidate scope and whole-row type checks done by the AST.
    if driver == DriverKind::Postgres && value.len() > 63 {
        return Err(error(
            "invalid_input",
            "PostgreSQL identifiers must contain at most 63 UTF-8 bytes.",
        ));
    }
    Ok(())
}

pub(crate) fn pinned_table_error(
    mut failure: ExecutionError,
    expected: Option<&str>,
) -> ExecutionError {
    if expected.is_some() {
        failure.code = "resource_unavailable";
        failure.message =
            "The table identity or safety could not be verified. Request a new approval in DBeast.";
    }
    failure
}

pub(crate) fn quote(value: &str, driver: DriverKind) -> String {
    let delimiter = if driver == DriverKind::MySql {
        '`'
    } else {
        '"'
    };
    format!(
        "{delimiter}{}{delimiter}",
        value.replace(delimiter, &format!("{delimiter}{delimiter}"))
    )
}

pub(crate) fn qualified(table: &super::TableRef, driver: DriverKind) -> String {
    format!(
        "{}.{}",
        quote(&table.schema, driver),
        quote(&table.name, driver)
    )
}

/// Pure validation and generation: the broker captures this before approval.
pub fn preview(source: &DataSource, operation: &Operation) -> Result<String> {
    // The broker binds the opaque public resource ID to this captured private
    // source. DataSource.id can contain a local path and is never an MCP ID.
    if let Some(table) = operation.table() {
        dialect_identifier(&table.schema, source.driver)?;
        dialect_identifier(&table.name, source.driver)?;
        let system = match source.driver {
            DriverKind::Postgres => {
                table.schema.starts_with("pg_") || table.schema == "information_schema"
            }
            DriverKind::MySql => ["mysql", "information_schema", "performance_schema", "sys"]
                .iter()
                .any(|name| table.schema.eq_ignore_ascii_case(name)),
            DriverKind::Sqlite => {
                table.schema != "main" || table.name.to_ascii_lowercase().starts_with("sqlite_")
            }
        };
        if system {
            return Err(unsupported());
        }
        if source.driver == DriverKind::MySql && table.schema != source.database {
            return Err(unsupported());
        }
    }
    match operation {
        Operation::ListTables { .. } => Ok(crate::drivers::mcp::list_preview(source)),
        Operation::DescribeTable { table, .. } => Ok(format!(
            "Inspect system metadata for {}.\nNo row contents or stored expressions are evaluated.\nThe table is locked and its physical identity is verified in a read-only transaction.",
            qualified(table, source.driver)
        )),
        Operation::ReadTable {
            table,
            columns,
            limit,
            offset,
            ..
        } => {
            if columns.is_empty()
                || columns.len() > MAX_COLUMNS
                || *limit == 0
                || *limit > MAX_ROWS
                || *offset > 1_000_000
            {
                return Err(error(
                    "invalid_input",
                    "Select 1 to 64 explicit columns, 1 to 500 rows and an offset no larger than 1000000.",
                ));
            }
            for column in columns {
                dialect_identifier(column, source.driver)?;
            }
            let unique: std::collections::HashSet<_> = columns.iter().collect();
            if unique.len() != columns.len() {
                return Err(error(
                    "invalid_input",
                    "Duplicate columns are not supported.",
                ));
            }
            let columns = columns
                .iter()
                .map(|column| quote(column, source.driver))
                .collect::<Vec<_>>()
                .join(", ");
            let only = if source.driver == DriverKind::Postgres {
                "ONLY "
            } else {
                ""
            };
            // Numeric options are bounded Rust integers, never SQL fragments.
            Ok(format!(
                "SELECT {columns} FROM {only}{} LIMIT {limit} OFFSET {offset}",
                qualified(table, source.driver)
            ))
        }
        Operation::Sql { sql, .. } => {
            if source.read_only {
                return Err(error(
                    "permission_denied",
                    "Full SQL is disabled for this read-only source.",
                ));
            }
            if sql.is_empty()
                || sql.len() > MAX_SQL_BYTES
                || sql.contains('\0')
                || crate::splitter::split(sql, source.driver).len() != 1
            {
                return Err(error(
                    "invalid_input",
                    "Full SQL requires exactly one statement of at most 64 KiB.",
                ));
            }
            // Preserve the exact submitted SQL. Native prepared statements also
            // reject stacked commands independently of the presentation splitter.
            Ok(sql.clone())
        }
        Operation::ReadSql { sql, .. } => Ok(sql_policy::analyze(source, sql)?.sql),
        Operation::ReadFile { .. } => Err(error(
            "invalid_input",
            "Document reads are handled by the approval broker.",
        )),
    }
}

pub async fn execute(request: ExecutionRequest) -> Result<ExecutionOutput> {
    let started = Instant::now();
    let sql = preview(&request.source, &request.operation)?;
    let read_plan = if let Operation::ReadSql { sql, .. } = &request.operation {
        Some(sql_policy::analyze(&request.source, sql)?)
    } else {
        None
    };
    let full = request.operation.is_full_sql();
    let mut connection = tokio::select! {
        biased;
        _ = request.cancellation.cancelled() => return Err(error("cancelled", "The request was cancelled.")),
        result = Connection::open_mcp(&request.source, !full, request.execution_lease.clone()) => result.map_err(failed)?,
    };
    let cancel = connection.cancel_handle();
    let remaining = EXECUTION_TIMEOUT.saturating_sub(started.elapsed());
    let completed = {
        let execution = connection.execute_mcp(
            &request.operation,
            &sql,
            request.expected_table_identity.as_deref(),
            request.cancellation.clone(),
            read_plan.as_ref(),
        );
        tokio::pin!(execution);
        tokio::select! {
            biased;
            _ = request.cancellation.cancelled() => None,
            _ = tokio::time::sleep(remaining) => None,
            result = &mut execution => Some(result),
        }
    };
    let mut result = if let Some(result) = completed {
        connection.close_mcp().await;
        result?
    } else {
        if let Some(cancel) = cancel {
            let _ = tokio::time::timeout(Duration::from_secs(1), cancel.cancel()).await;
        }
        // Disposable connection drop closes the socket/driver task. No retry,
        // no shared transaction and no return to a connection pool.
        connection.close_mcp().await;
        return Err(ExecutionError {
            code: if full { "outcome_unknown" } else { "cancelled" },
            message: if full {
                "The SQL was interrupted. Its effects may have committed; do not retry automatically."
            } else {
                "The request was cancelled or exceeded its time limit."
            },
            outcome_unknown: full,
        });
    };
    // Cancellation is an authorization boundary even when it races completion.
    if request.cancellation.is_cancelled() {
        if full {
            return Err(ExecutionError {
                code: "outcome_unknown",
                message: "Access was revoked before the SQL result could be delivered. Its effects may have committed; do not retry automatically.",
                outcome_unknown: true,
            });
        }
        return Err(error("cancelled", "The request was cancelled."));
    }
    if let Some(object) = result.value.as_object_mut() {
        object.insert("elapsed_ms".into(), json!(started.elapsed().as_millis()));
    }
    Ok(result)
}

/// Response accounting happens while rows stream, before copying a cell. The
/// wire/SQLite limits additionally bound allocations inside database libraries.
pub(crate) struct Rows {
    columns: Vec<String>,
    rows: Vec<Vec<Value>>,
    bytes: usize,
    truncated: bool,
}

impl Rows {
    pub(crate) fn new(columns: Vec<String>) -> Result<Self> {
        if columns.len() > MAX_COLUMNS || columns.iter().any(|column| column.len() > MAX_CELL_BYTES)
        {
            return Err(error(
                "limit_exceeded",
                "The result has too many columns or an oversized column name.",
            ));
        }
        let bytes = serde_json::to_vec(&columns).map_err(failed)?.len() + 256;
        Ok(Self {
            columns,
            rows: Vec::new(),
            bytes,
            truncated: false,
        })
    }

    pub(crate) fn push(&mut self, row: Vec<Value>) -> Result<bool> {
        let size = serde_json::to_vec(&row).map_err(failed)?.len() + 1;
        if self.rows.len() == MAX_ROWS as usize || self.bytes + size > MAX_RESULT_BYTES - 1024 {
            self.truncated = true;
            return Ok(false);
        }
        self.bytes += size;
        self.rows.push(row);
        Ok(true)
    }

    pub(crate) fn finish(self, identity: Option<String>, affected: Option<u64>) -> ExecutionOutput {
        ExecutionOutput {
            value: json!({"columns": self.columns, "rows": self.rows, "truncated": self.truncated, "affected_rows": affected,
            "value_encoding": "Non-null scalar values are exact text. Binary values use {\"binary_hex\":\"...\"}."}),
            table_identity: identity,
        }
    }
}

pub(crate) fn text_cell(value: Option<&str>) -> Result<Value> {
    match value {
        None => Ok(Value::Null),
        Some(value) if value.len() <= MAX_CELL_BYTES => Ok(Value::String(value.to_owned())),
        _ => Err(error(
            "limit_exceeded",
            "A result cell exceeds the 64 KiB limit.",
        )),
    }
}

pub(crate) fn binary_cell(value: &[u8]) -> Result<Value> {
    if value.len() > MAX_CELL_BYTES / 2 {
        return Err(error(
            "limit_exceeded",
            "A binary result cell exceeds the 64 KiB encoded limit.",
        ));
    }
    use std::fmt::Write;
    let mut hex = String::with_capacity(value.len() * 2);
    for byte in value {
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(json!({"binary_hex": hex}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::TableRef;

    #[test]
    fn structured_read_quotes_identifiers_and_rejects_sql_fragments_as_expressions() {
        let source = DataSource::new(DriverKind::Postgres);
        let operation = Operation::ReadTable {
            source_id: source.id.clone(),
            table: TableRef {
                schema: "public".into(),
                name: "items\"; DROP TABLE users;--".into(),
            },
            columns: vec!["dangerous()".into()],
            limit: 1,
            offset: 0,
        };
        assert_eq!(
            preview(&source, &operation).unwrap(),
            "SELECT \"dangerous()\" FROM ONLY \"public\".\"items\"\"; DROP TABLE users;--\" LIMIT 1 OFFSET 0"
        );
    }

    #[test]
    fn full_sql_rejects_stacked_statements_and_read_only_sources() {
        let mut source = DataSource::new(DriverKind::Postgres);
        for sql in ["SELECT 1; SELECT 2", "", "SELECT 1\0"] {
            assert!(
                preview(
                    &source,
                    &Operation::Sql {
                        source_id: source.id.clone(),
                        sql: sql.into()
                    }
                )
                .is_err()
            );
        }
        source.read_only = true;
        assert_eq!(
            preview(
                &source,
                &Operation::Sql {
                    source_id: source.id.clone(),
                    sql: "SELECT 1".into()
                }
            )
            .unwrap_err()
            .code,
            "permission_denied"
        );
    }
}
