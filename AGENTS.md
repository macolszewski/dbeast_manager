# DBeast: a fast database client in Rust

Instructions for agents working in this repository. The feature overview for humans is in [README.md](README.md), and the full user documentation is the MkDocs site under `docs/`.

## Project goal

DBeast is a database client with the comfort of a mature IDE, written from scratch in Rust and therefore noticeably faster than tools that run on the JVM. It is an open source project under active development, growing into a complete tool for everyday work.

- **Consistency with the existing interface**: window layout, tool windows, context menus, action names, keyboard shortcuts, colors, editor and grid behavior, and log messages must match what the application already does. Design new features so they feel familiar to users of professional database tools. If you change established behavior (for example, for performance or data safety), explain the change in your reply to the user and in the documentation.
- **Performance is the reason the project exists.** JVM-based clients start slowly, use a lot of memory and stutter on large results. DBeast must start instantly, use little memory and scroll large results smoothly. A feature that works correctly but slowly is not finished.

## Assets and licenses

- Do not copy third-party code, icons, graphics or theme files. Layout is defined in `src/ui/theme.rs`, and icons are drawn as vectors.
- The interface and SQL syntax colors in `src/ui/theme.rs` are taken from the IntelliJ Platform dark theme (Apache License 2.0). The attribution is in `NOTICE` and the README; keep it when colors change, and record any new third-party source the same way.
- Queries against system catalogs are based on the database vendors' documentation (PostgreSQL, MySQL, SQLite).
- Fonts: Inter and JetBrains Mono under the OFL; license files are in `assets/fonts/`.

## Performance rules

- The UI thread only draws. Every database, network or disk operation that may take time goes through `backend.rs` (tokio runtime) and comes back as a `backend::Event`, handled in `App::handle_events` in `src/ui/mod.rs`.
- Load everything lazily: the explorer tree one level at a time, results in pages (500 rows by default), and completion metadata once per connection.
- The grid draws only visible rows. Do not add per-frame work proportional to the row count: sorting, serialization and recalculation must run once or be cached (example: `ResultSet::data_json`).
- Avoid allocating or cloning large structures every frame. Cache expensive results and invalidate them explicitly.
- Targets, not yet measured in CI: window startup under 0.5 s, 60 fps scrolling in results and the editor, and memory use several times lower than JVM-based clients with the same set of tabs. For changes that may affect these, measure and report the result. `DBEAST_TRACE_STARTUP=1 ./target/release/dbeast` (with a temporary `DBEAST_CONFIG`) prints the time to first frame, and `ps -o rss=,%cpu= -p <pid>` shows idle memory and CPU.

## Architecture

Rust 2024, GUI in egui/eframe 0.36 (immediate mode rendering), UI tests with egui_kittest on wgpu.

| Path | Contents |
|---|---|
| `src/model.rs` | shared types: `DataSource`, `Value`, `QueryResult`, `DbObject`, `Page`, `Scope` |
| `src/drivers/` | `Connection` for PostgreSQL (`tokio-postgres`), MySQL (`mysql_async`), SQLite (`rusqlite`) and the SSH tunnel (`russh`). Each driver provides query execution, the object tree, metadata, primary keys and DDL |
| `src/backend.rs` | sessions per tab and per database, background execution, cancellation, events to the UI |
| `src/splitter.rs` | splitting scripts into statements |
| `src/completion.rs` | clause-aware code completion |
| `src/formatter.rs` | SQL formatting with a check that tokens are unchanged |
| `src/folding.rs` | code folding in the console |
| `src/sqlgen.rs` | literals, identifiers and parameterized DML from grid edits (`bound_mutations`, shared by table tabs and console results) |
| `src/result_edit.rs`, `src/ui/result_editing.rs` | whether a result is editable (column origin, primary key, database metadata) and saving through `backend::apply_update_batch` |
| `src/export.rs` | CSV, TSV, JSON, SQL INSERT, Markdown |
| `src/files.rs` | CSV/TSV, Data Package and SQLite files as temporary data sources |
| `src/geo.rs` | geometry decoding and coordinate system transformation |
| `src/depth.rs`, `src/ui/results/depth_view.rs` | Depth View: reading time columns and the timeline model, view with a time range |
| `src/schema_graph.rs`, `src/drivers/*/graph.rs`, `src/ui/schema_graph.rs` | Database Graph: model of objects, metrics and relationships, per-driver catalog reads, force-directed layout and the graph window |
| `src/ui/scene3d.rs` | shared orbit camera and perspective projection for 3D scenes (Depth View, Database Graph) |
| `src/config.rs` | `config.json` (data sources, history, filters); `DBEAST_CONFIG` overrides the path |
| `src/ui/mod.rs` | `App`: application state, tabs, event handling, main window layout |
| `src/ui/session.rs` | saving and restoring the work session (`session.json`, result rows in `session-results/`) |
| `src/ui/console.rs` | console and table editor: toolbar, SQL editor |
| `src/ui/results.rs` | result grid with editing, Output log |
| `src/ui/explorer.rs`, `scope_chooser.rs` | Database Explorer and database/schema selection |
| `src/ui/dialog.rs` | Data Sources and Drivers dialog |
| `src/ui/theme.rs` | dark theme palette, fonts, shared widgets |
| `src/ui/about.rs` | About DBeast screen: animation, ASCII portrait, music on a separate thread (`rodio`), mute setting saved in the configuration |
| `src/ui/map.rs`, `json_view.rs`, `files_panel.rs`, `highlight.rs` | Geo Viewer, JSON panel, Files window, syntax highlighting |
| `docs/`, `mkdocs.yml` | MkDocs documentation site: user guide, reference, design notes, images |

