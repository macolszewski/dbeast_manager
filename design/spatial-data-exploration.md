# Research feature request: Spatial Database Graph and Depth View

**Status:** implemented: Depth View (R0/R1) as a 3D scene with an orbital camera, and Database Graph (G1, with partitions and FDW from G2) for PostgreSQL, with metadata also read for MySQL and SQLite. Behavior and limitations are described in the user documentation.  
**Date:** October 15, 2026.  
**Project:** DBeast (`macolszewski/dbeast_manager`).  
**Basis:** the project owner's concept, developed in discussion; verification of selected public sources and of the repository documentation.  
**Repository reference point checked:** `adc2feb40790370b617e15ea0dea65e85e7cf3f0` (`master`).  
**Decision requested:** approve a research prototype, not a full implementation of every option described.

> SQL builds the context and the relationships. Depth View keeps the result as a table, but lets you see time as depth, and the differences between several dates of one row as spatial connections.

## 1. Summary and scope of the proposal

Two independent, complementary views are proposed:

1. **Spatial Database Graph**: a graph of database objects: tables, views, materialized views and FDW foreign tables. Node size depends on the selected measure: row count or storage size. Edges represent documented relationships or dependencies, with their kind distinguished. The graph is for exploring the structure, scale and dependencies of a database.
2. **Depth View / Depth Timeline**: a spatial mode for viewing a finished SQL result. Columns stay columns, the vertical axis keeps the row order, and depth represents a selected date. The view also supports overlaying two or three projections of the same row based on different time columns, connected with lines.

**The main subject of the research prototype is Depth View with multiple times.** The graph has a separate scope and is not a prerequisite for it. The Depth View prototype should not depend on a complete schema scanner, column lineage reconstruction or implementing both modules at once.

This is a deliberate extension of DBeast's direction. This document does not add features to the "What works" section of the README. Only an actual implementation should describe the behavior, limitations and any deliberate deviation from the established interface conventions there.

### Requirements that follow directly from the idea

- A spatial database graph, switching the node size measure, actual relationships, FDW, views, materialized views and view configuration.
- Transition of a regular result table into depth based on a selected date column: older rows further away, newer rows closer.
- A time range slider, projections disappearing outside the filter, and a configurable step.
- Works on the result of any supported query, including JOIN, CTE, aggregation and HAVING; sorting independent of the time used for depth.
- Overlaying times from different columns: two or three spatial counterparts of one row, connected with a line.
- A particularly important use case: analyzing events in systems and looking for timing anomalies.

The other solutions in this document are **a recommended prototype contract or research questions**, not additional requirements approved by the project owner.

## 2. Problem and value hypothesis

An analyst can use SQL to join a log with a user, a service, a job and a business operation. After running the query, however, they still have to follow the row order, several dates and the event description at the same time. Sorting the table by one date changes its earlier order, for example by user. A separate chart may require drilling into details to recover the context of the whole row.

Hypothesis: **spatially offsetting the rows themselves**, without replacing them with anonymous points or bars, will help compare the business order with the temporal order. Multiple projections of a row should additionally reveal the differences between timestamps.

This is not a promise of automatic diagnosis or a proven advantage of 3D. We need to check whether users solve specific tasks faster and more accurately than with a regular table and a readable 2D chart.

Proposed use cases include post-incident investigations, queues and imports, late data delivery, retried operations and discrepancies between business and technical time. The distinction between the time of occurrence and the time of observation is a real part of the OpenTelemetry log data model (`Timestamp` and `ObservedTimestamp`); it does not, however, require DBeast to integrate with OpenTelemetry. The user can pick any suitable result columns. [S5]

## 3. Depth View mental model

### 3.1. Not a single source table, but the answer to an SQL query

Flow:

```text
User SQL
  -> execution through the existing DBeast path
  -> result and an explicit range of fetched data
  -> row order + choice of time columns
  -> Depth View
```

The visualizer does not need to know which JOINs were used. It needs the result, a stable identity of row occurrences and the values of the selected columns. The date can come from a log, the user name from another table and the event count from an aggregate.

| Dimension | Meaning |
| --- | --- |
| X | Column position in the table; column order and widths are kept. |
| Y | Current order of row occurrences, for example by user name. |
| Z | Value of the selected time column on a shared axis. |

