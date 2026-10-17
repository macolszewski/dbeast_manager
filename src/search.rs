//! Literal search using Unicode scalar offsets shared with the SQL editor.
//!
//! Case-insensitive searches compare locale-independent Unicode lowercase forms.
//! Lowercase expansions are supported, but matches must span complete original
//! characters. This is not Unicode normalization or full case folding: `ß` does
//! not match `ss`, and final sigma `ς` does not match `σ`.

use std::ops::Range;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SearchOptions {
    pub match_case: bool,
    pub whole_words: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchMatch {
    pub bytes: Range<usize>,
    pub chars: Range<usize>,
}

/// A literal pattern compiled once for repeated searches, such as grid cells.
#[derive(Clone, Debug)]
pub struct SearchPattern {
    pattern: Vec<char>,
    fallback: Vec<usize>,
    options: SearchOptions,
}

impl SearchPattern {
    pub fn new(needle: &str, options: SearchOptions) -> Self {
        let pattern: Vec<char> = if options.match_case {
            needle.chars().collect()
        } else {
            needle.chars().flat_map(char::to_lowercase).collect()
        };
        let mut fallback = vec![0; pattern.len()];
        let mut matched = 0;
        for index in 1..pattern.len() {
            while matched > 0 && pattern[index] != pattern[matched] {
                matched = fallback[matched - 1];
            }
            if pattern[index] == pattern[matched] {
                matched += 1;
            }
            fallback[index] = matched;
        }
        Self {
            pattern,
            fallback,
            options,
        }
    }

    /// Return whether `text` contains a match with the same rules as [`find_all`].
    ///
    /// This performs no allocations and stops at the first valid match. A lazy
    /// second iterator locates candidate starts without a per-call ring buffer:
    /// each iterator advances at most once across the text, keeping worst-case
    /// time linear even for repetitive input and Unicode lowercase expansions.
    /// An empty pattern matches nothing.
    pub fn is_match(&self, text: &str) -> bool {
        if self.pattern.is_empty() {
            return false;
        }
        let characters = folded_characters(text, self.options.match_case);
        let mut starts = characters.clone();
        let mut starts_consumed = 0;
        let mut matched = 0;
        for (index, character) in characters.enumerate() {
            while matched > 0 && character.value != self.pattern[matched] {
                matched = self.fallback[matched - 1];
            }
            if character.value == self.pattern[matched] {
                matched += 1;
            }
            if matched != self.pattern.len() {
                continue;
            }
            if character.last {
                let start_index = index + 1 - self.pattern.len();
                let start = starts
                    .nth(start_index - starts_consumed)
                    .expect("the candidate start precedes the current character");
                starts_consumed = start_index + 1;
                if start.first
                    && (!self.options.whole_words
                        || word_boundaries(text, start.byte_start, character.byte_end))
                {
                    return true;
                }
            }
            matched = self.fallback[matched - 1];
        }
        false
    }
}

#[derive(Clone, Copy)]
struct FoldedCharacter {
    value: char,
    byte_start: usize,
    byte_end: usize,
    first: bool,
    last: bool,
}

fn folded_characters(
    text: &str,
    match_case: bool,
) -> impl Iterator<Item = FoldedCharacter> + Clone + '_ {
    text.char_indices().flat_map(move |(byte_start, value)| {
        let byte_end = byte_start + value.len_utf8();
        let mut unchanged = Some(value);
        let mut lowercase = value.to_lowercase().peekable();
        let mut first = true;
        std::iter::from_fn(move || {
            let value = if match_case {
                unchanged.take()?
            } else {
                lowercase.next()?
            };
            let character = FoldedCharacter {
                value,
                byte_start,
                byte_end,
                first,
                last: match_case || lowercase.peek().is_none(),
            };
            first = false;
            Some(character)
        })
    })
}

/// Find nonoverlapping literal matches, ordered by their position in `text`.
///
/// Empty needles have no matches. Whole-word mode requires boundaries outside
/// each match; Unicode alphanumerics, `_`, and `$` are SQL word characters.
/// Streaming KMP keeps time linear in text plus needle length, including for
/// repetitive input, and uses O(needle length + number of matches) memory.
pub fn find_all(text: &str, needle: &str, options: SearchOptions) -> Vec<SearchMatch> {
    let SearchPattern {
        pattern, fallback, ..
    } = SearchPattern::new(needle, options);
    if pattern.is_empty() {
        return Vec::new();
    }

    let mut matcher = Matcher {
        text,
        options,
        starts: vec![MatchStart::default(); pattern.len()],
        pattern,
        fallback,
        consumed: 0,
        matched: 0,
        matches: Vec::new(),
    };
    for (char_index, (byte_index, value)) in text.char_indices().enumerate() {
        let source = MatchStart {
            byte_index,
            char_index,
            first: true,
        };
        let byte_end = byte_index + value.len_utf8();
        if options.match_case {
            matcher.push(value, source, byte_end, true);
        } else {
            let mut source = source;
            let mut lowercase = value.to_lowercase().peekable();
            while let Some(folded) = lowercase.next() {
                matcher.push(folded, source, byte_end, lowercase.peek().is_none());
                source.first = false;
            }
        }
    }
    matcher.matches
}