UI pattern: views collect user intent into enums or event structs (`TreeAction`, `GridEvents`), and state changes after drawing. This avoids borrow conflicts in egui. Add new actions the same way.

## Conventions

- Code, identifiers, comments, commits, README and all documentation in `docs/` are in English. UI text is in English and uses standard action names (for example "Add Row", "Show Record View").
- User-facing documentation is the MkDocs site under `docs/`, configured by `mkdocs.yml`: the user guide is in `docs/guide/`, the reference in `docs/reference/` and design notes in `docs/design/`.
- Keyboard shortcuts follow macOS conventions. In code, define them as `KeyboardShortcut` with `Modifiers::COMMAND` so they map to Ctrl on other systems. Write shortcut text in the UI with macOS symbols and pass it through `theme::keys`, which shows Ctrl+Shift+L and so on on Linux and Windows. The application must work on macOS, Linux and Windows: platform-specific code goes behind `cfg` only. Verify Linux with a build in a Linux container and Windows with `cargo clippy --target x86_64-pc-windows-gnu --all-targets` (mingw-w64 installed); the on-demand Windows workflow runs the unit tests there.
- Every new user-visible feature needs a test (unit test or UI test in `tests/ui_*.rs`), an entry in the relevant user guide page in `docs/guide/`, and an entry in the keyboard shortcuts reference in `docs/reference/` if it has a shortcut.
- UI tests write screenshots to `docs/screenshots/`. Before finishing, look at the screenshot of the changed view and check that it is consistent with the rest of the interface. Screenshots used by the documentation are copied from `docs/screenshots/` into `docs/images/` by `scripts/docs-screenshots.sh`. `docs/screenshots/` is not committed; only `docs/images/` is. Run the script after changing a view that the documentation shows, with the test databases up so every screenshot has data.
- Database, SSH, SSH private key and TLS identity passwords are stored in the macOS Keychain, in Secret Service on Linux (`secret-service`; the `tests/secret_service.rs` test runs only with `DBEAST_TEST_SECRET_SERVICE=1`), encrypted with DPAPI in files under the configuration folder on Windows, or only in application memory (Until restart). An older `config.json` may contain plain-text passwords for migration. Do not read the real configuration during tests, do not print secrets and do not copy them into logs or tests. UI tests use `App::new_for_test` and a temporary `DBEAST_CONFIG`, which explicitly injects an in-memory store. The native keychain test uses only a dedicated temporary keychain.

## Running and testing

```bash
export PATH=/opt/homebrew/opt/rustup/bin:$PATH   # rustup is not on PATH on the author's machine
./scripts/test-dbs.sh                            # containers: PostgreSQL :55432, PostGIS :55433, MySQL :53306, SSH :52222
cargo test                                       # tests that need a database are skipped when the container is not running
cargo clippy --all-targets                       # must produce no warnings
cargo build --release && ./target/release/dbeast
```

UI tests set `DBEAST_CONFIG` to a file in a temporary directory, so they never touch the user's configuration. Do the same in new tests.

## Git

Commit changes locally on `master` after each finished task, using Conventional Commits. Push only when the user explicitly asks.

## What's next

The list of missing features is in the "Roadmap" section of the README. Main directions: more grid features (Record View, Aggregate View, local filters), editing console results, EXPLAIN plans, a system password store on Windows, more drivers, and performance measurements against JVM-based clients.