Sorting by user must not be replaced by sorting by date just because Depth View was turned on. Without `ORDER BY` we keep the order received, but do not promise the same order after the query is run again.

### 3.2. Example: JOIN + HAVING + independent depth

The following PostgreSQL SQL is an example for a hypothetical schema, not a query run against a user's database:

```sql
WITH selected_logs AS (
    SELECT *
    FROM logs
    WHERE occurred_at >= TIMESTAMPTZ '2026-10-15 10:00:00+02'
      AND occurred_at <  TIMESTAMPTZ '2026-10-15 11:00:00+02'
),
noisy_requests AS (
    SELECT request_id, COUNT(*) AS event_count
    FROM selected_logs
    GROUP BY request_id
    HAVING COUNT(*) >= 3
)
SELECT
    l.id AS log_id,
    u.name AS user_name,
    s.name AS service_name,
    l.request_id,
    n.event_count,
    l.occurred_at,
    l.received_at,
    l.processed_at,
    l.level,
    l.message
FROM selected_logs AS l
JOIN noisy_requests AS n ON n.request_id = l.request_id
LEFT JOIN users AS u ON u.id = l.user_id
LEFT JOIN services AS s ON s.id = l.service_id
ORDER BY u.name NULLS LAST, s.name NULLS LAST, l.id;
```

The analyst keeps the ordering by user and service, chooses `occurred_at` as depth, and then adds `received_at` and `processed_at`. The whole context stays in the same row.

An aggregated result with `MIN(occurred_at)` and `MAX(processed_at)` is also allowed. In that case the projections represent **the aggregate row**, not a reconstructed single log entry. The two extremes can come from different events; the line must not pretend to be the handling time of one operation.

### 3.3. Position, window, step and stretch

The controls have separate meanings:

| Control | Contract proposed for the prototype |
| --- | --- |
| `Time position` | Moves the viewed window along the time axis. |
| `Window` | Width of the visible interval `[start, end)`. |
| `Step` | Increment when using the arrow keys or a stepwise move, for example 1 s, 1 min, 1 h. |
| `Depth scale` | Stretches the scene; does not change the values or the data range. |
| `Time columns` | Selection of one, two or three columns for projections. |

The basis is a continuous, shared time scale. Grouping into buckets should be a separate option, not a hidden effect of `Step`.

Example mapping, with positive Z pointing into the screen:

```text
z(t) = depth_scale * (end - t) / (end - start)
```

Conditions: `end > start`, a shared time unit and the same transformation for every series. Newer values are closer, older ones further away. With `depth_scale = 0` the geometry returns to a flat table; turning the view off additionally removes its local filter from the table view.

Changing the camera does not filter data. Clipping by the camera plane is a rendering matter and is not equivalent to excluding a row by date. The current interval and whether its bounds are open or closed must be visible.

## 4. Overlaying multiple times and lines between projections

### 4.1. One row, many representations

Each selected time column creates a projection of the whole row at the corresponding depth. The projections share X/Y, the same row identity and a common selection. They are visual instances, not copies of the data or new records in the database.

```text
Result row: Anna | import | request-917 | ...

occurred_at                received_at    processed_at
10:00:00                   10:00:18       10:00:19
    ●--------------------------●--------------●
                 18 s                  1 s

Illustrative 2D sketch; in the application the connection runs in depth.
```

All series use **the same absolute time axis**. Normalizing the range of each column independently is not acceptable: it would visually remove real delays.

The primary series can be solid and the others semi-transparent. Each gets a name and a distinguishable style; color alone is not enough. Clicking any projection highlights all of its counterparts and connectors, without moving the other rows.

### 4.2. Meaning and order of connections

A line means: **the values belong to the same occurrence of a result row**. It is not a foreign key, an automatically detected flow or evidence of causality.

The user sets the order of series, for example `occurred_at -> received_at -> processed_at`. We do not sort the points by date automatically, because that would hide reversed temporal order. Arrows indicating process stages are optional and require an explicit interpretation by the user.

A segment has a signed difference: `delta = timestamp_B - timestamp_A`. The tooltip shows both full dates, the column names, the unit and the delta. The measurement comes from the values, not from the on-screen length of the line, which changes with perspective.