#[derive(Clone, Copy, Default)]
struct MatchStart {
    byte_index: usize,
    char_index: usize,
    first: bool,
}

struct Matcher<'a> {
    text: &'a str,
    options: SearchOptions,
    pattern: Vec<char>,
    fallback: Vec<usize>,
    starts: Vec<MatchStart>,
    consumed: usize,
    matched: usize,
    matches: Vec<SearchMatch>,
}

impl Matcher<'_> {
    fn push(&mut self, value: char, source: MatchStart, byte_end: usize, last: bool) {
        let length = self.pattern.len();
        self.starts[self.consumed % length] = source;
        self.consumed += 1;
        while self.matched > 0 && value != self.pattern[self.matched] {
            self.matched = self.fallback[self.matched - 1];
        }
        if value == self.pattern[self.matched] {
            self.matched += 1;
        }
        if self.matched != length {
            return;
        }

        let start = self.starts[(self.consumed - length) % length];
        if start.first
            && last
            && (!self.options.whole_words || word_boundaries(self.text, start.byte_index, byte_end))
        {
            self.matches.push(SearchMatch {
                bytes: start.byte_index..byte_end,
                chars: start.char_index..source.char_index + 1,
            });
            self.matched = 0;
        } else {
            self.matched = self.fallback[length - 1];
        }
    }
}

fn word_boundaries(text: &str, start: usize, end: usize) -> bool {
    !text[..start]
        .chars()
        .next_back()
        .is_some_and(is_word_character)
        && !text[end..].chars().next().is_some_and(is_word_character)
}

fn is_word_character(value: char) -> bool {
    value.is_alphanumeric() || matches!(value, '_' | '$')
}

/// Replace matches in one pass with literal replacement text (`$` has no meaning).
///
/// `matches` must be ordered, nonoverlapping byte ranges with valid UTF-8
/// boundaries in this exact `text`, as returned by [`find_all`].
pub fn replace_matches(text: &str, matches: &[SearchMatch], replacement: &str) -> String {
    let removed: usize = matches.iter().map(|found| found.bytes.len()).sum();
    let capacity = text.len() - removed + replacement.len() * matches.len();
    let mut result = String::with_capacity(capacity);
    let mut copied = 0;
    for found in matches {
        result.push_str(&text[copied..found.bytes.start]);
        result.push_str(replacement);
        copied = found.bytes.end;
    }
    result.push_str(&text[copied..]);
    result
}

