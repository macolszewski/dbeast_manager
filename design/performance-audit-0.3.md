# Performance and memory audit of 0.3.0

Verification record of the source audit of version 0.3.0 (commit `e5b9fff`, 18 October 2026). Each finding was traced on the current code, reproduced with a deterministic test or measured, and fixed where the mechanism was confirmed. Measurements are release builds on Apple Silicon unless stated otherwise; several were taken while the host ran parallel builds, which is noted where it matters. CPU frame times exclude GPU rendering and presentation. "Before" figures come from the same benchmark run on the earlier code or from an exact copy of the earlier code path inside the benchmark.

## Summary

| ID | Finding | Status | Main evidence |
|---|---|---|---|
| F01 | Autosave copies all consoles and queues full configurations | Fixed | Per change 1.38 ms → 0.85 µs (20 MiB in 10 consoles); at most one ordinary save in flight |
| F02 | Output log keeps large allocations after cutting a message | Fixed | Test checks `capacity()` of cut messages and the byte limit counted by owned buffers |
| F03 | Geometry detection parses large values on the UI thread | Fixed | 20 × 10 MiB plain JSON: 329 ms → 16 µs on the UI thread |
| F04 | MySQL console read drains the whole result | Fixed | First page of 1,000,000 rows: 421 ms, 112 MB → 0.92 ms, 55 KB |
| F05 | No overall memory budget for results | Fixed, with limits | 1 GiB budget with release to session files; script results wait while the UI has 256 MiB untaken |
| F06 | Session restore loads every result | Fixed | 30 tabs: active tab rows after 11 ms, other tabs on demand; benchmark peak RSS 69 → 44 MB |
| F07 | Import and export previews without a byte budget | Fixed | Export preview with 7 MB cells: 541.6 ms, 173.9 MB → 0.15 ms, 6,953 B |
| F08 | Import cancellation checked too rarely | Fixed | Write waiting on a row lock ends 6 ms (PostgreSQL) and 11 ms (MySQL) after Stop, previously no end within 10 s |
| F09 | Abandoned MySQL connections keep draining | Fixed | Socket closed 2.0 s after Stop instead of reading 512 MB more |
| F10 | Undo history keeps full SQL copies without a byte budget | Fixed | Test: 50 versions of a 4 MiB script stay within 16 MiB |
| F11 | Value panel cache keeps large data after a result closes | Fixed | Tests: views and workers released on close and on hiding the panel; 128 MiB budget |
| F12 | Map rebuilds intersecting features on every repaint | Fixed | Line of 2,000,000 points, hover: 27.2 ms → 0.05 ms CPU |
| F13 | Depth View threshold by rows and no cancellation | Fixed | 5,000 × 60 time columns, first frame: 227.5 ms → 0.06 ms |
| F14 | Database Graph rebuilds on hover and builds off-screen objects | Fixed | Few objects in view, orbit: 46.4 ms → 0.73 ms CPU |
| F15 | Files panel does file system work while drawing | Fixed | 5,000 files: 16.0 ms per frame → no file system access per frame |
| F16 | Completion loads the whole catalog eagerly | Fixed | Own connection, streamed; 3,000 tables × 40 columns: names after 11 ms, columns after 165-243 ms |
| F17 | Deep OFFSET and per-page primary key read | Fixed for the key; keyset paging not adopted | Key read once per table (4 → 1 catalog queries for 4 pages) |
| F18 | Large editor work proportional to the text | Targeted fix; remaining cost in egui | 1 MB SQL: highlighting 15.6 → 8.1 ms; typing frame 38.5 → 36.0 ms |

## F01: configuration autosave

Every console edit called `save_config()`, which cloned the SQL of all consoles and the whole `Config`, and sent it through an unbounded channel to the persistence worker, which wrote each snapshot in turn; the writer encoded the configuration twice (file and revision).

A change now only marks the configuration dirty (`App::save_config`). `App::flush_config` takes the snapshot after edits pause (400 ms, at most 2 s while typing continues), with at most one ordinary save in flight; changes made meanwhile remain a flag. Console and history text is `Arc<str>`, and an unchanged console reuses the buffer of the previous snapshot. Apply, Load and credential operations are not coalesced and keep their order; a change during Apply is saved after it. Closing waits until the latest revision is written. The writer encodes once (`atomic_write_bytes`).

Test `autosave_coalesces_edits_and_shares_unchanged_console_text`: 1,000 edits take no snapshot until the pause; while a save is in flight further edits do not queue another; the latest revision is written; an unchanged console shares its `Arc`. Benchmark `autosave_cost_benchmark`: 1.38 ms per change before, 0.85 µs after; a snapshot with one changed console of 10 takes 1.5 ms.

## F02: Output log buffers

