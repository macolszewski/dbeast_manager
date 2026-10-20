# Typed cell editors

`cell_value::ColumnSpec` is the shared column contract. It combines an
`EditorKind`, nullability, the catalog type name and driver semantics. The enum
models editor strategies explicitly; composition and exhaustive matching replace
an inheritance tree. UI code does not implement another type parser.

## Metadata and validation flow

1. Driver catalog queries populate `PhysicalColumn` with a complete type name,
   nullability and PostgreSQL enum labels. PostgreSQL domains resolve to their
   base types. MySQL uses `COLUMN_TYPE`, preserving unsigned, precision and enum
   labels. SQLite uses `table_xinfo`.
2. `result_edit::resolve` constructs `ResultEditMetadata::column_specs` once,
   matching projected columns to their physical origins.
3. `ui/results/cell_editors.rs` supplies boolean/enum controls and cached inline
   validation feedback. Grid, Transpose, Record View and Value Editor reuse them.
4. `ResultSet::set_cell` validates before staging. Failed commits retain the draft.
5. `sqlgen::bound_updates` and `bound_mutations` validate again before SQL can leave
   the UI. `ColumnSpec::bind_value` preserves exact decimal text and binds booleans
   as typed values, including SQLite integer storage.

## Performance

Type parsing runs when metadata arrives, not for every cell on every frame.
Validation feedback is cached by widget identity and refreshed when the input
changes or editing starts. Only visible cell controls are built. Enum options are
built when their popup opens. No catalog reads or new tasks run during drawing.
The focused UI tests cover invalid-entry retention and checkbox/enum interaction;
the SQLite in-memory test covers metadata through parameter binding.

## Adding a type

Add an `EditorKind` strategy and its catalog mapping in `ColumnSpec::new`, then
implement `validate_text` and, if needed, `bind_value`. Reuse the shared UI module
for a specialized control. Add contract cases for valid, invalid, NULL and DEFAULT
values, plus a UI test when introducing a new control. Do not normalize precise
numbers through f64 or duplicate validation in individual views.

## Boundaries

This is local editing assistance, not a complete implementation of database
constraints. Unknown/custom types stay server-validated. CHECK, uniqueness and
foreign keys remain server responsibilities. Binary editing is unavailable.
SQLite affinity differs from strict type enforcement; the chosen editor behavior
and MySQL TINYINT(1) treatment are documented in the results-grid user guide.

## Database semantics references

- [PostgreSQL numeric types](https://www.postgresql.org/docs/15/datatype-numeric.html): precision, scales and rounding.
- [MySQL numeric type syntax](https://dev.mysql.com/doc/refman/8.4/en/numeric-type-syntax.html): BOOLEAN aliases and unsigned numeric types.
- [SQLite datatypes](https://www.sqlite.org/datatype3.html): affinities and boolean storage.

## Verification (macOS, 2026-10-20)

The focused contract, SQL generation, editing metadata, calendar and Value Editor
suites passed 42 tests. Two pre-existing manual benchmarks were ignored. The checkbox/enum test exercises mouse and keyboard
input; the invalid numeric draft remains editable after Enter. The Value Editor
also retains invalid input on Command+Enter and clears its error on Escape.
The rendered screenshot is in `docs/images/typed-cell-editors.png`.

SQLite metadata and boolean binding were checked with an in-memory database.
PostgreSQL and MySQL catalog changes were compiled but were not exercised against
live servers in this run. No database containers or cross-platform builds were
started. The macOS build uses two compiler jobs and the existing release target.

A single release-bundle startup with an empty temporary `DBEAST_CONFIG` reached
its first frame in 397 ms (`DBEAST_TRACE_STARTUP=1`). At three seconds, `ps` reported
110,304 KiB RSS (about 108 MiB) and 0.0% CPU. This is a startup smoke measurement,
not a large-result scrolling benchmark. The temporary process was then stopped.

Calendar component inputs additionally share a constrained `TextBuffer`: digits
and component ranges are checked before insertion, including pasted replacement
text. A rejected replacement restores the selected text. Incomplete components
remain in the popup draft; only a complete value passing `ColumnSpec` validation
updates the cell. Apply is disabled while the draft is incomplete. A focused UI
test covers hours, minutes, seconds, fractional seconds and offsets, including
invalid letters, out-of-range replacement and clearing required components.
The eight calendar/editor regression tests passed after this change.
