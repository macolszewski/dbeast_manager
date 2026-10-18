# Performance

Speed is the reason DBeast exists: it should start instantly, use little memory and scroll large results smoothly. This page lists the performance goals, how to measure them yourself and every measurement taken so far.

## Goals

- The window appears in under 0.5 s.
- Results and the editor scroll at 60 fps.
- Memory use stays low with many open tabs.
- An idle window uses no CPU.

How these goals are met:

- The interface thread only draws. Every database, network or disk operation that may take time runs in the background and reports back as an event.
- Everything is loaded lazily: the Explorer tree level by level, results in pages (500 rows by default), completion metadata once per connection.
- Grids, trees, lists and logs draw only the visible rows. Work proportional to the number of rows (sorting, serialization, indexing) is done once and cached, never repeated on every frame.

The verification of the 0.3.0 audit, with the measurements behind each fix, is in [Performance and memory audit of 0.3.0](../design/performance-audit-0.3.md).

Most measurements below are micro-benchmarks of a single stage, without GPU rendering. They show the cost of that stage; they are not a frame-rate measurement of the whole application. Unless stated otherwise, they were taken on Apple Silicon in a release build.

## Measuring startup, memory and CPU

Time to first frame is printed when `DBEAST_TRACE_STARTUP=1` is set. Use a temporary configuration so your real one is not touched:

```sh
cargo build --release
DBEAST_CONFIG="$(mktemp -d)/config.json" DBEAST_TRACE_STARTUP=1 ./target/release/dbeast
```

Memory (RSS in KB) and CPU usage of the running process:

```sh
ps -o rss=,%cpu= -p <pid>
```

## Startup and memory

Apple Silicon, empty configuration, 16 October 2026.

| Metric | Result |
|---|---|
| First frame, warm start (5 runs) | 75-95 ms |
| Memory (RSS) | about 100-107 MB |
| CPU with an idle window | 0% |
| Executable size | 50.1 MB |

On macOS DBeast renders through wgpu with Metal, compiles only the Metal backend, requests the integrated GPU and keeps a single frame in the queue. Before those optimizations, the first Metal build was measured in 3 runs: window after 306-542 ms (about 195 ms with the earlier OpenGL renderer), memory about 106 MB (about 115 MB with OpenGL), executable 49 MB (43 MB with OpenGL). Metal was chosen because the OpenGL renderer covered the macOS window buttons in the integrated title bar. Linux keeps OpenGL.

## Results grid

### Paging on the server

Measured against the PostgreSQL 17 test container on Apple Silicon, debug build, best of five runs, page size 500:

| Query | Read to the end | Limited on the server |
|---|---|---|
| Console `SELECT` of 2,000,000 rows | 974 ms | 25 ms (cancel request after the page) |
| Console `SELECT` of 600 rows | 0.8 ms | 1.7 ms (cancel request after the page) |
| Table page at offset 0 of 20,000 rows | 6.2 ms | 1.9 ms (`LIMIT 501 OFFSET 0`) |
| Table page at offset 19,800 of 20,000 rows | 13.0 ms | 1.2 ms (`LIMIT 501 OFFSET 19800`) |
| Table page past the end | 13.3 ms | 1.0 ms |

```bash
cargo test --test drivers postgres_paging_cost -- --ignored --nocapture
```