/// Map a 1-based line and column to a Unicode scalar offset for the editor.
///
/// Zero coordinates and nonexistent lines return `None`. Columns beyond the
/// line are clamped to its end, before any LF or CRLF line ending. A trailing
/// newline introduces an empty final line; an empty document has one empty line.
/// Columns count Unicode scalar values, not bytes, graphemes, or display cells.
pub fn caret_at_line_column(text: &str, line: usize, column: usize) -> Option<usize> {
    if line == 0 || column == 0 {
        return None;
    }
    let mut chars = text.chars().peekable();
    let mut offset = 0;
    let mut current_line = 1;
    while current_line < line {
        let value = chars.next()?;
        offset += 1;
        if value == '\n' {
            current_line += 1;
        }
    }
    for _ in 1..column {
        let Some(&value) = chars.peek() else {
            break;
        };
        if value == '\n' || (value == '\r' && chars.clone().nth(1) == Some('\n')) {
            break;
        }
        chars.next();
        offset += 1;
    }
    Some(offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_unicode_with_distinct_byte_and_character_offsets() {
        let text = "😀 Zażółć; ZAŻÓŁĆ; zażółć";
        let found = find_all(text, "zażółć", SearchOptions::default());
        assert_eq!(found.len(), 3);
        assert_eq!(found[0].bytes, 5..15);
        assert_eq!(found[0].chars, 2..8);
        assert_eq!(&text[found[1].bytes.clone()], "ZAŻÓŁĆ");
        assert_eq!(found[1].chars, 10..16);
        assert_eq!(found[2].chars, 18..24);
        assert_eq!(
            find_all(
                text,
                "zażółć",
                SearchOptions {
                    match_case: true,
                    whole_words: false
                }
            ),
            vec![found[2].clone()]
        );
    }

    #[test]
    fn lowercase_expansion_requires_complete_source_characters() {
        let text = "İ i\u{307} I i";
        let found = find_all(text, "İ", SearchOptions::default());
        assert_eq!(
            found,
            vec![
                SearchMatch {
                    bytes: 0..2,
                    chars: 0..1
                },
                SearchMatch {
                    bytes: 3..6,
                    chars: 2..4
                },
            ]
        );
        let single = find_all(text, "i", SearchOptions::default());
        assert_eq!(
            single
                .iter()
                .map(|found| found.chars.clone())
                .collect::<Vec<_>>(),
            vec![2..3, 5..6, 7..8]
        );
        assert!(find_all("İ", "\u{307}", SearchOptions::default()).is_empty());
        assert!(find_all("ß", "ss", SearchOptions::default()).is_empty());
        assert!(find_all("ς", "Σ", SearchOptions::default()).is_empty());
        assert!(find_all("é", "e\u{301}", SearchOptions::default()).is_empty());
    }

    #[test]
    fn whole_words_respect_sql_identifier_characters() {
        let text = "foo _foo foo_ $foo foo$ foo1 1foo αfoo fooβ (foo)\nFOO";
        let found = find_all(
            text,
            "foo",
            SearchOptions {
                match_case: false,
                whole_words: true,
            },
        );
        assert_eq!(
            found
                .iter()
                .map(|found| &text[found.bytes.clone()])
                .collect::<Vec<_>>(),
            vec!["foo", "foo", "FOO"]
        );
        assert_eq!(found[0].chars, 0..3);
        assert!(
            find_all(
                "αβ",
                "α",
                SearchOptions {
                    match_case: false,
                    whole_words: true
                }
            )
            .is_empty()
        );
    }

    #[test]
    fn literal_multiline_and_adjacent_matches_do_not_overlap() {
        assert!(find_all("anything", "", SearchOptions::default()).is_empty());
        assert!(find_all("", "query", SearchOptions::default()).is_empty());
        let adjacent = find_all("aaaaa", "aa", SearchOptions::default());
        assert_eq!(
            adjacent
                .iter()
                .map(|found| found.bytes.clone())
                .collect::<Vec<_>>(),
            vec![0..2, 2..4]
        );
        assert_eq!(replace_matches("aaaaa", &adjacent, "x"), "xxa");
        let text = "a.*\nβ a.*\nΒ";
        let multiline = find_all(text, ".*\nβ", SearchOptions::default());
        assert_eq!(multiline.len(), 2);
        assert_eq!(replace_matches(text, &multiline, "$1\\n"), "a$1\\n a$1\\n");
        assert_eq!(replace_matches(text, &multiline, ""), "a a");
        assert_eq!(replace_matches(text, &[], "$1"), text);
    }

    #[test]
    fn repetitive_prefix_fallback_finds_later_matches() {
        let mut text = "a".repeat(100_000);
        text.push('b');
        let needle = format!("{}b", "a".repeat(10_000));
        let found = find_all(&text, &needle, SearchOptions::default());
        assert_eq!(
            found,
            vec![SearchMatch {
                bytes: 90_000..100_001,
                chars: 90_000..100_001
            }]
        );
        let compiled = SearchPattern::new(&needle, SearchOptions::default());
        assert!(compiled.is_match(&text));
        assert!(!compiled.is_match(&text[..text.len() - 1]));
    }

    #[test]
    fn compiled_pattern_matches_find_all_across_cells_and_options() {
        let cells = [
            "",
            "customer_id Customer_ID customer id",
            "foo _foo foo_ $foo foo$ foo1 1foo αfoo fooβ (foo)\nFOO",
            "😀 Zażółć; ZAŻÓŁĆ; zażółć",
            "İ i\u{307} I i",
            "ß ss SS ς σ Σ",
            "aaaaa",
            "a.*\nβ a.*\nΒ",
            "é e\u{301}",
        ];
        for match_case in [false, true] {
            for whole_words in [false, true] {
                let options = SearchOptions {
                    match_case,
                    whole_words,
                };
                for needle in [
                    "",
                    "absent",
                    "customer_id",
                    "foo",
                    "ZAŻÓŁĆ",
                    "😀",
                    "İ",
                    "i",
                    "\u{307}",
                    "ß",
                    "ss",
                    "Σ",
                    "aa",
                    ".*\nβ",
                    "é",
                    "e\u{301}",
                ] {
                    let compiled = SearchPattern::new(needle, options);
                    for cell in cells {
                        assert_eq!(
                            compiled.is_match(cell),
                            !find_all(cell, needle, options).is_empty(),
                            "cell={cell:?}, needle={needle:?}, options={options:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn streaming_search_matches_independent_unicode_reference() {
        fn strings(max_length: usize) -> Vec<String> {
            let mut all = vec![String::new()];
            let mut level = vec![String::new()];
            for _ in 0..max_length {
                level = level
                    .iter()
                    .flat_map(|prefix| {
                        ['I', 'i', 'İ', '\u{307}', ' '].map(|value| format!("{prefix}{value}"))
                    })
                    .collect();
                all.extend(level.iter().cloned());
            }
            all
        }

        let needles = strings(2);
        for text in strings(4) {
            let offsets: Vec<usize> = text
                .char_indices()
                .map(|(offset, _)| offset)
                .chain(std::iter::once(text.len()))
                .collect();
            for needle in &needles {
                let normalized = needle.to_lowercase();
                let mut expected = Vec::new();
                let mut start = 0;
                while !needle.is_empty() && start + 1 < offsets.len() {
                    let end = (start + 1..offsets.len()).find(|&end| {
                        text[offsets[start]..offsets[end]].to_lowercase() == normalized
                    });
                    if let Some(end) = end {
                        expected.push(SearchMatch {
                            bytes: offsets[start]..offsets[end],
                            chars: start..end,
                        });
                        start = end;
                    } else {
                        start += 1;
                    }
                }
                assert_eq!(
                    find_all(&text, needle, SearchOptions::default()),
                    expected,
                    "text={text:?}, needle={needle:?}"
                );
                assert_eq!(
                    SearchPattern::new(needle, SearchOptions::default()).is_match(&text),
                    !expected.is_empty(),
                    "compiled text={text:?}, needle={needle:?}"
                );
            }
        }
    }

    #[test]
    fn caret_coordinates_handle_unicode_newlines_and_clamping() {
        let text = "é😀\r\nβ\n";
        assert_eq!(caret_at_line_column(text, 1, 1), Some(0));
        assert_eq!(caret_at_line_column(text, 1, 2), Some(1));
        assert_eq!(caret_at_line_column(text, 1, 100), Some(2));
        assert_eq!(caret_at_line_column(text, 2, 1), Some(4));
        assert_eq!(caret_at_line_column(text, 2, 100), Some(5));
        assert_eq!(caret_at_line_column(text, 3, 1), Some(6));
        assert_eq!(caret_at_line_column(text, 4, 1), None);
        assert_eq!(caret_at_line_column(text, 0, 1), None);
        assert_eq!(caret_at_line_column(text, 1, 0), None);
        assert_eq!(caret_at_line_column("", 1, 100), Some(0));
        assert_eq!(caret_at_line_column("", 2, 1), None);
        assert_eq!(caret_at_line_column("a\rb", 1, 3), Some(2));
    }

    #[test]
    #[ignore = "release-mode search benchmark; run explicitly with --ignored --nocapture"]
    fn search_large_sql_benchmark() {
        use std::{hint::black_box, time::Instant};

        let statement =
            "SELECT customer_id, 'Zażółć 😀' AS label FROM orders WHERE customer_id = 42;\n";
        for minimum_bytes in [100_000_usize, 1_000_000] {
            let text = statement.repeat(minimum_bytes.div_ceil(statement.len()));
            for options in [
                SearchOptions::default(),
                SearchOptions {
                    match_case: true,
                    whole_words: true,
                },
            ] {
                let iterations = 50;
                let start = Instant::now();
                let mut count = 0;
                for _ in 0..iterations {
                    let found = find_all(black_box(&text), black_box("customer_id"), options);
                    count = found.len();
                    black_box(found);
                }
                eprintln!(
                    "search: {} bytes, {options:?}, {count} matches, {:?}/iteration",
                    text.len(),
                    start.elapsed() / iterations
                );
            }
        }
    }

    #[test]
    #[ignore = "release-mode cell search benchmark; run explicitly with --ignored --nocapture"]
    fn search_compiled_cells_benchmark() {
        use std::{hint::black_box, time::Instant};

        let compiled = SearchPattern::new("customer", SearchOptions::default());
        for cell_count in [500, 5_000, 100_000] {
            let cells: Vec<String> = (0..cell_count)
                .map(|index| match index % 4 {
                    0 => format!("customer {index}: Zażółć 😀"),
                    1 => format!("invoice_{index} payment received"),
                    2 => format!("Account {index} / Customer account"),
                    _ => format!("audit event {index}: scheduled"),
                })
                .collect();
            let iterations = 20;
            let start = Instant::now();
            for _ in 0..iterations {
                let count = cells
                    .iter()
                    .filter(|cell| compiled.is_match(black_box(cell)))
                    .count();
                assert_eq!(black_box(count), cell_count / 2);
            }
            eprintln!(
                "compiled cell search: {cell_count} cells, {:?}/iteration",
                start.elapsed() / iterations
            );
        }
    }
}