Unsynchronized clocks in the sources limit the interpretation of the difference as a delay. We do not correct time automatically. Negative values stay visible as data to investigate.

### 4.3. Identity without the JOIN trap

We do not identify a row by `log_id` alone, by user name or by a content hash. A one-to-many JOIN, `UNION ALL` and identical values can produce many valid occurrences.

Proposed local identity: `(result_version, original_row_ordinal)`, and for a projection, additionally the column/series identifier. Sorting is a permutation of these identities, not an overwrite of them. Column names can repeat, so the mapping cannot rely on the alias alone.

Running the SQL again creates a new result version. Keeping the selection, if desired, requires a separately verified key; by default we do not automatically match rows between executions.

### 4.4. Filtering multiple projections

Proposed default mode: a row qualifies if **at least one** selected, valid date falls within `[start, end)`. Individual projections outside the window are hidden, and a connector at the boundary may have a continuation marker. We do not draw a clipped end as if it were a real event.

If both ends of a segment are outside the window, even though the segment crosses it, the default point filter does not show the row. An `Interval intersects window` mode, if added, is a separate option with its own semantics. Likewise, "primary date in window" and "all dates in window" are separate filters.

The delta is computed from the real values before clipping. A filter such as `received_at - occurred_at > 5 s` works on the finished result; a missing date yields an incomputable state, not zero. The row counter and the visible projection counter are separate: three projections do not mean three times as many events.

## 5. Interaction and readability

The prototype should keep the headers, the selection and the ability to read the full row in a pinned 2D panel. Returning to the table restores its sorting, scroll position and selection; Depth View does not write any changes to the data.

Recommended behavior:

- A smooth transition of the table into depth, with an option to reduce animation; front view, oblique view, `Fit` and camera reset.
- Fixed Y positions while moving the slider; excluded rows do not cause constant rearrangement of the remaining ones. Any compaction of the layout is explicit.
- Delta labels mainly for hovered or selected rows. The exact text stays available in the panel, even when a distant projection is simplified.
- Coinciding dates are marked as several series at the same position, for example with an outline or a `delta = 0` label; we do not add an artificial offset on the time axis.
- The histogram under the slider describes only the known range of the result. Aggregating projections into groups is clearly marked and reversible.
- Optional highlighting of rows that share a `request_id`, `trace_id`, `job_id` or an explicit composite key. This is a separate mechanism for correlating rows, not the line connecting the dates of one row.

Large results must not force all cells to be drawn at once. Simplifying distant elements can help, but the feature must not be reduced to a point cloud: a readable transition from the spatial view to the full row is part of what sets it apart.

## 6. Correctness of data and SQL semantics

### 6.1. The view filter does not rewrite the query

Depth View works on a frozen version of **the result already received**. Rotating, changing time columns, local sorting and moving the slider do not run SQL.

Moving the time filter into `WHERE` before aggregation can change both the aggregates and the groups that satisfy `HAVING`; it is not equivalent to filtering finished rows. That is why we do not automatically add predicates to the original query or rebuild its JOINs. [S6]

Fetching a larger range requires an explicit action and the existing execution path. Simply wrapping arbitrary SQL in a subquery is not the default solution either: it requires analysis of syntax, columns, parameters and the effects of re-execution. The basic prototype does not need this feature.

### 6.2. Range, completeness and refresh

The interface shows at least: the source, the result version, when it was obtained, the number of rows fetched, and whether it is a page, a fragment or a confirmed complete result. A full page with a limit does not prove completeness. Patterns and histograms refer to the fetched data, not to the whole database.

We do not silently merge pages obtained from different re-executions into a supposedly consistent snapshot. The term "result snapshot" means an immutable object in the application, not an automatic guarantee of a single transactional snapshot of all sources, especially with FDW.

A refresh invalidates the time indexes and the projection mapping. A late worker response must not replace a newer result. Uncommitted edits require the existing Submit/Revert process, or a refusal to open the view with a clear reason; opening the view by itself commits nothing.

### 6.3. Time types and missing values

Handling must distinguish `date`, time with a time zone and time without a time zone. PostgreSQL treats `timestamp without time zone` differently from `timestamp with time zone`; interpretation and presentation must preserve this difference. [S7]

Design recommendations:

