# Known limitations and roadmap

This page collects what DBeast does not do yet, and the deliberate limits of features that exist. Most first versions are intentionally narrow: they cover a well-defined case safely and say clearly when a request falls outside it.

## Roadmap

Planned directions:

- Parquet and DuckDB file formats,
- comparing whole schemas,
- relationship diagrams of tables,
- a system password store on Windows,
- more database drivers,
- further editor work: the remaining cost of the standard egui text editor, the first layout of very large SQL, preparing configuration snapshots and saving the session off the interface thread, and more search options.

## Platforms and drivers

- DBeast runs on macOS, Linux and Windows 10 or later (x86-64). The Windows code is checked on every change by cross-compiling, and the on-demand Windows workflow runs the unit tests; the Windows build has had less hands-on use than the macOS and Linux builds, and no signed installer is published yet.
- Supported databases are PostgreSQL, MySQL and SQLite, plus CSV, TSV and Data Package files opened as temporary sources. MariaDB is not specifically supported (for example by Explain Analyse).
- The macOS application bundle is signed ad hoc and not notarized. macOS may ask for keychain access again after the application is rebuilt or a different executable is run.

## Passwords and connections

- All four secrets of a data source (database, SSH, SSH key passphrase and TLS identity password) share one storage policy. There is no separate "Never" mode.
- Saved passwords are bound to the location of the configuration file. A copy elsewhere cannot use them; an independent configuration needs the passwords entered again.
- Encrypted PEM private keys are not supported for TLS client certificates; use PKCS#12 instead.
- After **Stop** on PostgreSQL, the session must be disconnected and reconnected, and an open transaction is not preserved. The result of an interrupted operation may be unknown and is never retried automatically.

## Console and editor

- Find and Replace have no regular expressions, Search In Selection, Preserve Case or multi-line search fields. Case-insensitive matching uses Unicode lowercase without normalization or full case folding (`ß` differs from `ss`).
- **Find Action** is a palette of commands only; it does not search settings, files or database objects.
- Closed consoles cannot be recovered. DBeast warns before closing any non-empty console.
- The per-console schema selector sets a single schema. Editing a multi-element `search_path` and a separate script mode are not available.
- The result context does not interpret arbitrary SQL or changes made inside procedures. On PostgreSQL, transactions hidden in procedures are not tracked. On MySQL without an initial database, **Default** cannot return to "no database" after `USE`.
- **Scroll from Editor** selects the data source of a console; locating the object under the SQL cursor and automatically following the editor are not supported yet.

## Results grid

- Transpose still creates a column for every record on the page, so very large pages in Transpose need further horizontal virtualization.
- Local column filter settings are not saved between application runs.
- Aggregate View works on the current page only. Custom aggregator scripts and pasting a range are not available.
- The value panel has no hex editor for binary values. Its layout cannot be changed yet, and very large values are not specially optimized.

## Editing console results

Only a simple `SELECT` from a single table that includes the full real primary key can be edited. Not supported:

- joins, CTEs, aggregates, compound `SELECT` statements and function calls in the projection,
- virtual keys, choosing the target table manually, and SQL expressions typed into cells,
- PostgreSQL tables with inheritance or partitioning,
- MySQL temporary tables, views and non-InnoDB tables,
- SQLite virtual tables.

PostgreSQL `NUMERIC` values with more than 28 significant digits or a scale above 28, `NaN` and infinities, MySQL `FLOAT` and unsupported types are read-only. They are still shown and exported exactly.

A PostgreSQL value of a type DBeast cannot read exactly (for example `money`, `tsvector` or a type from an extension) is shown as `<type: hex>`. Exporting it to a file fails with an error, but **Export ▸ Copy as …** copies the placeholder as shown.

## Related Rows

The first version navigates from one existing row in one physical source along explicit foreign keys, in a separate session with committed data. Not supported yet:

- an open transaction in the source console, temporary tables, SQLite `ATTACH` and in-memory databases, sources from imported files,
- MySQL `TIMESTAMP` keys that would require time zone transfer,
- PostgreSQL partitioned or inherited tables,
- virtual foreign keys, an inline preview, and navigation from several records at once.

Related Rows results are read-only. On SQLite, foreign keys are shown with synthetic names (`FK n`) because the PRAGMA catalog does not return constraint names.

## Query plans

- One plan per console; plans are not saved between runs.
- No flame graph, no custom `EXPLAIN` options, no global shortcut for Explain Analyse and no shortcut for the diagram.
- The diagram handles up to 100,000 operations; larger plans are available in Operations Tree and Raw.
- Explain Analyse is not available for SQLite. On MySQL it requires 8.0.21 or later and rejects `INSERT`.

See [Query plans](../guide/query-plans.md).

## Comparing structure and data

- **Compare Structure With** compares the exact DDL text of two tables or views. It does not generate migrations, compare whole schemas or apply changes. PostgreSQL DDL does not cover full identity and generated column semantics, privileges, triggers or row-level security, and partitioned, inherited, foreign and temporary tables are excluded. SQLite `ATTACH` and in-memory databases are not supported.
- **Compare Data** compares the fetched pages of two results. It has no tolerance setting, no heuristic column insertion detection and no data synchronization, and comparisons are not saved between runs.

## Import, export and tools

- MySQL dump (`mysqldump`) is not supported.
- PostgreSQL tools cannot use a PKCS#12 client certificate; provide PEM files.
- Exporting a whole MySQL result does not recreate temporary tables or other session settings, and a PostgreSQL result that uses temporary tables cannot be exported in full.

## Visualization

- The Geo Viewer always shows the whole fetched page; local filters apply to the grid only.
- Depth View works on the fetched page only and does not guess epoch times from numbers.
- The Database Graph scans up to 5,000 objects. On SQLite, sizes are measured only for files up to 256 MB, and view dependencies are not available.

## AI agents (MCP)

- The server listens only on `127.0.0.1`. Agents running only in a remote cloud environment cannot reach it.
- Read-only verification rejects PostGIS and other extension types, enums, domains, row-level security, and indexes or CHECK constraints with user-defined functions on PostgreSQL. MySQL requires InnoDB and the `LOCK TABLES` and `PROCESS` privileges.
- Results are limited to 500 rows and 256 KiB of data. Agents cannot open files by path.
- Parallel conversations need separate agent connections, because a new `begin_session` replaces the previous session of the same client.

See [AI agents](../guide/ai-agents.md) and the [MCP contract](../design/mcp-contract.md) for the complete list.