"Read to the end" is the time until the page is shown and the session is free for the next statement when the rest of the result is read and discarded. A cancel request costs about one millisecond locally and a few network round trips to a remote server, which is why a small result gains nothing from it; the gain grows with the size of the rest. See [Paging](../guide/results-grid.md#paging).

A table page already ends one row after the page through its own `LIMIT`, so since 18 October 2026 that row is read instead of cancelled: no cancel connection and no extra transaction-state query. Release build, best of 5, page of 500 out of 20,000 rows: 3.772 ms with a cancel request and 1.060 ms read to the end at offset 0; 3.003 and 2.623 ms at offset 19,800; 2.120 and 2.662 ms past the end, where no row follows and nothing is cancelled anyway (host under parallel load). The primary key used for the page order is still read before every page: 1.861 ms locally, one round trip to a remote server.


### Layout preparation

Micro-benchmark of layout preparation, 1,000 repetitions, two columns, 14 October 2026. "Before" is the earlier algorithm for column widths and the row list; "after" uses cached widths and index access.

| Rows | Normal layout before / after | Transpose before / after |
|---|---|---|
| 500 | 5.85 / 0.04 µs | 34.96 / 6.49 µs |
| 5,000 | 4.23 / 0.04 µs | 365.12 / 66.71 µs |
| 100,000 | 27.46 / 0.04 µs | 7.12 / 1.50 ms |

```sh
cargo test --release --lib layout_preparation_benchmark -- --ignored --nocapture
```

This is a supporting measurement without UI drawing and GPU. The normal grid no longer builds a list of all records on every frame. Both grids build only the columns in view, so the work follows the viewport: in a 1000×500 view, Transpose of 5,000 records with 20 fields went from 100,000 cells and 252 ms to 300 cells and 1.6 ms per frame, and a result of 2,000 columns from 120 ms to 1.3 ms (`grid_frame_cost_benchmark`, release, 18 October 2026). The history menu processes entries only when it is opened, without cloning full queries on every frame.

### Find on Current Page

Two columns (number and text), 10% of rows contain `Łódź`, pattern `łódź`, Filter rows on, 14 October 2026.

| Rows | First index build | Cache check and three index reads |
|---|---|---|
| 500 | 0.286 ms | 43.9 ns |
| 5,000 | 1.824 ms | 53.0 ns |
| 100,000 | 37.517 ms | 54.8 ns |

```sh
cargo test --release --lib ui::results::find::tests::benchmark_page_find_cache -- --ignored --nocapture
```

The build was measured once per size; the cache cost is the mean of 100,000 repetitions of a freshness check plus reading a visible row, its position and its highlight. This measures the real grid index without drawing and GPU. Indexing 100,000 rows takes longer than one frame budget, which is one reason the default page is limited to 500 rows.

The KMP pattern is compiled once per rebuild. Text and decimal values are searched without copying; numbers and binary values are formatted only during a rebuild. Idle frames use the index without scanning the page. Match order is refreshed after sorting or Transpose. Large pages in Transpose and locating a manually selected cell in a very large index remain to be optimized.

### Local column filters

Two text columns, two AND conditions, 10% of rows match, 14 October 2026.

| Rows | Index rebuild | Cache check and three index reads |
|---|---|---|
| 500 | 41.21 µs | 62.69 ns |
| 5,000 | 85.04 µs | 20.82 ns |
| 100,000 | 1.623 ms | 21.51 ns |

```sh
cargo test --release --lib ui::results::local_filter::tests::benchmark_local_filter_cache -- --ignored --nocapture
```

The build was measured once; the cache cost is the mean of 100,000 repetitions of a freshness check plus reading a row, its position and its membership in the result. Differences of a few tens of nanoseconds include measurement noise and warm-up.

Conditions compare borrowed values without copying text. The list of distinct values is built only when a column filter is opened, and drawing it is virtualized. Long value previews are prepared only on hover. A change of filter, data or order invalidates the index; idle frames do not recompute it.

### Selection aggregates

Mean of 20 runs, all 10 metrics including median and coefficient of variation, 14 October 2026.

| Rows | Int, 1 column | Decimal, 1 column | Float, 1 column | Text, 1 column | Mixed, 6 columns |
|---|---|---|---|---|---|
| 500 | 48.69 µs | 80.64 µs | 9.10 µs | 0.42 µs | 73.16 µs |
| 5,000 | 240.90 µs | 242.21 µs | 35.19 µs | 4.06 µs | 212.17 µs |
| 100,000 | 2.880 ms | 5.434 ms | 0.637 ms | 0.041 ms | 4.926 ms |

```sh
cargo test --release --lib aggregate::tests::benchmark -- --ignored --nocapture
```

The mixed data has Int, Decimal, Float, Text, NULL and Bool columns, so the last sample covers 600,000 cells. Creating the data is outside the measurement; input values are borrowed, and only the numbers needed for the median are kept. This is a single local measurement of the computation without UI and GPU; small samples are sensitive to warm-up. A separate test confirms that nothing is recomputed while idle or while the end of a dragged selection stays still.

## SQL editor

### Find in the console

50 repetitions on SQL with Unicode characters, pattern `customer_id`, 14 October 2026.

| SQL size | Case-insensitive | Match Case + Words |
|---|---|---|
| 100,015 bytes, 2,410 matches | 0.65 ms | 0.27 ms |
| 1,000,067 bytes, 24,098 matches | 6.06 ms | 2.72 ms |

```sh
cargo test --release --lib search_large_sql_benchmark -- --ignored --nocapture
```

KMP search is linear in the length of the text and the pattern, without an extra dependency or a lowercase copy of the whole text. The match index is refreshed after a change of SQL, pattern or options; after Find is closed, only at the next search. Highlights use cached positions and pick matches on visible lines. This measures indexing only, without editor layout, rendering and GPU.

### Full CPU frames

Comparison with commit `c4d0dc8`, 1440×900 window, scale 1×, scripts with Unicode characters: 100,150 bytes / 2,757 lines and 1,000,116 bytes / 27,237 lines. 20 samples after warm-up, 14 October 2026. These measurements cover frame construction by egui, without tessellation, GPU rendering or presenting the window.

Without the AccessKit accessibility tree, active editor, time of `Context::run_ui`:

| SQL | State | Median before / after | p95 before / after |
|---|---|---|---|
| 100 kB | idle | 8.434 / 0.635 ms | 9.005 / 0.639 ms |
| 100 kB | scrolling | 8.937 / 0.611 ms | 10.408 / 0.668 ms |
| 100 kB | first keystroke after a pause | 11.774 / 4.512 ms | 12.597 / 4.873 ms |
| 1 MB | idle | 440.493 / 6.400 ms | 461.761 / 7.138 ms |
| 1 MB | scrolling | 443.300 / 6.191 ms | 466.765 / 6.798 ms |
| 1 MB | first keystroke after a pause | 466.401 / 42.993 ms | 480.807 / 45.332 ms |

Typing was measured in five samples, each after three idle frames. The inserted character is a space, so completion is not measured as well. The configuration is saved after edits pause, so the typing frame only marks it changed; the correctness of the saved SQL is checked outside the measurement.

With AccessKit and the test accessibility tree, time of a whole test step for 1 MB of SQL:

| State | Median before / after | p95 before / after |
|---|---|---|
| idle | 465.940 / 37.175 ms | 487.516 / 39.560 ms |
| scrolling | 501.061 / 72.617 ms | 553.736 / 99.205 ms |
| Find open | 468.218 / 36.832 ms | 503.314 / 37.925 ms |
| everything folded | 897.866 / 17.222 ms | 983.935 / 18.356 ms |
| scrolling folded SQL | 898.942 / 25.141 ms | 921.864 / 27.253 ms |

```sh
cargo test --release --test ui_editor_performance -- --ignored --nocapture --test-threads=1
```

The first UI construction for 1 MB of SQL with AccessKit dropped from 724 to 267 ms. This is measured inside the application's UI function, not as application startup. The accessibility tree remains expensive in egui, so measurements without it do not prove smoothness with a screen reader. GPU time and process memory were not measured here.

The cache keeps a single text layout and compact indexes of line starts and fold offsets; it does not map every character to a source position. SQL analysis, highlighting and assembling the visible text happen only after changes. The current egui API requires copying and hashing the cached layout job once per UI pass to keep its paragraph cache; since 18 October 2026 adjacent tokens of one format share a layout section, which made this keep-alive 7.5 → 5.5 ms and highlighting 15.6 → 8.1 ms for 1 MB of SQL (`editor_layout_cost_benchmark`). The standard egui text editor also makes its own copies of the text on focused frames, and a change still lays out the whole text: typing in 1 MB of SQL takes about 36 ms of CPU, 8.4 ms for 100 kB. The configuration and the session are saved in the background (see [Configuration and password storage](#configuration-and-password-storage) and [Session saving](#session-saving)).

## Database Explorer

Medians of 7 warm samples, expanded source, schema and tables folder, deterministic ASCII and Unicode names, 14 October 2026.

| Tables | Index rebuild | Speed Search `fa9` | Speed Search `ż東9` |
|---:|---:|---:|---:|
| 500 | 0.033 ms | 0.101 ms | 0.094 ms |
| 5,000 | 0.136 ms | 0.562 ms | 0.538 ms |
| 100,000 | 2.797 ms | 12.366 ms | 11.789 ms |

```sh
cargo test --release --lib explorer::navigation::tests::benchmark -- --ignored --nocapture
```

The measurement covers the index and the search projection, without frame drawing and GPU. With unchanged data, search uses the cache and does not resolve tree nodes again; a test with a resolver that would abort the measurement if called confirms this.

## Console context

Local containers, 14 October 2026. The benchmark compares `SELECT 1` with and without capturing the console context on the same connection: 5 warm-up pairs, median of 7 samples of 50 queries each, alternating order.

| Database | Without context | With context |
|---|---:|---:|
| PostgreSQL | 0.372 ms | 0.588 ms |
| MySQL | 0.214 ms | 0.395 ms |

```sh
cargo test --release --lib context_tests::benchmark -- --ignored --nocapture
```

PostgreSQL reads the context in parallel with the existing statement preparation. MySQL needs an extra `DATABASE()` read, which is one more round trip, so on a remote database the cost depends on network latency. The final measurement includes temporary namespace detection and was taken while the host was under parallel load. This measures the driver, not a UI frame.

Search in the schema selector: rebuilding for 1,000 Unicode names took 0.216 ms and for 10,000 names 1.719 ms, averaged over 100 runs. An unchanged frame checks only the cache flag.

```sh
cargo test --release --lib benchmark_schema_catalog_cache -- --ignored --nocapture
```

## Editing console results

One-time mapping of column origin and full primary key check, 5 columns, median of 9 runs after 3 warm-ups, 14 October 2026:

| Rows | Time |
|---:|---:|
| 500 | 0.024 ms |
| 5,000 | 0.139 ms |
| 100,000 | 3.610 ms |

```sh
cargo test --release --lib benchmark_result_edit_resolution -- --ignored --nocapture
```

Keys are compared by reference, without serializing or copying records, and the check does not run while scrolling.

Backend response times, medians of 7 runs, without UI and GPU. Opening the connection, the SELECT and building the batch are outside the Submit time. Submit covers the session check, the table structure lock, fresh metadata, `SAVEPOINT`, `UPDATE` and `COMMIT`. The final run was taken under parallel host load.

| Database | Metadata, cold | Metadata, cached | Submit 1 row | Submit 100 rows |
|---|---:|---:|---:|---:|
| SQLite | 0.072 ms | 0.009 ms | 0.309 ms | 1.138 ms |
| PostgreSQL | 1.966 ms | 0.150 ms | 2.773 ms | 17.648 ms |
| MySQL | 2.547 ms | 0.085 ms | 4.272 ms | 12.229 ms |

```sh
cargo test --release --test backend console_update_metadata_and_submit_latency -- --ignored --nocapture
```

The same benchmark before and after pipelined PostgreSQL Submit and single-task SQLite Submit, release build, 18 October 2026, both runs under parallel host load (medians, Submit of 100 rows): SQLite 2.639 → 0.850 ms, PostgreSQL 23.132 → 7.727 ms. MySQL, whose path did not change, measured 25.229 and 31.264 ms, which shows the noise of that host.

The PostgreSQL cache keeps up to 128 prepared `UPDATE` templates; a session keeps metadata for up to 256 tables. A cached metadata read checks the context but does not query the catalogs again. Scrolling never fetches metadata. PostgreSQL sends up to 32 `UPDATE` statements of one Submit before reading their results, so a remote server costs about one round trip per 32 changes instead of one per change: 100 changes through a local proxy with 2 ms one-way delay took 906.6 ms one at a time and 103.8 ms pipelined (release build, best of 5, `postgres_update_batch_latency`, 18 October 2026). SQLite runs the whole Submit in one background task with cached statements. MySQL still sends one statement at a time, so there its cost grows with network latency.

## Related Rows

Local databases, medians of 7 runs after warm-up, 14 October 2026. Time from sending the request to receiving the backend response, without UI and GPU. Opening connections, the source SELECT and capturing the key are outside the measurement. Reading related rows also verifies the foreign key on both the source and target sessions, binds parameters and decodes the result.

| Database | Outgoing FKs, cold / cached | Incoming FKs, cold / cached | Read parent, 1 row | Read children, 500 rows |
|---|---:|---:|---:|---:|
| SQLite | 0.093 / 0.014 ms | 0.083 / 0.014 ms | 0.137 ms | 0.190 ms |
| PostgreSQL | 2.089 / 0.147 ms | 2.577 / 0.096 ms | 4.868 ms | 5.621 ms |
| MySQL | 1.314 / 0.180 ms | 1.583 / 0.153 ms | 2.111 ms | 2.820 ms |

```sh
cargo test --release --test related_rows related_metadata_and_bound_read_latency -- --ignored --nocapture
```

SQLite has to scan the catalog on the first lookup of incoming foreign keys. With 500 extra empty tables this took 1.648 ms cold and 0.027 ms cached; with 5,000 tables, 14.902 ms and 0.018 ms. The work runs in the background. MySQL reads narrowed metadata without repeatedly joining `information_schema` views. The relation cache holds up to 256 entries per session and is invalidated after changes or a reconnect.

Related Rows pages are limited on the server (`ORDER BY` key, `LIMIT` page size + 1, `OFFSET`). Previously the whole relation was read and the client skipped rows. Best of 5 runs, release build, 100,000 children of one parent, 500-row page, 18 October 2026, time until the page is decoded:

| Database | First page, whole relation / server window | Page at 50,000 | Page at 99,500 |
|---|---:|---:|---:|
| SQLite | 0.123 / 0.154 ms | 3.837 / 5.987 ms | 10.539 / 13.795 ms |
| PostgreSQL | 21.446 / 14.857 ms | 42.319 / 31.988 ms | 40.433 / 35.796 ms |
| MySQL | 71.855 / 1.284 ms | 78.535 / 29.857 ms | 76.541 / 59.262 ms |

SQLite already stopped reading after the page, so the sort by key adds a little there. PostgreSQL and MySQL no longer send the rest of the relation over the network; deep pages still cost an `OFFSET` scan on the server.

```sh
cargo test --release --test related_rows related_paging_cost -- --ignored --nocapture
```

Preparing Explain for a Related Rows console (validating the saved relation and source identity, generating SQL with two parameters and wrapping it in `EXPLAIN`) took on average **3.407 µs per request** over 100,000 repetitions, CPU only (`benchmark_captured_explain_preparation`, 15 October 2026). The check for local PostgreSQL encoding errors before execution scans the text without copying it: **0.008 µs** for 32 B of text and **54.944 µs** for 1 MB (`benchmark_explain_parameter_preflight`, 10,000 repetitions). This cost is paid once per request and depends on the total length of text keys.

## Query plans

### Explain Plan

15 October 2026, median of 7 runs after warm-up. Normalizing a synthetic PostgreSQL plan (reading JSON, nodes, properties, the Raw line index, and the diagram layout and index) for 101 / 1,001 / 10,001 operations (12 kB / 122 kB / 1.23 MB) took 0.112 / 1.082 / 11.362 ms. The input is created before the measurement; the parser runs in the background.

| View nodes | Cache preparation on the UI thread | Idle CPU frame | Collapse and expand all |
|---|---:|---:|---:|
| 100 | 0.0013 ms | 0.178 ms | 0.0002 ms |
| 5,000 | 0.053 ms | 0.191 ms | 0.007 ms |
| 100,000 | 1.268 ms | 0.212 ms | 0.213 ms |

The table uses simple nodes without extra properties. A separate variant with 100,000 nodes with objects, costs and 600,000 properties prepares its cache in 3.746 ms. Frames measure UI construction without AccessKit and GPU rendering. Scrolling is virtualized, and the operation index is rebuilt after expanding or collapsing, never by scanning the whole plan on idle frames.

```sh
cargo test --release --lib query_plan -- --ignored --nocapture --test-threads=1
```

### Explain Analyse

15 October 2026, median of 7 runs after warm-up, run sequentially. Normalizing a synthetic PostgreSQL plan with actual metrics for 101 / 1,001 / 10,001 operations (21 kB / 209 kB / 2.1 MB) took 0.223 / 2.278 / 22.494 ms, compared with 0.239 / 2.079 / 18.518 ms for the same document in plan mode. Metrics are computed once on the worker in a separate vector, so a plain plan does not pay for them in memory.

A view of 100,000 operations with metrics prepares its cache in 1.316 ms (1.310 ms without metrics), and an idle CPU frame takes 0.252 ms (0.204 ms without metrics). The difference comes from the extra table column; values are not formatted during the frame. Example from the UI test (an `UPDATE` of 4 out of 200 rows, PostgreSQL 17 in a container): client time 4.5 ms, Planning Time 0.195 ms, Execution Time 0.139 ms.

```sh
cargo test --release --lib -- --ignored --nocapture --test-threads=1 benchmark_query_plan_normalization benchmark_plan_view_cache
```

### Explain Analyse on MySQL

15 October 2026. For a synthetic TREE of 101 / 10,001 operations, parsing metrics and building the tree and diagram layout took on average 0.110 / 7.757 ms over 20 runs, without UI and GPU. The work is linear in the number of operations and bytes of text, done once on the worker. The view uses the same cache and virtualization as PostgreSQL.

Local MySQL 8.4.11 in a container, `SUM` over 1,000 rows of a temporary table, warm session, median of 11 measurements after 3 warm-ups: 0.464 ms for a normal execution and 1.097 ms for Explain Analyse. The second figure covers the backend, reading context and transaction state, the parser, the layout and the final event, without UI drawing. It is not the server's Execution Time.

```sh
cargo test --release --lib benchmark_mysql_tree_normalization -- --ignored --nocapture --test-threads=1
cargo test --release --test explain benchmark_mysql_analyse_execution -- --ignored --nocapture --test-threads=1
```

### Plan diagram

15 October 2026, median of 7 runs after warm-up. Layout and index for 100,000 operations:

| Plan shape | Time | Retained buffers |
|---|---:|---:|
| Chain | 2.267 ms | 11.09 MB |
| Star | 0.877 ms | 2.00 MB |
| Balanced tree | 1.161 ms | 2.60 MB |
| Forest of separate roots | 0.928 ms | 2.00 MB |

Retained buffers are the summed capacity of geometry and index buffers, without the input plan, temporary build allocations or the memory of the whole process.

A query for the visible 1280×720 area takes 0.115-2.217 µs, and preparing an overview of the whole plan 0.929-4.060 µs, reusing result buffers. A still UI keeps the finished result and does not repeat the query. The overview has at most 512 groups and never skips operations because the drawing budget ran out.

CPU cost of the diagram view alone, 1100×500 area, without AccessKit, GPU and the rest of the application, at 100,000 operations: idle detail frame 0.009 ms, overview frame 0.021-0.047 ms, panning 0.015-0.027 ms, zooming 0.014-0.018 ms. Only 3-5 cards are visible, and operations outside the screen do not increase the number of drawn cards. The command is the same as for Explain Plan.

## Compare Data

14 October 2026, median of 7 runs after warm-up. Two results with 8 columns, a composite key and 1% changed rows:

| Rows | Time |
|---:|---:|
| 500 | 0.142 ms |
| 5,000 | 0.510 ms |
| 100,000 | 14.476 ms |

With all 100,000 rows changed: 15.875 ms. For 5,000 rows and 129 columns: 2.959 ms; with a 4 KiB BLOB in every record: 1.166 ms, and with that BLOB in the key: 10.408 ms. The measurement covers the full comparison and the uniqueness check, without creating the input data and without UI.

For 100,000 rows, retained indexes and bitmaps take about 5.6 MB (buffer capacity) with 1% changes and 6.65 MB with all rows changed, excluding the shared snapshots, temporary maps and the memory of the whole process. Capturing the two snapshots does not copy data; a clone/drop pair takes about 4 ns.

View with two columns: cache preparation for 500 / 5,000 / 100,000 rows takes 0.001 / 0.008 / 0.207 ms, and an idle CPU frame 0.088 / 0.091 / 0.092 ms. Two toggles of Show identical for 100,000 rows take 0.033 ms. Frames exclude AccessKit and GPU.

```sh
cargo test --release --lib compare -- --ignored --nocapture --test-threads=1
```

## DDL Diff

15 October 2026, median of 7 runs after warm-up. Line indexing, previews and comparison of two definitions with 1% changes:

| Lines | Time |
|---:|---:|
| 500 | 0.114 ms |
| 5,000 | 0.586 ms |
| 100,000 | 15.255 ms |

With all 100,000 lines changed, the simplified alignment takes 12.981 ms. A repetitive 750 kB text takes 4.685 ms, a single 1 MB line 0.926 ms, and 500 almost identical lines of about 16 KiB each (8.19 MB per side) 105.906 ms. The input is created before the measurement, and the computation runs in the background. The 250 ms budget and the cell limit apply to the detailed alignment only, not to the whole preparation. Exceeding the budget only reduces alignment detail and is shown in the interface.

View preparation for 500 / 5,000 / 100,000 lines: 0.001 / 0.006 / 0.138 ms. Idle CPU frame: 0.086 / 0.082 / 0.085 ms; frame after jumping to another difference: 0.185 / 0.184 / 0.186 ms. These include accessibility and UI construction, without GPU rendering.

Fetching two fresh definitions of tables with 101 and 102 columns from local databases, 3 warm-up runs and median of 7:

| Database | Median | First read |
|---|---:|---:|
| SQLite | 0.278 ms | 0.948 ms |
| PostgreSQL | 6.580 ms | 8.603 ms |
| MySQL | 1.977 ms | 6.284 ms |

This covers two new connections, catalog reads, alignment and receiving the event, without preparing the tables or rendering. On a remote server the time also depends on network latency.

```sh
cargo test --release --lib --test ddl_diff ddl -- --ignored --nocapture --test-threads=1
```

## Data transfer

| Operation | Result | Test |
|---|---|---|
| Export of one million rows from PostgreSQL | about 0.4 s | `large_export_streams` |
| Import of one million rows into PostgreSQL with `COPY` | about 0.8 s | `large_import_uses_copy` |

Export streams rows in batches of 1,000 through a bounded channel to a writer thread with a 1 MB buffer, so memory does not grow with the table size. Import streams the file in a separate thread and loads it in batches inside one transaction. See [Import and export](../guide/import-export.md).

## Database Graph layout

Computing one force layout (release, Apple Silicon): 100 objects in 7 ms, 1,000 objects in 0.16 s, 5,000 objects and 15,000 relations in 1.2 s. The finished picture is kept as a single mesh and redrawn only when something changes. See [Visualization](../guide/visualization.md#database-graph).

## Configuration and password storage

15 October 2026: 100 data sources with four synthetic secrets each, one in-memory vault and a real atomic write to a temporary directory.

| Operation | Result |
|---|---|
| One-time migration | 29.798 ms |
| Reload | 0.812 ms, with exactly one vault read |
| Average autosave (100 repetitions) | 8.453 ms |

Autosave covers cloning the configuration, JSON, locking, `fsync` of the file and directory, and the rename. None of the 100 repetitions performed an extra vault operation.

Since 18 October 2026 an edit only marks the configuration changed. The snapshot is taken after edits pause (400 ms, at most every 2 s while typing continues), with at most one ordinary save in flight, so the worker never receives a backlog of snapshots. Console and history text is shared with the snapshot (`Arc<str>`), and the writer encodes the file once for both the write and its revision. With 20 MiB of text in 10 consoles, the work per change on the interface thread went from 1.38 ms to 0.85 µs; a snapshot with one changed console takes 1.5 ms (`autosave_cost_benchmark`). This is a local measurement, not a performance threshold. It does not include native macOS Keychain time, system prompts, UI, GPU or window startup.

```sh
cargo test --release --lib persistence::tests::benchmark_config_and_credential_persistence -- --ignored --nocapture
```

## Session saving

The session is checked for changes every 2 seconds. `session.json` holds only metadata; the rows of each result are written once, by the writer thread with a streaming writer, into `session-results/`, and later saves rewrite only `session.json`. Nothing is written when the hash of the metadata is unchanged. Snapshots queued behind a slow write are replaced by the newest one, exit waits for the last write, and files of results no longer open are removed after a successful save. On start, the session is read on a background thread and the rows of a tab are read when the tab is first shown.

18 October 2026, release build, synthetic results of 500 rows x 20 columns per tab, medians of 21 saves, time on the interface thread:

| Tabs (rows on disk) | First save after the results arrive | Unchanged / changed | Reading the session / restoring tabs | Rows of the active tab shown |
|---|---|---|---|---|
| 10 (5.0 MB) | 0.6 ms (8.6 ms when serialized on the interface thread) | 0.019 / 0.017 ms | 0.4 / 0.2 ms | 12.7 ms |
| 30 (15.1 MB) | 0.2 ms (24.4 ms before) | 0.060 / 0.050 ms | 0.3 / 0.2 ms | 11.0 ms |

Before the rows were read on demand, restoring 30 tabs read all rows (53 ms in the background) and kept them in memory; the peak RSS of this benchmark process went from 69 to 44 MB. Results of all tabs are kept within 1 GiB in memory; above that, clean results of the tabs unused for longest are released and read again from their files when shown (see [Sessions](../guide/sessions.md)).

```sh
cargo test --release --lib ui::session::tests::benchmark_session_save -- --ignored --exact --nocapture
```

## TLS connections

Local, isolated containers started by `scripts/test-tls.sh`, debug build, five attempts each of opening a connection, reading the version and encryption state, and `SELECT 1`. Times include the driver, loopback networking, Docker, authentication, reading files and importing the native identity; they exclude UI and the production keychain.

| Database | Without TLS: median (range), ms | TLS: median (range), ms | mTLS: median (range), ms |
|---|---:|---:|---:|
| MySQL | 3.9 (3.6-4.3) | 10.8 (10.6-15.8) | 39.0 (35.8-88.8) |
| PostgreSQL | 79.2 (70.1-86.1) | 82.5 (71.9-1627.8) | 132.6 (122.0-306.0) |

The PostgreSQL samples vary widely; five measurements are not enough to judge long-term performance. The native PKCS#12 import for MySQL is internal to the driver, outside the UI.

Cancelling a query after a deliberate 300 ms wait completed after 324.4 ms (PostgreSQL) and 367.7 ms (MySQL) directly, and after 377.1 ms and 341.3 ms through SSH, also after the CA and client identity files were deleted.