`String::truncate` keeps the whole allocation, and the 8 MiB limit counted text length. Cut messages now get a new buffer, the limit counts `capacity()`, and statements are written to the log through `results::log_text`, which copies at most 64 KiB of the parts. Test `output_log_keeps_bounded_messages_and_bytes` checks capacity after a 4 MiB Unicode message, shrinking of short messages in large buffers and that the counted bytes equal the owned buffers.

## F03: geometry detection

The result toolbar called `geometry_columns()`, which parsed up to 20 sampled values per column on the UI thread, plain JSON included, and a NULL-only column was scanned to the end of the page. The UI thread now decides by column type and small samples (`geo::detect_geometry_columns`: 1,000 rows, 256 KiB of values for the whole result, JSON without GeoJSON members rejected by a scan). Columns with larger samples are confirmed by `geo::confirm_geometry_columns` on at most two workers owned by the result, cancelled when it is dropped; the Geo Viewer appears when they finish. Benchmark `geometry_detection_cost_benchmark`: 20 values of 10 MiB plain JSON took 329 ms on the UI thread before; now 16 µs on the UI thread and 1.7 ms on the worker, which rejects the column by a scan without parsing. Layers still decode geometries again when the viewer opens.

## F04: MySQL console pages

`MyConn::read_result` reads the rest of the result with `drop_result()` to keep the protocol consistent. A plain console `SELECT` outside a transaction now carries the optimizer hint `SET_VAR(sql_select_limit = skip + limit + 1)`, added only when the parser sees exactly one query statement and nothing the server could read differently (locking clauses, `INTO`, variables, executable comments, user functions). Table pages, MCP requests and PostgreSQL are unchanged; MariaDB and older servers ignore the hint. Measured through a byte-counting proxy, first page of 500: 10,000 rows 2.57 ms and 1.1 MB → 0.54 ms and 55 KB; 1,000,000 rows 421 ms and 112 MB → 0.92 ms and 55 KB.

## F05: memory of results

Two mechanisms, separate from fetch concurrency:

- Results kept: `App::release_results_over` keeps decoded results of all tabs within 1 GiB. Over it, the clean results of the tabs unused for longest go back to their files in `session-results` and are read again when the tab is shown. The active tab, running tabs, results with pending changes or pending actions, and rows not written to a session file yet (including results of opened files, which are not saved) stay in memory. Test `results_over_the_memory_budget_go_back_to_their_files`.
- Results in transit: the backend counts decoded bytes of results sent and not taken by the UI (`backend::PendingResults`). Over 256 MiB, a script waits before its next statement; Stop ends the wait. The UI never blocks. Test `a_script_waits_while_the_ui_has_not_taken_its_results`.

Limits: the results of the active tab are not released, so one script returning many large results keeps them while its tab is shown. Each running query may hold up to one 64 MB page while it is read; the number of concurrent queries is not limited. Stop during the wait is reported like Stop during a statement. Peak RSS of many wide tabs was not measured in the application.

## F06: session restore

The session reader returns only metadata; the rows of a tab are read on a worker when the tab is first shown, starting with the active tab. Results not read yet keep naming their files, so autosave keeps those files. Sessions with rows inline from 0.2.0 still restore. Benchmark `benchmark_session_save`, 30 tabs × 500 rows × 20 columns: reading the session 0.3 ms, restoring on the UI thread 0.2 ms, rows of the active tab shown after 11 ms; peak RSS of the benchmark process 69 → 44 MB. Test `restored_results_are_read_when_their_tab_is_shown_and_keep_their_files`.

## F07: previews

Import samples are read through a reader limited to 8 MiB, so one huge field is cut before the CSV parser finishes the record; a cut record is dropped from type detection, and preview fields are shortened to 256 characters with a notice. The export preview shortens values when it is built, caches the text with line offsets and draws only visible lines. The real import and export read and write full values. Export preview of 10 rows with 7 MB text and 4 MB binary cells: 541.6 ms and 173.9 MB of text before, 0.15 ms and 6,953 B after; a file with a 200 MiB field is previewed after reading at most 8 MiB.

## F08: import cancellation

Connection, `BEGIN`, `DELETE`, batches and sub-batches run through a stoppable wrapper that sends a cancel request on Stop and abandons the statement after 2 s (closing the connection, so the server rolls back). Splitting a failing batch checks Stop before each attempt, and the file reader stops between records. The final `COMMIT` is not interrupted. A write waiting on a row lock ended 6 ms (PostgreSQL) and 11 ms (MySQL) after Stop with the table empty; 2,000 bad records through a delaying proxy ended 117 ms after Stop. Before, none of these ended within 10-30 s.

## F09: abandoned MySQL connections

`mysql_async` 0.37.1 starts a detached cleanup that reads the pending result without a time limit when a connection is dropped. All MySQL connections, including the one used for `KILL QUERY`, now close like the MCP variant: `disconnect` limited to 1 s, after which dropping the stream closes the socket. With the `KILL` connection blocked, Stop during a 10^9-row result: before, the socket was still open after 4 s and 512 MB more were read; after, the socket closes after 2.0 s and the server session ends. Repeated three times without growing connections.

