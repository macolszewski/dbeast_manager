# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- First public release: SQL console, result grid with editing, Database
  Explorer, query plans, data import and export, PostgreSQL client tools,
  Activity Monitor, Geo Viewer, Depth View, Database Graph and a local MCP
  server for AI agents, for PostgreSQL, MySQL and SQLite.
- About DBeast screen with theme music.
- Documentation site at <https://macolszewski.github.io/dbeast_manager/>, built with
  MkDocs from `docs/`.
- Continuous integration on Linux (clippy and unit tests); the full test suite runs locally.

- Windows 10 and 11 (x86-64) support: passwords encrypted with DPAPI, ssh-agent
  through the OpenSSH named pipe or Pageant, PostgreSQL client tools found in
  Program Files, Show in Explorer, an embedded icon and no console window.
  `scripts/build-windows.sh` cross-compiles a zip, and an on-demand workflow
  builds and tests on Windows.

### Changed

- All documentation, comments and project files are in English.
- MySQL and PostgreSQL strict reads give a separate reason for each refusal
  (missing table, view, storage engine, partitioning, too many columns,
  unsupported column type, generated column) and say that the whole table is
  checked, including columns the query does not select.
- In-app window title bars on macOS show the close button on the left.
- Table editor pages are read with `LIMIT`/`OFFSET` on the server and, without
  an ORDER BY, in primary key order, so each page shows the same rows.
- A plain PostgreSQL console SELECT outside a transaction is stopped on the
  server once its page is read; other statements are read to the end.

### Security

- Narrowing what is shared with an AI agent connection (a lower database or
  table level, a table set to Not Shared, or a removed connection entry)
  revokes that connection's session permissions on the source and its waiting,
  running and finished requests there. Results are released and automatic
  execution starts only while a session permission or standing access still
  covers the request.

### Fixed

- MySQL: after DDL commits a transaction implicitly, the console no longer
  shows a stale open transaction. Tx: Manual opens a new transaction before
  the next statement, so Rollback undoes it, and the Output log reports the
  implicit commit.
- MySQL import into MyISAM and other non-transactional tables: "Delete
  existing rows" and "Stop and roll back" are refused before any change, and
  Skip mode inserts row by row, so rows are no longer written twice and the
  reported counts and line numbers are correct.
- Saving the session no longer blocks the interface: unchanged sessions are
  detected without serializing results, results are shared instead of copied,
  and the file is written on a background thread.
- The integrated macOS title bar keeps room for the window buttons at every
  Zoom IDE level.
- PostgreSQL arrays, numeric and interval values are shown and exported
  exactly as PostgreSQL prints them: quoted array elements, multi-dimensional
  arrays and lower bounds, all numeric digits, NaN, infinities, interval
  microseconds and negative parts. Exports stop with an error instead of
  writing a placeholder for a value that cannot be read exactly.
- A PostgreSQL query that fails at or after the end of the fetched page is
  reported as an error, not as a successful page.
