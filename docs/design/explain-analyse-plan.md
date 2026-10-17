# Explain Analyse: plan and resume point

This document describes the scope and decisions of the Explain Analyse increment.

## Status

The increment was completed on October 15, 2026 for PostgreSQL; measurements and the user-facing description are in the README. MySQL was added in the next increment. The sections below remain as a record of the decisions for the initial PostgreSQL scope.

## Agreed scope

- The first increment covers PostgreSQL, a single selected statement or the statement under the cursor, on the same session and in the console context.
- A separate **Explain Analyse** action really executes the SQL, including DML, CTEs and functions. It respects Auto/Manual, with no automatic rollback after success. The action description must say this clearly. We do not add a new confirmation dialog.
- Plain Explain Plan keeps its existing behavior: planning without ANALYZE and without BEGIN just because of Manual mode.
- Generator: `EXPLAIN (ANALYZE TRUE, FORMAT JSON)`. Single statement validation and prepared execution; no arbitrary user EXPLAIN options and no automatic retries.
- MySQL and SQLite have the action disabled at this stage, with a reason. MySQL needs a separate TREE parser. Parameterized Related Rows consoles stay outside this increment.
- The existing Explain Plan button still runs a plain plan. The adjacent **Explain Actions** menu contains both actions and does not remember Analyse as the default command. Both actions are also in the editor menu, the overflow menu and Find Action. No new global shortcut.

## Work order

1. Finish the backend, typed PostgreSQL errors and cancellation bound to a specific request.
2. Add the mode model, the parser for actual metrics and the plan summary.
3. Integrate the controller, events, context protection and invalidation of metadata of earlier results.
4. Add the actions and the presentation of statistics in Operations Tree, Details, Raw and the diagram.
5. Finish unit, database and UI tests; run the full regression, Clippy and a release build.
6. Measure the parser, view preparation, CPU frames and local execution. Review screenshots of the normal and narrow window.
7. Update the README and the iteration report, commit locally on master. No pushing.

## Agreed types and integration points

