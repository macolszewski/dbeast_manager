# Testing

DBeast has three layers of tests: unit tests inside the crate, integration tests against real databases in Docker containers, and headless UI tests that drive the application and save screenshots. Tests that need a database are skipped when its container is not running, so `cargo test` works on a fresh machine too.

## Running the tests

```sh
./scripts/test-dbs.sh        # start the test databases in Docker
cargo test                   # run all tests
cargo clippy --all-targets   # lint; expected to produce no warnings
```

`scripts/test-dbs.sh` starts (or reuses) these containers and loads the sample data:

| Service | Address | Credentials |
|---|---|---|
| PostgreSQL 17 | `localhost:55432` | `dbeast` / `dbeast`, databases `dbeast` and `dbeast_archive` |
| PostGIS 3.5 | `localhost:55433` | `dbeast` / `dbeast`, database `gis`, schema `geo` |
| MySQL 8.4 | `127.0.0.1:53306` | `dbeast` / `dbeast` |
| SSH server | `localhost:52222` | user `tunnel`, password `tunnel`, keys in `tests/ssh/keys`; databases reachable inside as `pg:5432` and `mysql:3306` |

The seed data is in `tests/seed_pg.sql`, `tests/seed_pg_archive.sql`, `tests/seed_postgis.sql` and `tests/seed_mysql.sql`.

## Test isolation

Tests never read or write your real configuration. UI tests set `DBEAST_CONFIG` to a file in a temporary directory and use an in-memory credential store, so no password ever reaches the system keychain, the logs or the test files. New tests must follow the same rule.

## Unit tests

Unit tests are inside the source modules and cover, among other things:

- splitting scripts into statements,
- code completion,
- generating DML from grid edits,
- export formats,
- recognizing transaction statements,
- the MCP read-only SQL parser and policies,
- result comparison, plan normalization and other computations.

Several modules also contain benchmarks marked `#[ignore]`. They run only on request; the commands are listed in [Performance](performance.md).

## Integration tests

Integration tests in `tests/` run against the Docker databases, local SQLite files or a local MCP server:

| Test | Covers |
|---|---|
| `drivers.rs` | Queries, types, errors, paging, primary keys, metadata and DDL on live databases |
| `backend.rs` | Manual transactions with rollback, edit batches rolled back as a whole on error, exporting a whole result to a file |
| `transfer.rs` | Streamed export, stopping, and the throughput of a large export |
| `ssh.rs` | SSH tunnels to PostgreSQL and MySQL: password, key, key with passphrase, ssh-agent, wrong credentials and a changed host key |
| `geo.rs` | Geometry from PostGIS and MySQL, column detection, SRID from data, and reprojection matching PostGIS to 0.000001° |
| `ddl_diff.rs` | Fresh definitions from separate sources, object types and paths, errors and cancellation |
| `related_rows.rs` | Foreign keys, typed parameters, both directions, composite keys, paging and export, collation and SQLite affinity rules, session limits |
| `explain.rs` | Plans on three engines, the current session and transaction, no DML execution, errors and cancellation; Explain Analyse on PostgreSQL and MySQL: single execution in Auto, Commit and Roll Back in Manual, earlier work kept after errors and Stop, read-only sources, CTEs and functions, temporary tables, unsupported `INSERT` on MySQL and uncertain results after `KILL QUERY` |
| `schema_graph.rs` | Database Graph from PostgreSQL catalogs (partitions, composite and `NOT VALID` foreign keys, views, materialized views, FDW without server options, a table without statistics) and MySQL |
| `mcp_server.rs` | A real MCP SDK client over loopback in current and older protocol versions, authentication, Host and Origin checks, HTTP limits and deadlines, approvals for documents and SQL, and the boundaries of table grants |
| `mcp_execution.rs` | Execution on separate connections, the read-only parser, table identity, result limits, client isolation, expiry, revoked grants, idempotency and uncertain write outcomes |

## UI tests

The `tests/ui*.rs` files test the interface without a window, using `egui_kittest` with the wgpu renderer. They click through the tree, run queries, use completion, open and roll back transactions, edit and submit cells, clone and delete rows from the context menu and with shortcuts, and generate DDL. Areas covered include:

- the console editor, Find and Replace, formatting, folding and editor performance,
- the results grid, selection, Record View, Aggregate View, local filters and Find on Current Page,
- editing console results, Related Rows, Compare Data and DDL Diff,
- query plans and Explain Analyse on PostgreSQL and MySQL,
- the Database Explorer, scope chooser, groups, per-console schema and session restore,
- Geo Viewer, Depth View and Database Graph,
- Export, Import, `pg_dump`, Activity Monitor and the Run window,
- the MCP settings, sharing, approvals and sessions,
- Data Sources and Drivers, TLS settings, password storage, appearance, Zoom IDE, the title bar and the About screen.

Several tests render both normal and narrow window sizes (for example 1440×900, 1280×720 and 1100×500) to check that toolbars and dialogs fit.

UI tests run on Linux as well; there they render through Vulkan (lavapipe).

### Screenshots

UI tests save screenshots to `docs/screenshots/`. Only `01-console.png` is tracked in git; the rest are regenerated by each test run. After changing a view, look at its screenshot before finishing the work.

The documentation uses copies of these screenshots in `docs/images/`. After running the UI tests, copy the referenced screenshots with:

```sh
./scripts/docs-screenshots.sh
```

## Optional and opt-in tests

| Test | How to run | Notes |
|---|---|---|
| TLS (`tests/tls.rs`) | `scripts/test-tls.sh` | Starts disposable, loopback-only PostgreSQL 17, MySQL 8.4 and SSH containers with generated certificates, then runs the TLS tests. Without arguments it runs `cargo test --test tls -- --ignored --test-threads=1 --nocapture`; pass another command to run that inside the fixture environment instead. |
| Secret Service (`tests/secret_service.rs`) | `DBEAST_TEST_SECRET_SERVICE=1 cargo test --test secret_service` | Linux only. Stores, reads, overwrites and deletes a password in Secret Service. Requires a running keyring daemon such as GNOME Keyring on the session bus; skipped without the variable. |
| Native Keychain (`tests/keychain_native.rs`) | `cargo test --test keychain_native -- --ignored` | macOS only. Uses a disposable keychain created for the test and never opens the login keychain. |
| Map tiles in `tests/ui_geo.rs` | `DBEAST_TILES=1 cargo test --test ui_geo` | Also loads OpenStreetMap tiles; without the variable the test does not depend on the network. |
| Benchmarks | see [Performance](performance.md) | Marked `#[ignore]`; run in release mode with `--ignored --nocapture`. |

The TLS script covers all SSL modes, CA files in PEM, DER and bundle form, a wrong CA, a host name mismatch, PEM client certificates with three key types, a key from another certificate, PKCS#12 with a correct and a wrong password, and connections through SSH.

## What is skipped without containers

When the Docker containers are not running, tests that need them print a skip message and pass without doing anything. This applies to:

- driver, backend, transfer, SSH, geometry, Related Rows, Explain, DDL Diff and Database Graph tests on PostgreSQL and MySQL,
- UI tests that use PostgreSQL, PostGIS or MySQL.

Tests on local SQLite files, unit tests and most UI tests still run. TLS, Secret Service and native Keychain tests always need their explicit opt-in.

## Linux builds

DBeast builds and runs on Linux. The build is checked in a Linux container to make sure that platform-specific code compiles on both systems. See [Getting started](../guide/getting-started.md) for the packages needed to build on Debian and Ubuntu.