- Do not assume UTC or the computer's time zone for values without a zone. For mapping onto an axis of instants, require an explicit interpretation policy and show it in the view.
- Dates without a time of day must not pretend to be a precise event time. Do not compare them with precise timestamps as equivalent without a warning.
- For numeric epochs and text dates, allow an explicit format/unit; no silent guessing of seconds versus milliseconds.
- Preserve value precision; compute GPU coordinates from differences relative to a nearby reference point, not by converting the whole epoch to `f32`.
- `NULL`, invalid text, infinity, out-of-range values and ambiguous local time need their own states. Do not convert them to 1970-01-01.
- A missing date in one series does not remove valid projections of the other series. Do not draw a segment that requires the missing value.

**The view does not reconstruct database history.** Moving the slider to May does not restore the May status of a record changed in July. We view the available values through their dates; reconstructing previous versions requires a separate history source.

## 7. Spatial Database Graph: a separate structure view

### 7.1. Nodes and measures

Nodes represent objects, not records. The target is to distinguish plain tables, partition parents and leaves, views, materialized views, foreign tables and groups of external sources.

Basic toggle: `Estimated rows` / `Storage size`. Size needs an explicit scope: data without indexes or total storage. In PostgreSQL `reltuples` is an estimate and can be unknown (`-1`), while `pg_table_size` excludes indexes and `pg_total_relation_size` includes them along with TOAST. [S8] [S9]

Each measure keeps its value, unit, source, read time and status: measured, estimated, unknown, unavailable or not applicable. The read time is not automatically the time statistics were last updated. An object statistic is not the number of rows visible to the current user after filters or row-level policies.

A plain view does not store its own result set; we do not give it a fictitious size. A materialized view stores its result and requires separate treatment of its dependencies on sources. [S13] [S14] For a partition parent, aggregation of children must be defined explicitly to avoid double-counting storage.

After the measure changes, node positions stay fixed. In a sphere model, volume proportional to the value means a radius proportional to the cube root, not to the value itself. A logarithmic scale is optional and described in the legend; unknown values do not look like zero.

### 7.2. Edge types

| Edge | Meaning and limits of interpretation |
| --- | --- |
| `Foreign key` | A declared column reference; the order of a composite FK is preserved. Not proof that all data has been validated. |
| `View dependency` | Dependency of a view definition on an object. Does not imply record identity. |
| `Materialized view dependency` | Dependency of the materialized data on a source at build/refresh time; not a direct read of the source on every SELECT. |
| `Foreign source mapping` | Configuration of a foreign table and server; not a foreign key and not a confirmation of the remote schema. |
| `Partition membership` | Membership in a partition tree, separate from FK. |

For PostgreSQL the starting point is `pg_class`, `pg_namespace`, `pg_constraint`, `pg_depend` and, for view dependencies, mapping through `pg_rewrite` rules. Dependencies have to be normalized to user objects, not drawn as the raw dependency catalog turned into an arbitrary set of edges. [S8] [S10] [S11] [S15]

"Actual relationships" means relationships confirmed by metadata within the stated scope. The validation/enforcement status should be shown when the engine exposes it. The absence of a visible edge does not prove the absence of a business relationship. The name `customer_id` alone does not justify drawing an FK.

### 7.3. FDW and scan cost

"Scanning the database" means primarily reading catalogs and available statistics, not a full `COUNT(*)` on every table. The schema scope, the number of objects and the cost of reads must be boundable; progress, cancellation and partial results are explicit.

An FDW node describes a locally known foreign table and its source. We do not assume that the remote size or the full remote dependencies are available. Extending the graph to a remote server requires separate consent, configuration and privileges. We do not reveal user mappings, passwords or full connection strings.

We do not automatically run `ANALYZE`, `EXPLAIN`, remote queries or materialized view refreshes for the sake of how the graph looks. In `postgres_fdw`, fetching estimates can use a remote `EXPLAIN`, and `ANALYZE` can read source data; the cost is not neutral. [S12]

Everything uses the privileges of the existing connection, without escalating them. Introspection is versioned per driver; we do not promise PostgreSQL catalogs for MySQL or SQLite.

### 7.4. Navigation and configuration

