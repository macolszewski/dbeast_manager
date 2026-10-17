//! Code folding for the SQL console: multi-line statements and
//! subqueries can be collapsed to a one-line summary.
//!
//! The console keeps the real text. The editor shows a "visible" text in which
//! every folded range is replaced by one private-use character, so the caret
//! steps over a fold as a whole; edits of the visible text are mapped back by
//! expanding those characters again.

use crate::model::DriverKind;
use crate::splitter;
use std::ops::Range;

/// First placeholder character; fold `i` is shown as `PLACEHOLDER_BASE + i`.
/// Supplementary private use area, which no SQL text contains.
const PLACEHOLDER_BASE: u32 = 0xF0000;

pub fn placeholder(index: usize) -> char {
    char::from_u32(PLACEHOLDER_BASE + index as u32).expect("placeholder in private use area")
}

/// Fold index of a placeholder character.
pub fn placeholder_index(c: char) -> Option<usize> {
    let v = c as u32;
    (PLACEHOLDER_BASE..PLACEHOLDER_BASE + 0xFFFD)
        .contains(&v)
        .then(|| (v - PLACEHOLDER_BASE) as usize)
}

/// A foldable range of the real text and the 0-based line its fold marker is on.
#[derive(Clone, Debug, PartialEq)]
pub struct Region {
    pub range: Range<usize>,
    pub line: usize,
}

/// Foldable regions: statements spanning several lines, and multi-line
/// contents of parentheses (subqueries, window definitions, column lists).
/// One region per line, the outermost, like the IDE's gutter markers.
pub fn regions(sql: &str, driver: DriverKind) -> Vec<Region> {
    // Many statements must not repeatedly scan the entire preceding script.
    let newlines: Vec<usize> = sql
        .bytes()
        .enumerate()
        .filter_map(|(offset, byte)| (byte == b'\n').then_some(offset))
        .collect();
    let line_of = |byte: usize| newlines.partition_point(|&offset| offset < byte);
    let mut found: Vec<Region> = Vec::new();
    for stmt in splitter::split(sql, driver) {
        // Leading comments stay visible; the marker sits on the first line of code.
        let start = stmt.range.start + leading_comments_len(&sql[stmt.range.clone()]);
        if sql[start..stmt.range.end].contains('\n') {
            found.push(Region {
                line: line_of(start),
                range: start..stmt.range.end,
            });
        }
        for inner in paren_contents(&sql[stmt.range.clone()]) {
            let range = stmt.range.start + inner.start..stmt.range.start + inner.end;
            if sql[range.clone()].contains('\n') {
                found.push(Region {
                    line: line_of(range.start),
                    range,
                });
            }
        }
    }
    found.sort_by_key(|r| (r.line, std::cmp::Reverse(r.range.len())));
    found.dedup_by_key(|r| r.line);
    found
}

/// Length of the whitespace and comments a statement starts with.
fn leading_comments_len(stmt: &str) -> usize {
    let mut rest = stmt;
    loop {
        let trimmed = rest.trim_start();
        if let Some(r) = trimmed.strip_prefix("--") {
            rest = r.find('\n').map_or("", |i| &r[i + 1..]);
        } else if let Some(r) = trimmed.strip_prefix("/*") {
            rest = r.find("*/").map_or("", |i| &r[i + 2..]);
        } else {
            return stmt.len() - trimmed.len();
        }
    }
}

