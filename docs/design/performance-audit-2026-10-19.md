# Performance follow-up, 19 October 2026

Scope: macOS startup and idle resources, normal and transposed grid repainting,
column resizing, result eviction, and the existing large-SQL editor benchmark.
All native measurements use a temporary configuration with no connections.
No database containers, cross-platform builds, or user data were used.

## Confirmed problems and changes

### Hidden columns were still visited every frame

The grid already drew only visible cells, but `column_window` summed the widths
of all hidden columns on every repaint. In Transpose, each hidden record caused
a width-cache lookup. Revealing a distant cell also summed all preceding widths.

Cumulative positions are now cached and searched with binary search. A distant
column's position is a direct lookup. Changes to sorting, filtering, edits,
metadata, zoom, spacing and user resizing invalidate the relevant layout.
Resized widths use a shared `Arc` rather than cloning a hash map each frame.

The initial layout still prepares widths for the page; this change removes
repeated work, not the initial preparation. Position storage costs about four
bytes per displayed horizontal column plus the vector allocation.

Diagnostic CPU benchmark: 1000x500 viewport, five warm-up frames, ten sampled
frames, 300 visible cells in Transpose. Same release dependency build before
and after, with `profile.release.package.dbeast.opt-level=0` to reuse the local
low-cost test build. These are not production frame times or GPU/FPS results.

| Case | Before | After |
| --- | ---: | ---: |
| Transpose, 500 records x 20 fields | 0.774 ms | 1.071 ms |
| Transpose, 5000 records x 20 fields | 1.587 ms | 0.692 ms |
| Normal, 50 rows x 2000 columns | 0.626 ms | 0.627 ms |
| Normal, 50 rows x 20 columns | 0.606 ms | 0.606 ms |

These short runs are noisy, particularly the 500-record result. The deterministic
regression test is stronger evidence of the complexity change: preparing an
unchanged 100,000-column layout must not call the width function again.

### Session eviction performed disk reads on the UI thread

When results exceeded the memory budget, `release_results_over` called filesystem
existence checks while drawing. Slow storage could therefore stall the window.
The existing session worker now verifies files after preceding writes complete,
then wakes the UI with the result. Only one check is outstanding, and retries
for unwritten files are limited to once a second. Eligibility is checked again
before eviction, keeping the active tab and pending edits in memory.

Session tests verify save ordering, deferred restoration and eviction, including
retention of active/edited tabs and restoration of the evicted data.

## Native startup and idle resources

One warm launch before and one after, optimized macOS binary, temporary empty
configuration. `DBEAST_TRACE_STARTUP=1` reports the first UI frame. `ps` samples
were taken after three and eight seconds; each launched process was terminated.

| Metric | Before | After |
| --- | ---: | ---: |
| First frame | 123 ms | 141 ms |
| Idle RSS at eight seconds | 108896 KiB | 108896 KiB |
| Idle CPU at eight seconds | 0.0% | 0.0% |
| Cumulative CPU at three / eight seconds | 0.17 / 0.17 s | 0.18 / 0.18 s |

These small variations do not establish a startup improvement or regression.
No idle CPU loop was observed. Results do not represent a populated user session.

## Remaining editor bottleneck

The existing `ui_editor_performance` benchmarks also ran with full release
optimization, without the diagnostic package override. 1440x900 viewport,
20 samples for idle/scroll and five for typing after idle. These benchmarks
measure CPU UI construction, not GPU work or real input-to-display latency.

| SQL size | Idle p50 / p95 | Scroll p50 / p95 | Typing p50 / p95 |
| --- | ---: | ---: | ---: |
| 100 kB, AccessKit disabled | 0.581 / 0.599 ms | 0.593 / 0.609 ms | 9.756 / 12.457 ms |
| 1 MB, AccessKit disabled | 5.712 / 5.893 ms | 5.677 / 5.848 ms | 36.263 / 37.134 ms |

With the AccessKit-enabled kittest path, 1 MB (27,237 lines) took 33.221 / 35.146 ms
for idle frames and 51.972 / 55.102 ms for scrolling. This path includes test
framework/accessibility-tree processing, so the difference cannot be attributed
entirely to native accessibility overhead. Folding reduced idle p50 to 12.130 ms.

Large-buffer typing remains above a 16.7 ms frame budget even without AccessKit.
The grid/session changes do not fix that editor path. Incremental text/layout
and accessibility updates need a separate focused implementation; disabling
accessibility is not an acceptable performance fix.

Reproduction:

```sh
CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 cargo test --release --test ui_editor_performance editor_ -- --ignored --nocapture --test-threads=1
```

## Validation

- Layout tests, including cache reuse/invalidation and column positions.
- Wide-grid navigation to distant records and fields.
- Header dragging and resize isolation.
- Date/time cell editing in the changed grid and screenshots at 100% and 150%.
- Session serialization, ordering, lazy restoration and memory-budget eviction.
- Native macOS release bundle.

Database latency, SSH/TLS, GPU rendering time, real user sessions and other
platforms were not measured in this pass. No claim of universal 60 fps is made.