Required direction: search, object and edge type filters, collapsing schemas/partitions/sources, focus on neighbors and dependencies, pinning nodes, a legend, reset and saving the layout. The same graph should have a 2D and a 3D mode, without a physics simulation running constantly at rest.

Clicking an object leads to its existing definition, the explorer or an explicitly opened data view. Merely rotating or hovering over an object does not trigger a read of its contents. The schema graph and the time projection lines never share undocumented semantics.

## 8. Preliminary research into similar solutions

Scope: public documentation and authors' descriptions checked on October 15, 2026, without installing or testing these applications. This is not an exhaustive review of the market, patents or visualization literature.

| Solution | What the source confirms | Relevance to the idea |
| --- | --- | --- |
| Altova DatabaseSpy | Charts from SQL results, including 2D/3D pie and bar charts. [S2] | "SQL result in 3D" is too general a claim of novelty. |
| ColumnLens | SQL with JOIN/CTE/aggregation, a 3D City View that turns rows into buildings, column mapping and a time slider. [S3] | A close direction; the description does not confirm a preserved grid with several connected projections of the same row. |
| Schema3D | Tables as 3D objects and FK connections based on a schema description. [S4] | A 3D schema graph has predecessors; what needs evaluating is database integration, metrics and usability, not 3D alone. |

**Conclusion:** these sources do not confirm the exact full set of proposed interactions. That does not prove it does not exist elsewhere. Do not state "world first" as a fact.

Candidate differentiator: **the same SQL result, preserved cells and independent sorting, time as depth, several projections of one row on a shared axis, and explicit connections and deltas**. The research should be extended to spatial tables, temporal visualizations, parallel coordinates views and tools for traces and process analysis. They are not assumed in advance to be either equivalents or irrelevant.

## 9. Fit with the DBeast architecture