[`src/query_plan.rs`](https://github.com/macolszewski/dbeast_manager/blob/master/src/query_plan.rs):

- `ExplainMode::{Plan, Analyse}`, Plan by default; `label()` returns the action name.
- Keep `explain_sql(driver, statement)` and `parse(driver, sql, data)` as the Plan variants. Add `explain_sql_mode(driver, statement, mode)` and `parse_mode(driver, sql, data, mode)`.
- `QueryPlan.mode: ExplainMode` and `analysis: Option<Box<PlanAnalysis>>`. A plain plan does not get extra text fields in every node.
- `PlanAnalysis { nodes: Vec<ActualMetrics>, summary: PlanSummary }`. The number of metric entries must match `QueryPlan.nodes`, also with an empty Raw fallback.
- `ActualMetrics`: `startup_ms`, `total_ms`, `rows`, `loops` as `Option<String>`, and `never_executed: bool`.
- `PlanSummary`: `planning_time_ms`, `execution_time_ms` as `Option<String>`. A summary only for an unambiguous single source document.

Metrics are built once from the JSON. A missing value is distinct from zero. Do not sum node times or compute percentages; actual values depend on loops and include the work of child nodes. An unknown format keeps the full Raw output and Analyse mode, without re-execution. The diagram geometry is still computed in the background, once per plan.

[`src/backend/explain.rs`](https://github.com/macolszewski/dbeast_manager/blob/master/src/backend/explain.rs) and [`src/backend.rs`](https://github.com/macolszewski/dbeast_manager/blob/master/src/backend.rs):

- `ExplainRequest` gets `mode` and `manual_tx`.
- `ExplainExecution::{NotStarted, Succeeded, Failed, Unknown}` describes the execution of the analyzed SQL, independently of later parsing and session cleanup. For Plan: NotStarted.
- `ExplainCompletion { ds_id: String, execution: ExplainExecution, in_tx: Option<bool>, session_closed: bool, post_context: Option<ResolvedContext> }`.
- `Event::ExplainDone`: `console_id`, `request_id`, `completion`, `result`. Do not silence Clippy when the enum grows; use Box if needed.
- `in_tx=None` means the state was not observed, for example cancellation before the lock was acquired. `ds_id` makes it possible to invalidate metadata also after the tab is closed or a stale response arrives, without carrying secrets.

[`src/ui/explain.rs`](https://github.com/macolszewski/dbeast_manager/blob/master/src/ui/explain.rs), [`src/ui/mod.rs`](https://github.com/macolszewski/dbeast_manager/blob/master/src/ui/mod.rs), actions and the console:

- A single controller `explain_tab(index, mode)` and `explain_blocked_reason(index, mode)`.
- `RunKind::Explain` keeps the mode, the request identifier and the context revision. Toolbar and editor intent: `Option<ExplainMode>`.
- Keep SQL selection with source Unicode positions and folds, the locks on editing, saving and concurrent execution, and the rejection of late responses.

## Transactions, cancellation and uncertain outcomes

Auto without a transaction uses server autocommit. Manual without a transaction opens BEGIN, then a SAVEPOINT. Inside an existing transaction it uses its own SAVEPOINT. Success releases the SAVEPOINT; a confirmed error or cancellation runs ROLLBACK TO and RELEASE. Never commit or roll back the user's entire earlier work. Respect read-only mode.

Register the cancellation token and notification before waiting for the session mutex. Check the token after acquiring the lock, after preparing the context and after prepare, before Execute. Only the owning worker sends cancel; do not leave a separate cancellation task that could hit the next SQL statement. Disconnect, closing the session and closing the application also cancel pending requests. A duplicate does not replace the active controller.

PostgreSQL does not confirm cancellation merely by finishing sending the CancelRequest packet. The result of the original operation has to be read. When the transport result is uncertain, cleanup fails, or a sent cancel may arrive after the SQL has finished, the chosen approach is to close and lock the session until an explicit **Disconnect and reconnect of the source**. Refresh alone does not remove this lock. Keep the known execution result; never claim an apparent rollback or a cancellation of a success. In Auto the effects may have been committed; closing the session may lose an earlier open Manual transaction. The message and the README must describe this trade-off.

Recovery limit after Stop: 5 s for the cancellation/cleanup transport, without imposing a limit on regular SQL execution. Never automatically retry a statement after an unknown outcome. Distinguish typed server and transport errors; do not parse error text.

## Context and earlier results

`QueryPlan.context` describes the context before the SQL. Analyse may change search_path or the catalog through a function; the current UI state comes only from the new `post_context`. Without a confirmed read, the current context is cleared instead of restoring the old snapshot.

After every possible start of Analyse, invalidate the edit, relation and prepared UPDATE caches in the backend, and the result permissions/metadata and completion data for the relevant source in the UI. This also applies to SELECT/WITH, errors and stale responses. Keep data as snapshots, without copying large tables or re-executing. Do not restore write permission through delayed metadata responses. Clear the caches of other sessions after releasing the own lock.

## Presentation and verification criteria

The header distinguishes Estimated plan and Actual execution. Client elapsed time is separate from the server's Planning Time and Execution Time. For Analyse, Tree shows Operation, Actual Rows, Loops and Actual Time (ms); additional columns only when there is enough width. Estimates stay in Details and Raw. The diagram keeps the 260×100 card size, and Attributes shows actual Rows/Loops/Time/Startup. Missing values: N/A, without misleading units. Zero loops: Not executed. Remove duplicates of promoted properties once, during view preparation.

Evidence required before closing the increment:

- Tests of cancellation phases: before the lock, between prepare and Execute, during SQL and after success; no late cancel and no automatic reconnect/retry.
- PostgreSQL: Auto UPDATE exactly once; Manual Commit/Rollback; earlier work preserved on error and on confirmed cancel; read-only; CTEs and functions with side effects; the same session, temp objects and schema; fresh context after `set_config`.
- The existing Explain Plan still does not execute DML.
- Parser: missing metrics, zeros, multiple loops, exponential notation, unknown fields and Raw fallback; no re-execution after a normalization error.
- UI: action availability, full SQL and cursor, preserved earlier results, statistics in three views, copying, rejection of stale events, the 1100×500 window. Tests with their own `DBEAST_CONFIG`.
- `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo build --release`; test databases and GPU actually available. Run Cargo sequentially, without parallel test processes that modify shared fixtures.
- Release benchmarks with data prepared outside the measurement; separately for parser/layout, UI cache, CPU frames and database time. Screenshots reviewed, README updated, local commit.

References: [PostgreSQL EXPLAIN](https://www.postgresql.org/docs/17/sql-explain.html), [PostgreSQL request cancellation](https://www.postgresql.org/docs/17/protocol-flow.html#PROTOCOL-FLOW-CANCELING-REQUESTS), [MySQL EXPLAIN](https://dev.mysql.com/doc/refman/8.4/en/explain.html).
