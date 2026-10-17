//! Bounded, byte-exact DDL alignment. All indexing and preview work runs off the UI thread.

use crate::model::{ObjectKind, SourceIdentity};
use std::{
    collections::HashMap,
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub const MAX_DDL_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_DDL_LINES: usize = 100_000;
const PREVIEW_BYTES: usize = 4096;
const GAP_CELLS: usize = 1_000_000;
const TOTAL_CELLS: usize = 4_000_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DdlObjectIdentity {
    pub ds_id: String,
    pub source: SourceIdentity,
    pub kind: ObjectKind,
    pub path: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct DdlDefinition {
    pub identity: DdlObjectIdentity,
    pub sql: String,
    pub coverage: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DdlChange {
    Equal,
    Changed,
    OriginOnly,
    TargetOnly,
}

#[derive(Clone, Debug)]
pub struct DdlLine {
    /// Includes the original line ending, if any.
    pub range: Range<usize>,
    pub preview: String,
    pub truncated: bool,
}

#[derive(Clone, Debug)]
pub struct DdlSnapshot {
    pub identity: DdlObjectIdentity,
    pub sql: Arc<str>,
    pub lines: Vec<DdlLine>,
    pub coverage: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DdlDiffRow {
    pub origin: Option<usize>,
    pub target: Option<usize>,
    pub change: DdlChange,
}

#[derive(Clone, Debug)]
pub struct ComparedDdl {
    pub origin: DdlSnapshot,
    pub target: DdlSnapshot,
    pub rows: Vec<DdlDiffRow>,
    /// Start row of each contiguous changed block.
    pub differences: Vec<usize>,
    pub alignment_limited: bool,
    pub elapsed: Duration,
}

fn check_cancel(cancelled: &AtomicBool) -> Result<(), String> {
    if cancelled.load(Ordering::Relaxed) {
        Err("Structure comparison cancelled".into())
    } else {
        Ok(())
    }
}

impl DdlSnapshot {
    fn prepare(definition: DdlDefinition, cancelled: &AtomicBool) -> Result<Self, String> {
        check_cancel(cancelled)?;
        if definition.sql.len() > MAX_DDL_BYTES {
            return Err("DDL exceeds the 8 MiB limit per object".into());
        }
        let mut lines = Vec::new();
        let mut offset = 0;
        for line in definition.sql.split_inclusive('\n') {
            check_cancel(cancelled)?;
            if lines.len() == MAX_DDL_LINES {
                return Err("DDL exceeds the 100000 line limit per object".into());
            }
            let display = line.strip_suffix('\n').unwrap_or(line);
            let display = if line.ends_with('\n') {
                display.strip_suffix('\r').unwrap_or(display)
            } else {
                display
            };
            let truncated = display.len() > PREVIEW_BYTES;
            let mut end = display.len().min(PREVIEW_BYTES);
            while !display.is_char_boundary(end) {
                end -= 1;
            }
            let mut preview = display[..end].to_owned();
            if truncated {
                preview.push('…');
            }
            lines.push(DdlLine {
                range: offset..offset + line.len(),
                preview,
                truncated,
            });
            offset += line.len();
        }
        Ok(Self {
            identity: definition.identity,
            sql: definition.sql.into(),
            lines,
            coverage: definition.coverage,
        })
    }

    pub fn line(&self, index: usize) -> &str {
        &self.sql[self.lines[index].range.clone()]
    }
}

/// Exact line identity includes whitespace, qualification, CRLF and final newline.
/// Large gaps use conservative positional alignment, with an explicit flag.
pub fn compare_ddl(
    origin: DdlDefinition,
    target: DdlDefinition,
    cancelled: &AtomicBool,
) -> Result<ComparedDdl, String> {
    compare_with_budget(
        origin,
        target,
        cancelled,
        TOTAL_CELLS,
        Duration::from_millis(250),
    )
}

fn compare_with_budget(
    origin: DdlDefinition,
    target: DdlDefinition,
    cancelled: &AtomicBool,
    cells: usize,
    duration: Duration,
) -> Result<ComparedDdl, String> {
    let started = Instant::now();
    if origin.identity.kind != target.identity.kind
        || origin.identity.source.driver != target.identity.source.driver
    {
        return Err("Choose two objects of the same type and database driver".into());
    }
    let origin = DdlSnapshot::prepare(origin, cancelled)?;
    let target = DdlSnapshot::prepare(target, cancelled)?;
    let mut alignment = Alignment {
        origin: &origin,
        target: &target,
        cancelled,
        deadline: Instant::now() + duration,
        cells,
        rows: Vec::with_capacity(origin.lines.len().max(target.lines.len())),
        limited: false,
    };
    alignment.align()?;
    let Alignment { rows, limited, .. } = alignment;
    let mut differences = Vec::new();
    let mut changed = false;
    for (index, row) in rows.iter().enumerate() {
        if index % 1024 == 0 {
            check_cancel(cancelled)?;
        }
        let current = row.change != DdlChange::Equal;
        if current && !changed {
            differences.push(index);
        }
        changed = current;
    }
    Ok(ComparedDdl {
        origin,
        target,
        rows,
        differences,
        alignment_limited: limited,
        elapsed: started.elapsed(),
    })
}

struct Alignment<'a> {
    origin: &'a DdlSnapshot,
    target: &'a DdlSnapshot,
    cancelled: &'a AtomicBool,
    deadline: Instant,
    cells: usize,
    rows: Vec<DdlDiffRow>,
    limited: bool,
}

impl Alignment<'_> {
    fn equal(&mut self, origin: usize, target: usize) {
        self.rows.push(DdlDiffRow {
            origin: Some(origin),
            target: Some(target),
            change: DdlChange::Equal,
        });
    }

    fn replace(&mut self, origin: Range<usize>, target: Range<usize>) -> Result<(), String> {
        for offset in 0..origin.len().max(target.len()) {
            if offset % 1024 == 0 {
                check_cancel(self.cancelled)?;
            }
            let left = (offset < origin.len()).then_some(origin.start + offset);
            let right = (offset < target.len()).then_some(target.start + offset);
            let change = match (left, right) {
                (Some(l), Some(r)) if self.origin.line(l) == self.target.line(r) => {
                    DdlChange::Equal
                }
                (Some(_), Some(_)) => DdlChange::Changed,
                (Some(_), None) => DdlChange::OriginOnly,
                (None, Some(_)) => DdlChange::TargetOnly,
                _ => unreachable!(),
            };
            self.rows.push(DdlDiffRow {
                origin: left,
                target: right,
                change,
            });
        }
        Ok(())
    }

    fn align(&mut self) -> Result<(), String> {
        let mut start = 0;
        let mut left_end = self.origin.lines.len();
        let mut right_end = self.target.lines.len();
        while start < left_end.min(right_end) && self.origin.line(start) == self.target.line(start)
        {
            check_cancel(self.cancelled)?;
            self.equal(start, start);
            start += 1;
        }
        while left_end > start
            && right_end > start
            && self.origin.line(left_end - 1) == self.target.line(right_end - 1)
        {
            check_cancel(self.cancelled)?;
            left_end -= 1;
            right_end -= 1;
        }
        let anchors = if Instant::now() < self.deadline {
            self.anchors(start..left_end, start..right_end)?
        } else {
            Vec::new()
        };
        let (mut left, mut right) = (start, start);
        for (l, r) in anchors {
            self.gap(left..l, right..r)?;
            self.equal(l, r);
            left = l + 1;
            right = r + 1;
        }
        self.gap(left..left_end, right..right_end)?;
        for offset in 0..self.origin.lines.len() - left_end {
            if offset % 1024 == 0 {
                check_cancel(self.cancelled)?;
            }
            self.equal(left_end + offset, right_end + offset);
        }
        Ok(())
    }

    /// One patience pass: unique equal lines, followed by a longest increasing
    /// subsequence of target positions. HashMap also checks full string equality.
    fn anchors(
        &self,
        origin: Range<usize>,
        target: Range<usize>,
    ) -> Result<Vec<(usize, usize)>, String> {
        fn unique<'a>(
            snapshot: &'a DdlSnapshot,
            range: Range<usize>,
            cancelled: &AtomicBool,
        ) -> Result<HashMap<&'a str, Option<usize>>, String> {
            let mut map = HashMap::with_capacity(range.len());
            for index in range {
                check_cancel(cancelled)?;
                map.entry(snapshot.line(index))
                    .and_modify(|value| *value = None)
                    .or_insert(Some(index));
            }
            Ok(map)
        }
        let left = unique(self.origin, origin.clone(), self.cancelled)?;
        let right = unique(self.target, target, self.cancelled)?;
        let mut pairs = Vec::new();
        for index in origin {
            check_cancel(self.cancelled)?;
            let line = self.origin.line(index);
            if left.get(line) == Some(&Some(index))
                && let Some(Some(other)) = right.get(line)
            {
                pairs.push((index, *other));
            }
        }
        let mut tails: Vec<usize> = Vec::new();
        let mut previous = vec![None; pairs.len()];
        for (index, &(_, target)) in pairs.iter().enumerate() {
            check_cancel(self.cancelled)?;
            let position = tails.partition_point(|&tail| pairs[tail].1 < target);
            if position > 0 {
                previous[index] = Some(tails[position - 1]);
            }
            if position == tails.len() {
                tails.push(index);
            } else {
                tails[position] = index;
            }
        }
        let mut result = Vec::with_capacity(tails.len());
        let mut current = tails.last().copied();
        while let Some(index) = current {
            result.push(pairs[index]);
            current = previous[index];
        }
        result.reverse();
        Ok(result)
    }

    fn gap(&mut self, origin: Range<usize>, target: Range<usize>) -> Result<(), String> {
        check_cancel(self.cancelled)?;
        if origin.is_empty() || target.is_empty() {
            return self.replace(origin, target);
        }
        let n = origin.len();
        let m = target.len();
        let cells = (n + 1).saturating_mul(m + 1);
        if cells > GAP_CELLS || cells > self.cells || Instant::now() >= self.deadline {
            self.limited = true;
            return self.replace(origin, target);
        }
        self.cells -= cells;
        let width = m + 1;
        let mut lcs = vec![0u32; cells];
        for i in (0..n).rev() {
            check_cancel(self.cancelled)?;
            if Instant::now() >= self.deadline {
                self.limited = true;
                return self.replace(origin, target);
            }
            for j in (0..m).rev() {
                lcs[i * width + j] =
                    if self.origin.line(origin.start + i) == self.target.line(target.start + j) {
                        lcs[(i + 1) * width + j + 1] + 1
                    } else {
                        lcs[(i + 1) * width + j].max(lcs[i * width + j + 1])
                    };
            }
        }
        let (mut i, mut j, mut block_i, mut block_j) = (0, 0, 0, 0);
        while i < n && j < m {
            check_cancel(self.cancelled)?;
            if self.origin.line(origin.start + i) == self.target.line(target.start + j) {
                self.replace(
                    origin.start + block_i..origin.start + i,
                    target.start + block_j..target.start + j,
                )?;
                self.equal(origin.start + i, target.start + j);
                i += 1;
                j += 1;
                block_i = i;
                block_j = j;
            } else if lcs[(i + 1) * width + j] >= lcs[i * width + j + 1] {
                i += 1;
            } else {
                j += 1;
            }
        }
        self.replace(
            origin.start + block_i..origin.end,
            target.start + block_j..target.end,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DataSource, DriverKind};
    use std::hint::black_box;

    fn definition(sql: &str) -> DdlDefinition {
        let ds = DataSource::new(DriverKind::Sqlite);
        DdlDefinition {
            identity: DdlObjectIdentity {
                ds_id: ds.id.clone(),
                source: ds.identity(),
                kind: ObjectKind::Table,
                path: vec!["main".into(), "t".into()],
            },
            sql: sql.into(),
            coverage: vec!["Stored table, index and trigger SQL".into()],
        }
    }
    fn compare(left: &str, right: &str) -> ComparedDdl {
        compare_ddl(definition(left), definition(right), &AtomicBool::new(false)).unwrap()
    }
    fn preserved(diff: &ComparedDdl) {
        let left: Vec<_> = diff.rows.iter().filter_map(|row| row.origin).collect();
        let right: Vec<_> = diff.rows.iter().filter_map(|row| row.target).collect();
        assert_eq!(left, (0..diff.origin.lines.len()).collect::<Vec<_>>());
        assert_eq!(right, (0..diff.target.lines.len()).collect::<Vec<_>>());
        assert_eq!(
            left.iter()
                .map(|&i| diff.origin.line(i))
                .collect::<String>(),
            diff.origin.sql.as_ref()
        );
        assert_eq!(
            right
                .iter()
                .map(|&i| diff.target.line(i))
                .collect::<String>(),
            diff.target.sql.as_ref()
        );
        for row in &diff.rows {
            if row.change == DdlChange::Equal {
                assert_eq!(
                    diff.origin.line(row.origin.unwrap()),
                    diff.target.line(row.target.unwrap())
                );
            }
        }
    }
    #[test]
    fn preserves_empty_insert_delete_replace_and_reordering() {
        for (left, right) in [
            ("", ""),
            ("", "a\n"),
            ("a\n", ""),
            ("a\nb\nc\n", "x\na\nb\nd\n"),
            ("b\na\nc\n", "a\nb\nc\n"),
            ("\n\n", "\n"),
            ("x\nx\ny\nx\n", "x\ny\ny\nx\n"),
        ] {
            let diff = compare(left, right);
            preserved(&diff);
            assert_eq!(diff.differences.is_empty(), left == right);
        }
    }
    #[test]
    fn keeps_unicode_crlf_and_missing_final_newline_exact() {
        let diff = compare(
            "CREATE VIEW żółć\r\nAS SELECT '🦀';",
            "CREATE VIEW żółć\nAS SELECT '🦀';\n",
        );
        preserved(&diff);
        assert_eq!(diff.differences, vec![0]);
        assert!(diff.rows.iter().all(|row| row.change == DdlChange::Changed));
    }
    #[test]
    fn bounds_previews_without_losing_full_definition() {
        let sql = format!("SELECT '{}'\n", "🦀".repeat(20_000));
        let diff = compare(&sql, &sql);
        preserved(&diff);
        assert!(diff.origin.lines[0].truncated);
        assert!(diff.origin.lines[0].preview.len() <= PREVIEW_BYTES + '…'.len_utf8());
        assert_eq!(diff.origin.sql.as_ref(), sql);
        assert!(diff.differences.is_empty());
    }
    #[test]
    fn budget_and_deadline_fallback_preserve_every_line() {
        for (cells, duration) in [(0, Duration::from_secs(1)), (TOTAL_CELLS, Duration::ZERO)] {
            let diff = compare_with_budget(
                definition("a\na\na\n"),
                definition("b\nb\n"),
                &AtomicBool::new(false),
                cells,
                duration,
            )
            .unwrap();
            preserved(&diff);
            assert!(diff.alignment_limited);
            assert!(!diff.differences.is_empty());
        }
        let diff = compare(&"a\n".repeat(2000), &"b\n".repeat(2000));
        preserved(&diff);
        assert!(diff.alignment_limited);
    }
    #[test]
    fn cancellation_and_input_limits_are_explicit_errors() {
        assert!(
            compare_ddl(definition("a"), definition("b"), &AtomicBool::new(true))
                .unwrap_err()
                .contains("cancelled")
        );
        assert!(
            compare_ddl(
                definition(&"x".repeat(MAX_DDL_BYTES + 1)),
                definition(""),
                &AtomicBool::new(false)
            )
            .unwrap_err()
            .contains("8 MiB")
        );
        assert!(
            compare_ddl(
                definition(&"\n".repeat(MAX_DDL_LINES + 1)),
                definition(""),
                &AtomicBool::new(false)
            )
            .unwrap_err()
            .contains("line limit")
        );
    }
    #[test]
    fn small_repeated_line_gaps_match_lcs_length() {
        fn oracle(a: &[u8], b: &[u8]) -> usize {
            let mut last = vec![0; b.len() + 1];
            for &x in a {
                let mut next = vec![0; b.len() + 1];
                for (j, &y) in b.iter().enumerate() {
                    next[j + 1] = if x == y {
                        last[j] + 1
                    } else {
                        last[j + 1].max(next[j])
                    };
                }
                last = next;
            }
            last[b.len()]
        }
        // Every symbol repeats, so the exact gap algorithm handles these cases.
        for left in [b"aaabbb".as_slice(), b"ababab", b"bbaabb", b"bbbaaa"] {
            for right in [b"aaabbb".as_slice(), b"bababa", b"aabbaa", b"bbbaaa"] {
                let text = |bytes: &[u8]| {
                    bytes
                        .iter()
                        .map(|&b| format!("{}\n", b as char))
                        .collect::<String>()
                };
                let diff = compare(&text(left), &text(right));
                preserved(&diff);
                assert_eq!(
                    diff.rows
                        .iter()
                        .filter(|row| row.change == DdlChange::Equal)
                        .count(),
                    oracle(left, right)
                );
            }
        }
    }
    #[test]
    fn sparse_hunks_and_unique_anchors_stay_ordered() {
        let left = (0..10000)
            .map(|i| format!("column_{i} TEXT,\n"))
            .collect::<String>();
        let right = left
            .replace("column_200 TEXT", "column_200 INT")
            .replace("column_9000 TEXT", "column_9000 INT");
        let diff = compare(&left, &right);
        preserved(&diff);
        assert_eq!(diff.differences, [200, 9000]);
        assert!(!diff.alignment_limited);
    }
    #[test]
    fn types_and_drivers_must_match() {
        let mut right = definition("x");
        right.identity.kind = ObjectKind::View;
        assert!(compare_ddl(definition("x"), right, &AtomicBool::new(false)).is_err());
        let mut right = definition("x");
        right.identity.source.driver = DriverKind::Postgres;
        assert!(compare_ddl(definition("x"), right, &AtomicBool::new(false)).is_err());
    }
    #[test]
    #[ignore = "release-only DDL alignment microbenchmark"]
    fn benchmark_ddl_diff() {
        for (lines, all_changed) in [(500, false), (5000, false), (100000, false), (100000, true)] {
            let left = (0..lines)
                .map(|i| format!("column_{i} TEXT,\n"))
                .collect::<String>();
            let right = (0..lines)
                .map(|i| {
                    format!(
                        "column_{i} {},\n",
                        if all_changed || i % 100 == 0 {
                            "INTEGER"
                        } else {
                            "TEXT"
                        }
                    )
                })
                .collect::<String>();
            let mut samples = Vec::new();
            let mut limited = false;
            for sample in 0..8 {
                let origin = definition(&left);
                let target = definition(&right);
                let start = Instant::now();
                let diff = black_box(compare_ddl(origin, target, &AtomicBool::new(false)).unwrap());
                let elapsed = start.elapsed();
                limited = diff.alignment_limited;
                if sample > 0 {
                    samples.push(elapsed);
                }
                preserved(&diff);
            }
            samples.sort();
            println!(
                "ddl_diff lines={lines} all_changed={all_changed} bytes={} median={:?} limited={limited}",
                left.len() + right.len(),
                samples[3]
            );
        }
        for sql in [
            "SELECT '🦀';\n".repeat(50_000),
            format!("SELECT '{}'", "x".repeat(1_000_000)),
            format!("{}a\n", "x".repeat(16_380)).repeat(500),
        ] {
            let target = if sql.starts_with("SELECT") {
                sql.replace("SELECT", "select")
            } else {
                sql.replace("a\n", "b\n")
            };
            let mut samples = Vec::new();
            for sample in 0..8 {
                let origin = definition(&sql);
                let target = definition(&target);
                let start = Instant::now();
                let diff = black_box(compare_ddl(origin, target, &AtomicBool::new(false)).unwrap());
                if sample > 0 {
                    samples.push(start.elapsed());
                }
                preserved(&diff);
            }
            samples.sort();
            println!(
                "ddl_diff repeated_or_long bytes={} median={:?}",
                sql.len(),
                samples[3]
            );
        }
    }
}