## F10: undo history

The history kept up to 100 full versions with no byte limit, and copied the whole text on every frame with a change. It now copies text only when an undo point is created (after 1 s without changes or every 30 s of continuous typing) and keeps at most 100 points and 16 MiB in undo and redo together, always keeping one undo step. Tests cover owned bytes, order of undo and redo, a document larger than the limit, and typing without copies until a pause.

## F11: value panel cache

The cache lives in the egui context. Besides the two-view limit, prepared views now stay within 128 MiB (the newest view is kept), and a result releases its views and stops its worker when it is dropped or its value panel is hidden (`value::release_hidden`, `Drop for ResultSet`). Tests `closing_a_result_or_hiding_its_panel_releases_prepared_views` and `prepared_views_stay_within_their_byte_budget`. The JSON value tree and the parsed document coexist while the tree is built.

## F12-F15: visual tools

| View | Scenario | Before (CPU p50) | After (CPU p50) |
|---|---|---|---|
| Map | Line of 2,000,000 points, idle / hover / pan | 7.36 / 27.2 / 28.4 ms | 0.024 / 0.05 / 0.25 ms |
| Map | Polygon of 50,000 points, idle | 0.73 ms | 0.007 ms |
| Depth View | 5,000 rows × 60 time columns, first frame | 227.5 ms | 0.06 ms (analysis on a worker) |
| Depth View | Button check, 400 NULL-only columns | 16.1 ms | 0.33 ms |
| Database Graph | 4,900 objects in view, hover | 6.7 ms | 0.29 ms |
| Database Graph | Few objects in view, orbit | 46.4 ms | 0.73 ms |
| Files | Folder of 5,000 files, per frame | 16.0 ms | no file system access |

The map keeps the drawn layer while only the pointer moves, draws the selected feature once on top, and clips features crossing the view in world coordinates, with blocks of 256 coordinates skipped as a whole. Depth View decides by the number of cells to parse (10,000) and cancels replaced analyses, releasing their rows. The graph no longer rebuilds its mesh on hover and skips objects off the canvas while keeping edges that cross it. The Files panel lists folders and reads sizes in the background (at most 2 at once), draws only visible rows and starts a search 250 ms after typing, cancelling the previous one. These measurements were taken while the host ran parallel builds; p95 and p99 are noisy.

## F16: completion catalog

Completion reads the catalog on a short-lived connection of its own, grouping rows as they arrive, so expanding the explorer no longer waits for it (MySQL test: children before completion). The names of schemas and tables are sent first and the columns follow on the same connection. 3,000 tables × 40 columns in the local PostgreSQL container: names after about 11 ms, columns after 165-243 ms (three runs).

## F17: table pages

The primary key that orders table pages is cached per session and table; DDL, statements without a result set, errors, reconnects and Stop clear it, and an entry expires after 60 s. Saving edits still validates the key from fresh metadata. A proxy counted 4 key queries for 4 pages before and 1 after; one key query cost 17.5 ms with 2 ms one-way latency.

Paging by the last key was not adopted. A page at offset 990,000 of 1,000,000 rows costs 53.5 ms (PostgreSQL) and 124.6 ms (MySQL) with `OFFSET` against 0.8-0.9 ms by key, but the grid only offers previous and next page, so such an offset needs about 2,000 clicks at 500 rows per page. Keyset paging would need cursor state in the grid or wrapping the user's SQL in a derived table, which would lose column origins needed for editing.

## F18: large editor

Measured at low host load, 1 MB of SQL (27,237 lines), CPU without AccessKit: idle 5.6 ms, scrolling 5.5 ms, typing after a pause 38.5 ms; with AccessKit idle is 33 ms, so the accessibility tree built by egui dominates when a screen reader is active. The layout benchmark split typing into highlighting (15.6 ms) and laying out the text. The highlighter made a section for every space and punctuation character and an uppercase copy of every word; adjacent tokens of one format now share a section and keywords are compared without allocating: highlighting 15.6 → 8.1 ms, cache keep-alive 7.5 → 5.5 ms, rebuild after a typed character 48.6 → 31.9 ms, whole typing frame 38.5 → 36.0 ms. The remaining cost is egui laying out the whole text after a change and `TextEdit` copying the text several times per focused frame; reducing it needs incremental layout or a change in egui. The 100 kB case stays at 0.6 ms idle and 8.4 ms typing.

## Not verified

- GPU time, presentation and process RSS of the full application with many tabs.
- TLS tests (they need `scripts/test-tls.sh`).
- MariaDB behaviour of the MySQL page hint.
- A CSV of 1 GB (disk space during the work).