/// Byte ranges of the trimmed contents of parentheses, ignoring parentheses
/// inside strings, quoted identifiers and comments.
fn paren_contents(sql: &str) -> Vec<Range<usize>> {
    let bytes = sql.as_bytes();
    let mut stack = Vec::new();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            q @ (b'\'' | b'"' | b'`') => {
                i += 1;
                while i < bytes.len() && bytes[i] != q {
                    i += 1;
                }
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 1;
            }
            b'(' => stack.push(i + 1),
            b')' => {
                if let Some(start) = stack.pop() {
                    let inner = &sql[start..i];
                    let lead = inner.len() - inner.trim_start().len();
                    let trail = inner.len() - inner.trim_end().len();
                    if lead + trail < inner.len() {
                        out.push(start + lead..i - trail);
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// One-line summary of folded text, e.g. "SELECT id, … FROM translations".
pub fn summary(text: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut keyword: Option<&str> = None;
    for l in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        // A clause keyword alone on its line is joined with its content.
        if let Some(k) = keyword.take() {
            lines.push(format!("{k} {l}"));
        } else if l.chars().all(|c| c.is_ascii_uppercase() || c == ' ') {
            keyword = Some(l);
        } else {
            lines.push(l.to_string());
        }
    }
    lines.extend(keyword.map(str::to_string));
    let first = lines.first().map(String::as_str).unwrap_or_default();
    let from = lines
        .iter()
        .skip(1)
        .find(|l| l.to_ascii_uppercase().starts_with("FROM "));
    let tail = from.or(lines.last().filter(|_| lines.len() > 1));
    let mut s = match tail {
        Some(t) => format!("{first} … {t}"),
        None => first.to_string(),
    };
    if s.chars().count() > 80 {
        s = s.chars().take(79).collect::<String>() + "…";
    }
    s
}

/// Text shown in the editor: each folded range (sorted, not overlapping)
/// replaced by its placeholder character.
pub fn visible(real: &str, folds: &[Range<usize>]) -> String {
    let mut out = String::with_capacity(real.len());
    let mut pos = 0;
    for (i, f) in folds.iter().enumerate() {
        out.push_str(&real[pos..f.start]);
        out.push(placeholder(i));
        pos = f.end;
    }
    out.push_str(&real[pos..]);
    out
}

/// Real text for an edited visible text, with the folds whose placeholders survived.
pub fn expand(visible: &str, real: &str, folds: &[Range<usize>]) -> (String, Vec<Range<usize>>) {
    let mut out = String::with_capacity(real.len());
    let mut kept = Vec::new();
    for c in visible.chars() {
        match placeholder_index(c).and_then(|i| folds.get(i)) {
            Some(f) => {
                let start = out.len();
                out.push_str(&real[f.clone()]);
                kept.push(start..out.len());
            }
            None => out.push(c),
        }
    }
    (out, kept)
}

/// Char index in the real text for a char index in the visible text.
pub fn to_real_char(visible: &str, real: &str, folds: &[Range<usize>], index: usize) -> usize {
    let mut real_index = index;
    for c in visible.chars().take(index) {
        if let Some(f) = placeholder_index(c).and_then(|i| folds.get(i)) {
            real_index += real[f.clone()].chars().count() - 1;
        }
    }
    real_index
}

/// Adds a fold, dropping folds it contains; ignored if it overlaps another fold.
pub fn fold(folds: &mut Vec<Range<usize>>, range: Range<usize>) {
    folds.retain(|f| !(range.start <= f.start && f.end <= range.end));
    if folds
        .iter()
        .any(|f| f.start < range.end && range.start < f.end)
    {
        return;
    }
    folds.push(range);
    folds.sort_by_key(|f| f.start);
}

#[cfg(test)]
mod tests {
    use super::*;

    const SQL: &str = "WITH ranked AS (\n    SELECT id,\n           x\n    FROM t\n)\nDELETE FROM t\nUSING ranked r;\n\nselect 1;";

    #[test]
    fn statements_and_subqueries_are_foldable() {
        let r = regions(SQL, DriverKind::Postgres);
        assert_eq!(r.iter().map(|r| r.line).collect::<Vec<_>>(), [0, 1]);
        assert!(
            SQL[r[0].range.clone()].starts_with("WITH")
                && SQL[r[0].range.clone()].ends_with("ranked r")
        );
        assert_eq!(
            &SQL[r[1].range.clone()],
            "SELECT id,\n           x\n    FROM t"
        );
        assert_eq!(summary(&SQL[r[1].range.clone()]), "SELECT id, … FROM t");
        assert_eq!(
            summary("SELECT\n    id,\n    x\nFROM\n    t\nWHERE\n    a"),
            "SELECT id, … FROM t"
        );
    }

    #[test]
    fn leading_comments_stay_outside_the_statement_fold() {
        let sql = "-- note\n/* more */\nselect a,\n b from t;";
        let r = regions(sql, DriverKind::Postgres);
        assert_eq!(r[0].line, 2);
        assert_eq!(&sql[r[0].range.clone()], "select a,\n b from t");
    }

    #[test]
    fn repeated_unicode_statements_keep_real_line_numbers() {
        let block = "-- Łódź 😀\r\nSELECT 'żółć',\r\n (SELECT\r\n  1);\r\n\r\n";
        let sql = block.repeat(1000);
        let found = regions(&sql, DriverKind::Postgres);
        assert_eq!(found.len(), 2000);
        for (index, pair) in found.as_chunks::<2>().0.iter().enumerate() {
            assert_eq!(pair[0].line, index * 5 + 1);
            assert_eq!(pair[1].line, index * 5 + 2);
            assert_eq!(&sql[pair[1].range.clone()], "SELECT\r\n  1");
        }
    }

    #[test]
    fn parentheses_in_strings_are_ignored() {
        let sql = "select ')(' as a,\n (select\n 1)";
        let r = regions(sql, DriverKind::Postgres);
        assert_eq!(&sql[r[1].range.clone()], "select\n 1");
    }

    #[test]
    fn visible_text_round_trips_through_edits() {
        let r = regions(SQL, DriverKind::Postgres);
        let folds = vec![r[1].range.clone()];
        let vis = visible(SQL, &folds);
        assert_eq!(
            vis.lines().nth(1).unwrap(),
            format!("    {}", placeholder(0))
        );
        // Typing before the fold keeps it and moves it.
        let edited = format!("-- x\n{vis}");
        let (real, kept) = expand(&edited, SQL, &folds);
        assert_eq!(real, format!("-- x\n{SQL}"));
        assert_eq!(&real[kept[0].clone()], &SQL[folds[0].clone()]);
        // Removing the placeholder removes the folded text.
        let (real, kept) = expand(&vis.replace(placeholder(0), ""), SQL, &folds);
        assert!(!real.contains("SELECT id") && kept.is_empty());
    }

    #[test]
    fn caret_maps_past_folds() {
        let fold = 4..10;
        let folds = std::slice::from_ref(&fold);
        let real = "abc 123456 xyz";
        let vis = visible(real, folds);
        assert_eq!(vis.chars().count(), 9);
        assert_eq!(to_real_char(&vis, real, folds, 5), 10);
        assert_eq!(to_real_char(&vis, real, folds, 2), 2);
    }

    #[test]
    fn folding_an_outer_range_replaces_inner_folds() {
        let mut folds = vec![5..8, 20..30];
        fold(&mut folds, 2..12);
        assert_eq!(folds, [2..12, 20..30]);
        fold(&mut folds, 25..40);
        assert_eq!(folds, [2..12, 20..30], "overlapping folds are ignored");
    }
}