According to [AGENTS.md](https://github.com/macolszewski/dbeast_manager/blob/master/AGENTS.md), the interface uses egui/eframe, expensive operations go through the backend, results are paged, and rendering must avoid work proportional to the whole data set on every frame. [Cargo.toml](https://github.com/macolszewski/dbeast_manager/blob/master/Cargo.toml) declares the `glow` backend. The [README](https://github.com/macolszewski/dbeast_manager/blob/master/README.md) describes Geo Viewer, Related Rows, Compare Data snapshots and the plan diagram with layout computed outside the UI. These are points to investigate and possibly reuse, not evidence that a ready Depth View engine exists.

Proposed division of responsibilities:

| Element | Responsibility |
| --- | --- |
| Result adapter | Immutable access to the data, range, columns and occurrence identities. |
| Temporal model | Interpretation of time types, deltas, indexes and selection of visible projections. |
| View state | Series, sort permutation, window, camera, selection and legend. |
| Renderer | Drawing and hit testing; no SQL and no reinterpretation of values. |
| Graph provider | Separate introspection, metric provenance and typed dependencies. |

These are names of responsibilities, not declared existing Rust types. Integration should be checked in `src/model.rs`, `src/backend.rs`, `src/ui/results.rs`, `src/ui/mod.rs`, the drivers and the existing views.

In the first experiment, compare a limited 2.5D projection in the current UI with custom GPU rendering. Do not migrate the whole stack or add a browser or a heavy engine just because the word "3D" came up. Connections and time parsers must be testable independently of the graphics.

## 10. Performance, security and costs

Row data is shared; two additional projections do not clone its text and JSON values. Time parsing, sorting and index preparation happen when the data version or configuration changes, not on every frame. Filtering can use time indexes, and the renderer can use a selection of visible rows and columns.

Expensive tasks have cancellation, a bounded queue and result version checks. A closed view does not keep its own animation, polling or graph layout running. A static scene does not request continuous repaints without need.

Overload must not be fixed by silently dropping rows. The scene limit, grouping and level of detail must be visible, with access to the full fetched data in the table.

No automatic data export, no content telemetry and no sending of results to an external renderer. Saved presets should by default contain configuration, not an extra copy of the data, SQL with secrets or connection parameters. The read-only view does not extend the write permissions of the existing grid or MCP permissions.

**Targets to verify, not measurement results:** no new queries during interaction, no regression of the regular table after the module is closed, smooth interaction and bounded memory overhead. For an agreed machine and reference scene, a target of p95 frame time up to 16.7 ms can be adopted; overruns should be documented, not hidden behind a "60 fps" claim.

## 11. Proposed stages and MVP scope

| Stage | Scope | Exit criterion |
| --- | --- | --- |
| R0: interaction prototype | Frozen result, one date, independent order, smooth offset into depth, slider, detail panel and return to 2D. | Research tasks can be carried out without new SQL and without losing rows. |
| R1: multi-time core | Two/three series, shared scale, correct lines, deltas, missing values, clipping, selection. | Identity and time criteria met, plus an initial usability study. |
| R2: production integration | Driver time types, limits, cancellation, large results, UI tests, accessibility and documentation. | Measurements and no regression of existing flows. |
| G1: separate graph prototype | PostgreSQL, local metadata, explicit FKs and dependencies, available measures, 2D/3D. | Correct legend, unknown measures, cost control and navigation. |
| G2: graph extensions | Partitions, richer FDW, presets and further engines. | Capabilities of each provider verified separately. |

R0 is not the final replacement for the idea. **R1 is essential for testing the multi-time differentiator.** The order of G1 relative to R2 remains a product decision, not a technical dependency.

Outside the first MVP: reconstructing data version history, live streaming, automatic root cause detection, guessing FKs, full lineage of arbitrary SQL, automatic query rewriting, bulk data scanning, automatic clock correction, editing data from 3D and joining arbitrary independent results without an explicit key.

## 12. Studies and acceptance criteria

### 12.1. Scenarios with synthetic data

| Scenario | Pattern to find | Limits of inference |
| --- | --- | --- |
| Batch delivery | Spread `occurred_at`, nearly identical `received_at`. | Does not prove a queue failure; only confirms the arrangement of dates in the result. |
| Growing processing time | Increasing difference `processed_at - received_at`. | The cause requires further analysis. |
| Common moment of errors | Similar depth of errors across many services/users. | Co-occurrence does not imply causality. |
| Job retries | Repeating intervals for the same correlation key. | The view does not assume on its own that these are retries. |
| Reversed times | Negative delta between declared series. | The clock, data quality or field semantics need checking. |
| JOIN fan-out | The same `log_id` in several valid rows. | Projections connect by row occurrence, not by `log_id`. |

Compare the same data in: a table with delta columns, a simple 2D time chart and Depth View. Varying the task order will limit the learning effect. Measure accuracy, time to reach a conclusion, the number of mistakes and how easy it is to recover the full context. Aesthetic assessment is separate from usability.

The hypothesis moves forward when the view gives a noticeable benefit in at least some tasks without reducing accuracy. If 3D gets in the way, the interaction or scope must be corrected; the visual effect alone must not be counted as success.

### 12.2. Depth View acceptance checklist

- [ ] A JOIN/CTE/HAVING result keeps its values and number of occurrences; opening the view does not run SQL.
- [ ] Sorting by `user_name` works independently of depth based on log dates; the order is shared with the table.
- [ ] Two/three dates create the corresponding number of projections on one scale; identities do not mix after sorting and JOIN fan-out.
- [ ] Selecting any copy selects all counterparts; exporting records does not multiply them by the number of projections.
- [ ] Deltas are exact, signed, and independent of the camera and clipping.
- [ ] The `[start, end)` window, step and stretch are independent; bounds and row/projection counts are explicit.
- [ ] NULL, identical dates, reversed dates, invalid formats and missing stages have defined behavior.
- [ ] Time without a zone, DST, changing the display time zone and sub-second precision are tested.
- [ ] A partial result is not described as the whole table/database; refresh and a late worker do not mix versions.
- [ ] Returning to the table keeps its state and removes the effect of the local Depth View filter; no data is changed.
- [ ] Keyboard, reduced animation, the legend and the pinned panel allow working without relying on color alone.

### 12.3. Graph acceptance checklist

- [ ] Object and edge kinds are distinguishable; a composite FK is neither lost nor invented from column names.
- [ ] Metrics are labeled as estimated/measured/unknown/not applicable, with unit and scope.
- [ ] Switching `rows` to `bytes` does not rearrange the graph; views and partitions do not inflate totals through fictitious or double counting.
- [ ] FDW does not cause implicit remote scans, and missing access is not hidden behind a value of zero.
- [ ] Cancellation, partial metadata and a change of source have unambiguous behavior.
- [ ] The same objects can be explored in 2D/3D; opening data requires an explicit action.

### 12.4. Performance test matrix

Proposed data sets: 500, 10,000 and 100,000 rows; one and three series; short text and wide records with JSON; many identical dates; a very large date range. For the graph: 100, 1000 and 5000 objects with several edge densities. These are research workloads, not declared production limits.

The report should include hardware, operating system, rendering backend, build version, data size, p50/p95/p99 frame time, preparation time, memory, idle behavior and the number of queries. Compare the measurement after closing the module with baseline DBeast.

## 13. Open questions and decisions before implementation

1. Is a controlled 2.5D perspective enough, or does a free camera bring a measurable benefit?
2. How to keep wide rows readable, keep Y fixed and preserve orientation after narrowing the time range?
3. Which variant of multi-series filtering is easiest to understand, and how to show ends outside the range?
4. Which time types can be preserved losslessly across all current drivers and files?
5. When does grouping improve readability, and when does it hide exactly the events being searched for?
6. How to bind a preset to a result with changed or duplicated column names?
7. Which graph metadata is available with ordinary privileges, and how to describe incompleteness explicitly?
8. Which task does the user solve better than with a table of deltas or a 2D view?

**Recommendation:** start with R0/R1 on a synthetic, reproducible SQL result. Do not reduce the idea to a classic timeline or to an animation of a single source table. Evaluate the benefit that comes from preserving the full relational context and comparing several times at once.

## 14. Sources and scope of verification

Public sources were read on October 15, 2026. PostgreSQL links point to the documentation of version 18 specifically, matching the content checked; compatibility with the versions supported by DBeast requires separate tests. Product claims are the authors' descriptions, not independent benchmarks.

[S2]: https://www.altova.com/databasespy/database-visualization-charts
[S3]: https://marchildmann.com/columnlens/
[S4]: https://github.com/shane-jacobeen/schema3d
[S5]: https://opentelemetry.io/docs/specs/otel/logs/data-model/
[S6]: https://www.postgresql.org/docs/18/queries-table-expressions.html
[S7]: https://www.postgresql.org/docs/18/datatype-datetime.html
[S8]: https://www.postgresql.org/docs/18/catalog-pg-class.html
[S9]: https://www.postgresql.org/docs/18/functions-admin.html#FUNCTIONS-ADMIN-DBSIZE
[S10]: https://www.postgresql.org/docs/18/catalog-pg-constraint.html
[S11]: https://www.postgresql.org/docs/18/catalog-pg-depend.html
[S12]: https://www.postgresql.org/docs/18/postgres-fdw.html
[S13]: https://www.postgresql.org/docs/18/sql-createview.html
[S14]: https://www.postgresql.org/docs/18/rules-materializedviews.html
[S15]: https://www.postgresql.org/docs/18/catalog-pg-rewrite.html

- [S2]: Altova DatabaseSpy: Database Data Visualization & Charts.
- [S3]: ColumnLens: SQL and 3D City View.
- [S4]: Schema3D: project README and description of the schema visualization.
- [S5]: OpenTelemetry: Logs Data Model, timestamps and trace context.
- [S6]: PostgreSQL: Table Expressions, GROUP BY and HAVING.
- [S7]: PostgreSQL: Date/Time Types.
- [S8]: PostgreSQL: pg_class, object kinds and estimates.
- [S9]: PostgreSQL: object size functions.
- [S10]: PostgreSQL: pg_constraint.
- [S11]: PostgreSQL: pg_depend.
- [S12]: PostgreSQL: postgres_fdw, configuration and statistics.
- [S13]: PostgreSQL: CREATE VIEW.
- [S14]: PostgreSQL: Materialized Views.
- [S15]: PostgreSQL: pg_rewrite.

Repository documentation: [AGENTS.md](https://github.com/macolszewski/dbeast_manager/blob/master/AGENTS.md), [README.md](https://github.com/macolszewski/dbeast_manager/blob/master/README.md), [Cargo.toml](https://github.com/macolszewski/dbeast_manager/blob/master/Cargo.toml), read at the stated reference point. This document records a proposal and a validation plan; it is not a report from a working prototype or a commitment to a full implementation.
