# MCP server in DBeast

Project status: this increment is implemented and verified locally. The tool contract is in [mcp-contract.md](mcp-contract.md). Results of the checks performed and the limits of the measurements are given at the end of this document.

## Goal and trust boundary

An agent can use resources that the user shares in a running DBeast instance. It does not receive connection configuration, passwords, Keychain contents, file paths or the state of other agents. Every new agent session starts with no permissions. Grants are given only in the DBeast window and disappear when the server stops.

The first version listens only on `127.0.0.1`. The user chooses the port and starts the server manually. There is no automatic start, no LAN access, no public address, no CORS and no tokens saved to the configuration. The panel creates a separate token for each MCP client. The token sent in the `Authorization` header is independent of the session identifier shown in prompts.

A read-only restriction cannot rely on recognizing the word `SELECT`. Functions, views, RLS policies, operators, extensions and foreign tables can have side effects. Strict read mode allows only operations for which the driver can confirm a safe execution path. An unrecognized case ends in a refusal, not in a fallback to Full SQL. The details of this boundary have to be documented in the driver implementations and proven with local tests.

Full SQL is a separate, clearly labeled permission that grants the full capabilities of the given connection. It does not isolate the database or the server: a statement can use the account's privileges, server functions and access to other databases. The result of such a statement can reveal information about the server. A source marked `read_only` never allows this permission. A grant for a single table covers only strict reads and never authorizes Full SQL.

## Technology choices

As checked on October 15, 2026:

- The official Rust SDK `rmcp` is at release 3.5.1 and requires Rust 1.88 or later. We use the registry release pinned in `Cargo.lock`, not the `main` branch. We need the server features, tool schemas and Streamable HTTP. [rmcp documentation](https://docs.rs/rmcp/3.5.1/rmcp/), [SDK project manifest](https://github.com/modelcontextprotocol/rust-sdk/blob/main/Cargo.toml).
- `StreamableHttpService` is a Tower service that fits an `axum` router and the existing Tokio runtime. The current `axum` version is 0.8.9. A separate process, a second runtime or a custom JSON-RPC implementation are not needed. [Official Rust SDK](https://github.com/modelcontextprotocol/rust-sdk), [axum documentation](https://docs.rs/axum/0.8.9/axum/).
- The transport is a single `/mcp` endpoint over Streamable HTTP. Protocol version 2026-07-28 has no transport sessions. Compatibility with the older 2025-11-25 protocol is handled by the SDK; application permissions are always independent of `Mcp-Session-Id`. [MCP transport](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http).
- User consent comes from the DBeast UI, not from the agent's response to MCP elicitation. The agent gets no tool for approving or granting permissions.
- The DNS rebinding bug in older `rmcp` versions is described in RUSTSEC-2026-0189, currently fixed since 1.4.0. Independently of SDK fixes, the application checks the exact `Host`, rejects any `Origin` header that is present and requires a token on every request. [RustSec](https://rustsec.org/advisories/RUSTSEC-2026-0189.html).

Documentation and examples stay in the repository. The MCP contract is based on tool schemas generated from Rust types and on tests of the real transport. We do not build a parallel REST API or an OpenAPI specification describing endpoints that do not exist. We do not publish configuration or examples in an external workspace.

## Increments

1. Contract and threat model: explicit strict read boundaries, client isolation, resource versioning, limits and the approval procedure.
2. Permission broker: clients, sessions, one-time and session grants, requests, revocation, expiry and a bounded in-memory log. State machine and concurrency tests before SQL is connected.
3. Execution: separate MCP connections, verified read operations, bounded Full SQL, snapshots of open files, cancellation and permission checks before the result is released as well.
4. Transport and panel: start/stop, port, client creation, configuration copying, opt-in resources, sessions/permissions, a prompt with the exact SQL and refusal as the safe choice.
5. Verification: a real MCP client over loopback, negative HTTP attempts, local test databases, UI tests, screenshots, measurements, documentation and a local commit on `master`.

## Implementation layout

| Area | Responsibility |
|---|---|
| [`src/mcp/mod.rs`](https://github.com/macolszewski/dbeast_manager/blob/master/src/mcp/mod.rs) | Shared types without secrets in `Debug`, configuration and control handles |
| [`src/mcp/broker.rs`](https://github.com/macolszewski/dbeast_manager/blob/master/src/mcp/broker.rs) | Atomic state transitions, identities, grants, limits, deadlines, audit |
| [`src/mcp/protocol.rs`](https://github.com/macolszewski/dbeast_manager/blob/master/src/mcp/protocol.rs) | Input/output schemas, tool descriptions for the agent, bounded errors |
| [`src/mcp/server.rs`](https://github.com/macolszewski/dbeast_manager/blob/master/src/mcp/server.rs) | `rmcp`, `axum`, HTTP checks, authentication and the listener lifecycle |
| [`src/mcp/executor.rs`](https://github.com/macolszewski/dbeast_manager/blob/master/src/mcp/executor.rs), [`src/mcp/sql_policy.rs`](https://github.com/macolszewski/dbeast_manager/blob/master/src/mcp/sql_policy.rs), `src/drivers/*/mcp.rs` | Isolated connections, full validation of read SQL and execution of approved requests |
| [`src/ui/mcp.rs`](https://github.com/macolszewski/dbeast_manager/blob/master/src/ui/mcp.rs) | Settings panel, sharing, sessions and the exact approval preview |
| `tests/mcp_*.rs`, `tests/ui_mcp.rs` | Contract, security, local execution and view tests |

The UI sends commands and the backend emits `backend::Event`. Snapshots of the catalog and of open documents change in response to events, not by cloning large files every frame. The broker does not receive the whole serialized `DataSource` as an API response. Secrets and connections stay on the executor side.

## Acceptance criteria

- No port listens before a manual Start. A port-in-use error is visible. Stop immediately invalidates clients, sessions, grants and results.
- Two client configurations with different tokens cannot see each other's sessions, requests or results. A UUID visible on screen cannot be used to exercise another client's grant.
- `begin_session` always issues a new session ID and token and invalidates the client's previous session. Permissions are not restored after an application or server restart.
- The catalog contains no addresses, users, paths, passwords or Keychain references. The default MCP label is generic. A source name of the form `user@host` is not used as an alias.
- Reading data, schema or a file requires a grant. The prompt shows the full session ID, the resource alias, the scope, the operation kind and the immutable SQL or file snapshot. A name supplied by the client is marked as unverified.
- A one-time grant runs the exact approved operation only once. A change of SQL, context, source, document version or scope does not reuse the old grant.
- A repeated identical request does not run the SQL a second time. Different parameters with the same idempotency key are rejected.
- Strict read accepts SQL text only after full validation of the allowed constructs. It runs under the configured source account. Tests cover reads with JOIN, CTEs, subqueries and aggregates, and rejected write attempts through functions, views, comments, nesting, multiple statements, system tables and schema changes after approval.
- Revoking a grant and ending a session cancel work and block the release of a late result. An uncertain write outcome is never presented as a rolled-back write.
- Documents are accessible only by the ID of an already open snapshot. There is no path parameter, no opening of new files and no arbitrary file system reads.
- Limits cover request size, SQL, cell, response, the number of pending requests and clients, execution time and session lifetime. We check that memory does not grow after limits are exhausted and permissions are revoked.
- Tests use `App::new_for_test`, a temporary configuration, synthetic secrets, local servers and database fixtures. The real configuration, Keychain and the user's external database stay out of the tests.
- Screenshots of the panel, the prompt and the session list are reviewed at normal and narrow widths. Verification includes `cargo test`, `cargo clippy --all-targets -- -D warnings`, a release build and measurements of the impact of MCP when disabled and when running.

## Important product limitations

MCP does not tell the server that the user has started a new conversation with the model. The contract requires `begin_session` for a new conversation, and DBeast enforces deadlines, session end and local revocation of permissions. A client that has kept an active session secret can use it until it expires or is revoked. We do not present this as the application recognizing conversations.

The client token and the session secret protect access to MCP. They do not protect against a process that has taken over the user's system account, the application's memory or the client's own configuration. Database results and file text are untrusted data and may contain instructions aimed at the agent. They can never automatically increase its permissions.

The database administrator and the local file system are trusted. Checking table identity prevents another object from inheriting a grant after DDL; it does not protect against an administrator modifying the engine or a process replacing the whole SQLite file with a structurally compatible one.

The MCP panel and prompts are a DBeast security feature. The control layout, action names and colors follow the conventions of the rest of the application.

## Increment verification

Final check from October 15, 2026. Tests used the unoptimized profile and `CARGO_INCREMENTAL=0`:

- `cargo test -- --test-threads=1 --nocapture`: 534 tests passed, zero failures and 35 tests marked `ignored`. Nothing was skipped because of unavailable fixtures. Local PostgreSQL, MySQL, PostGIS and SSH were running. The TLS tests and the MCP benchmark were run separately; the native Keychain test was not part of this increment.
- After the mechanical fixes required by Clippy, `cargo test --lib mcp:: -- --test-threads=1 --nocapture` was run again: 46 tests passed and one manual benchmark was skipped, 5.31 s. This covers the broker, SQL validation, identity and grant boundaries, limits, refusals, expiry, protection of uncertain write outcomes and the liveness of native tasks after cancellation.
- Final `cargo test --test mcp_execution --test mcp_server --test ui_mcp -- --test-threads=1 --nocapture`: execution 7/7 in 2.16 s, HTTP 4/4 in 10.36 s, full UI panel 1/1 in 2.97 s. A real client built on the official SDK checks current and legacy initialization, approval of a document snapshot and SQL against a local SQLite file. Negative attempts cover missing and revoked authorization, Host, Origin, an oversized chunked body and a slow HTTP body. The table scope test confirms a separate prompt for a second table and for a write, and that data is unchanged after a refusal.
- PostgreSQL tests confirmed that metadata changes are blocked during a strict read and that execution of a prepared plan is refused after its columns or types change. An `INSERT RETURNING` attempt after such a change neither performed the write nor retried it. `SHOW`, `EXPLAIN`, `VALUES`, `CREATE`, `ALTER`, `INSERT` and `INSERT RETURNING` work in Full SQL. The MySQL test leaves a silent peer with an open socket and checks that the connection is closed after the deadline and that the concurrency slot is recovered after the caller is cancelled.
- `scripts/test-tls.sh`: 4/4 tests in 9.87 s after the final driver changes. Separate, disposable containers checked TLS verification, client certificates, the SSH tunnel, cancellation and MCP executor connections. A later Clippy fix in a test only replaced a `format!` of a literal with `to_owned`, keeping the SQL text identical.
- `cargo clippy --all-targets -- -D warnings`: finished with no warnings. `git diff --check` is clean. The touched MCP files and the TLS test pass Rustfmt; the broader `cargo fmt --all -- --check` is not clean and reports differences in files that integrate MCP and in earlier Explain, formatting and password store code. This increment does not reformat those files in bulk.
- `scripts/bundle-macos.sh` ran `cargo build --release` and built `target/release/DBeast.app` for arm64 in 3 min 22 s. `codesign --verify --deep --strict` and `plutil -lint` passed. This is a local ad hoc signature, without notarization. The final build and the focused tests used the same source files and manifests.
- Screenshots `84-mcp-server.png`, `85-mcp-sharing-small.png`, `86-mcp-approval.png`, `87-mcp-approval-small.png`, `88-mcp-sessions.png` and `89-mcp-sessions-small.png` were generated in `docs/screenshots/` and reviewed. The panel, the full session ID, the SQL, the grant scopes and the revoke actions stay readable at 740 × 500 as well. The dark theme and controls match the existing DBeast style; the approval mechanism itself is the extension described above. The screenshots stay local, following the repository rules.
- Transport, broker and UI tests used a temporary `DBEAST_CONFIG` and an in-memory secret store. SQL execution was checked only against temporary files and local test containers, without the real configuration, Keychain or the user's external database.

## MCP controller measurement

Command: `CARGO_INCREMENTAL=0 cargo test --lib mcp::broker::tests::benchmark_controller_snapshots -- --ignored --nocapture`. Debug profile, no database, network, UI drawing or GPU. A snapshot read is the median of 7 runs of 10,000 calls each, after 1000 warm-up calls. Final results: server disabled 0.045 µs, enabled and idle 0.046 µs, 32 pending requests 0.046 µs. The read cost includes a mutex and cloning an `Arc` handle, without copying preview contents.

A separate measurement of 50 publications of new state with 32 maximum-size SQL previews: median 0.143 ms, maximum 0.271 ms. Publication runs on every state change and once per second for active sessions. The measurement shows the cost of the controller and its cache; it does not prove 60 fps for the whole UI, window startup time or query latency against external databases.
