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

### Changed

- All documentation, comments and project files are in English.
- In-app window title bars on macOS show the close button on the left.

### Fixed

- MySQL: after DDL commits a transaction implicitly, the console no longer
  shows a stale open transaction. Tx: Manual opens a new transaction before
  the next statement, so Rollback undoes it, and the Output log reports the
  implicit commit.
- The integrated macOS title bar keeps room for the window buttons at every
  Zoom IDE level.
