# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0] - 2026-10-18

### Changed

- The work session keeps the rows of saved results in a `session-results`
  folder next to `session.json`, one file per result, written once in the
  background. The session is read in the background on start. Sessions saved
  by earlier versions are still restored.
- Result pages also end at 64 MB of decoded values; the range label then shows
  "(64 MB page limit)" and the next page continues after the rows kept.
  Export and import batches are bounded by bytes as well as rows.
- Grid columns are resized by dragging the right edge of a header;
  double-click restores the automatic width.
- Sorting compares decimals exactly and puts `NaN` after all other numbers.
- The Output log keeps the last 10,000 messages of a console, up to 8 MiB
  (64 KiB of one message), says how many earlier messages were removed and no
  longer wraps long lines.
- Stop ends Submit and Related Rows reads between statements; a Stop that had
  to cancel a running statement closes the session. Submit or Related Rows in a
  console that is busy is refused with a message instead of waiting.
- The value panel prepares a value once, large values in the background, and
  lays out only the visible lines and JSON tree rows.
- Grid cells draw at most 1024 characters, and wide results and Transpose
  build only the columns in view.
- Opening a CSV file streams it into the temporary database instead of reading
  it into memory.
- Map tile textures are limited to 64 MiB; the Geo Viewer prepares geometries
  in the background and draws only features in view.
- Related Rows pages are limited on the server, and their export streams to
  disk in batches.
- PostgreSQL table pages no longer open a cancel connection; PostgreSQL Submit
  sends up to 32 updates before reading their results, and SQLite runs a whole
  Submit in one task.
- Local value filters no longer copy the column's values into their list.

### Fixed

- A slow or unreachable SSH server no longer blocks other data sources from
  opening or reusing their tunnels.
- AI agent requests release their copy of a shared document when they finish,
  and repeated cancellation no longer extends how long they are kept.
- Opening a CSV file that changes while it is read reports an error.

## [0.2.0] - 2026-10-17

### Added

- Windows 10 and 11 (x86-64) support: passwords encrypted with DPAPI, ssh-agent
  through the OpenSSH named pipe or Pageant, PostgreSQL client tools found in
  Program Files, Show in Explorer, an embedded icon and no console window.
  `scripts/build-windows.sh` cross-compiles a zip, and an on-demand workflow
  builds and tests on Windows.
- Release workflow: pushing a version tag publishes ready-to-run macOS, Linux and
  Windows packages with checksums as a GitHub release.

### Changed

- MySQL and PostgreSQL strict reads give a separate reason for each refusal
  (missing table, view, storage engine, partitioning, too many columns,
  unsupported column type, generated column) and say that the whole table is
  checked, including columns the query does not select.
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
- PostgreSQL arrays, numeric and interval values are shown and exported
  exactly as PostgreSQL prints them: quoted array elements, multi-dimensional
  arrays and lower bounds, all numeric digits, NaN, infinities, interval
  microseconds and negative parts. Exports stop with an error instead of
  writing a placeholder for a value that cannot be read exactly.
- A PostgreSQL query that fails at or after the end of the fetched page is
  reported as an error, not as a successful page.

## [0.1.0] - 2026-10-17

First public version (source only).

### Added

- SQL console, result grid with editing, Database Explorer, query plans, data
  import and export, PostgreSQL client tools, Activity Monitor, Geo Viewer,
  Depth View, Database Graph and a local MCP server for AI agents, for
  PostgreSQL, MySQL and SQLite.
- About DBeast screen with theme music.
- Documentation site at <https://macolszewski.github.io/dbeast_manager/>, built with
  MkDocs from `docs/`.
- Continuous integration on Linux (clippy and unit tests); the full test suite runs locally.

### Changed

- All documentation, comments and project files are in English.
- In-app window title bars on macOS show the close button on the left.

### Fixed

- The integrated macOS title bar keeps room for the window buttons at every
  Zoom IDE level.
