//! Splits a SQL script into statements, the way a console does before execution.
//!
//! Delimiters inside string literals, quoted identifiers, comments and
//! PostgreSQL dollar-quoted bodies are ignored. MySQL scripts may switch the
//! delimiter with `DELIMITER //`, and SQLite trigger bodies (`BEGIN ... END`)
//! are kept together because they contain `;`.

use crate::model::DriverKind;
use std::ops::Range;

#[derive(Clone, Debug, PartialEq)]
pub struct Statement {
    pub text: String,
    /// Byte range of the statement (without the delimiter) in the script.
    pub range: Range<usize>,
}

pub fn split(script: &str, dialect: DriverKind) -> Vec<Statement> {
    Splitter::new(script, dialect).run()
}

/// Statement containing the cursor, or the nearest one before it when the
/// cursor stands on whitespace after a delimiter.
pub fn statement_at(script: &str, dialect: DriverKind, cursor: usize) -> Option<Statement> {
    let statements = split(script, dialect);
    statements
        .iter()
        .find(|s| s.range.start <= cursor && cursor <= s.range.end)
        .or_else(|| statements.iter().rev().find(|s| s.range.end <= cursor))
        .or_else(|| statements.first())
        .cloned()
}

struct Splitter<'a> {
    src: &'a str,
    bytes: &'a [u8],
    dialect: DriverKind,
    delimiter: String,
    pos: usize,
    out: Vec<Statement>,
}

impl<'a> Splitter<'a> {
    fn new(src: &'a str, dialect: DriverKind) -> Self {
        Self {
            src,
            bytes: src.as_bytes(),
            dialect,
            delimiter: ";".into(),
            pos: 0,
            out: Vec::new(),
        }
    }

    fn run(mut self) -> Vec<Statement> {
        let mut start = 0;
        // BEGIN/CASE ... END nesting inside a CREATE TRIGGER body (SQLite).
        let mut block_depth = 0usize;
        let mut word_count = 0usize;
        // A delimiter inside parentheses does not end the statement.
        let mut paren_depth = 0usize;
        let mut is_trigger = false;

        while self.pos < self.bytes.len() {
            if self.at_line_start_command(&mut start) {
                continue;
            }
            let c = self.bytes[self.pos];
            match c {
                b'\'' => self.skip_quoted(b'\'', self.backslash_escapes()),
                b'"' => self.skip_quoted(b'"', false),
                b'`' if self.dialect == DriverKind::MySql => self.skip_quoted(b'`', false),
                b'[' if self.dialect == DriverKind::Sqlite => self.skip_until(b"]"),
                b'-' if self.peek(1) == Some(b'-') => self.skip_line(),
                b'#' if self.dialect == DriverKind::MySql => self.skip_line(),
                b'/' if self.peek(1) == Some(b'*') => self.skip_block_comment(),
                b'$' if self.dialect == DriverKind::Postgres && self.try_skip_dollar_quote() => {}
                b'(' => {
                    paren_depth += 1;
                    self.pos += 1;
                }
                b')' => {
                    paren_depth = paren_depth.saturating_sub(1);
                    self.pos += 1;
                }
                _ if self.src[self.pos..].starts_with(self.delimiter.as_str())
                    && block_depth == 0
                    && paren_depth == 0 =>
                {
                    self.push(start, self.pos);
                    self.pos += self.delimiter.len();
                    start = self.pos;
                    word_count = 0;
                    paren_depth = 0;
                    is_trigger = false;
                }
                c if c.is_ascii_alphabetic() || c == b'_' => {
                    let word = self.read_word().to_ascii_uppercase();
                    if self.dialect == DriverKind::Sqlite {
                        // CREATE [TEMP] TRIGGER ...
                        if word_count < 3 && word == "TRIGGER" {
                            is_trigger = true;
                        }
                        if is_trigger {
                            match word.as_str() {
                                "BEGIN" | "CASE" => block_depth += 1,
                                "END" => block_depth = block_depth.saturating_sub(1),
                                _ => {}
                            }
                        }
                    }
                    word_count += 1;
                }
                _ => self.pos += utf8_len(c),
            }
        }
        self.push(start, self.bytes.len());
        self.out
    }

    /// Handles client-side commands that only make sense at the start of a line.
    fn at_line_start_command(&mut self, stmt_start: &mut usize) -> bool {
        if self.dialect != DriverKind::MySql || !self.src[*stmt_start..self.pos].trim().is_empty() {
            return false;
        }
        let rest = &self.src[self.pos..];
        if rest.len() >= 10 && rest[..10].eq_ignore_ascii_case("DELIMITER ") {
            let line_end = rest.find('\n').unwrap_or(rest.len());
            let new_delim = rest[10..line_end].trim();
            if !new_delim.is_empty() {
                self.delimiter = new_delim.to_string();
            }
            self.pos += line_end;
            *stmt_start = self.pos;
            return true;
        }
        false
    }

    fn backslash_escapes(&self) -> bool {
        match self.dialect {
            DriverKind::MySql => true,
            // E'...' strings in PostgreSQL.
            DriverKind::Postgres => self.pos > 0 && matches!(self.bytes[self.pos - 1], b'E' | b'e'),
            DriverKind::Sqlite => false,
        }
    }

    fn peek(&self, n: usize) -> Option<u8> {
        self.bytes.get(self.pos + n).copied()
    }

    fn skip_quoted(&mut self, quote: u8, backslash: bool) {
        self.pos += 1;
        while self.pos < self.bytes.len() {
            let c = self.bytes[self.pos];
            if backslash && c == b'\\' {
                self.pos += 2;
            } else if c == quote {
                // Doubled quote is an escaped quote.
                if self.peek(1) == Some(quote) {
                    self.pos += 2;
                } else {
                    self.pos += 1;
                    return;
                }
            } else {
                self.pos += 1;
            }
        }
        self.pos = self.pos.min(self.bytes.len());
    }

    fn skip_until(&mut self, end: &[u8]) {
        match self.src[self.pos + 1..].find(std::str::from_utf8(end).unwrap()) {
            Some(i) => self.pos += 1 + i + end.len(),
            None => self.pos = self.bytes.len(),
        }
    }

    fn skip_line(&mut self) {
        match self.src[self.pos..].find('\n') {
            Some(i) => self.pos += i + 1,
            None => self.pos = self.bytes.len(),
        }
    }

    fn skip_block_comment(&mut self) {
        // PostgreSQL allows nested block comments.
        let nested = self.dialect == DriverKind::Postgres;
        let mut depth = 0usize;
        while self.pos < self.bytes.len() {
            if self.bytes[self.pos..].starts_with(b"/*") && (nested || depth == 0) {
                depth += 1;
                self.pos += 2;
            } else if self.bytes[self.pos..].starts_with(b"*/") {
                depth -= 1;
                self.pos += 2;
                if depth == 0 {
                    return;
                }
            } else {
                self.pos += 1;
            }
        }
    }

    /// Skips `$tag$ ... $tag$` and returns true, or returns false when `$` is
    /// not a dollar quote opener (e.g. a `$1` parameter).
    fn try_skip_dollar_quote(&mut self) -> bool {
        let rest = &self.src[self.pos + 1..];
        let Some(close) = rest.find('$') else {
            return false;
        };
        let tag = &rest[..close];
        let valid_tag = tag
            .chars()
            .enumerate()
            .all(|(i, ch)| ch == '_' || ch.is_alphabetic() || (i > 0 && ch.is_ascii_digit()));
        if !valid_tag {
            return false;
        }
        let opener = format!("${tag}$");
        let body_start = self.pos + opener.len();
        match self.src[body_start..].find(&opener) {
            Some(i) => self.pos = body_start + i + opener.len(),
            None => self.pos = self.bytes.len(),
        }
        true
    }

    fn read_word(&mut self) -> &'a str {
        let start = self.pos;
        while self.pos < self.bytes.len()
            && (self.bytes[self.pos].is_ascii_alphanumeric() || self.bytes[self.pos] == b'_')
        {
            self.pos += 1;
        }
        &self.src[start..self.pos]
    }

    fn push(&mut self, start: usize, end: usize) {
        let raw = &self.src[start..end];
        if is_blank_sql(raw) {
            return;
        }
        let lead = raw.len() - raw.trim_start().len();
        let trimmed = raw.trim();
        let s = start + lead;
        self.out.push(Statement {
            text: trimmed.to_string(),
            range: s..s + trimmed.len(),
        });
    }
}

fn utf8_len(first: u8) -> usize {
    match first {
        0xF0..=0xFF => 4,
        0xE0..=0xEF => 3,
        0xC0..=0xDF => 2,
        _ => 1,
    }
}

/// True for text consisting only of whitespace and comments.
fn is_blank_sql(s: &str) -> bool {
    let mut rest = s.trim_start();
    loop {
        if rest.is_empty() {
            return true;
        }
        if let Some(r) = rest.strip_prefix("--").or_else(|| rest.strip_prefix('#')) {
            rest = r.find('\n').map_or("", |i| &r[i + 1..]).trim_start();
        } else if let Some(r) = rest.strip_prefix("/*") {
            rest = r.find("*/").map_or("", |i| &r[i + 2..]).trim_start();
        } else {
            return false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(sql: &str, d: DriverKind) -> Vec<String> {
        split(sql, d).into_iter().map(|s| s.text).collect()
    }

    #[test]
    fn splits_on_semicolons() {
        assert_eq!(
            texts("select 1; select 2;\n\nselect 3", DriverKind::Postgres),
            ["select 1", "select 2", "select 3"]
        );
    }

    #[test]
    fn ignores_delimiters_in_strings_and_comments() {
        let sql = "select ';' as a, \"x;y\" -- c;\nfrom t; /* a; b */ select 'it''s;'";
        assert_eq!(
            texts(sql, DriverKind::Postgres),
            [
                "select ';' as a, \"x;y\" -- c;\nfrom t",
                "/* a; b */ select 'it''s;'"
            ]
        );
    }

    #[test]
    fn skips_comment_only_fragments() {
        assert_eq!(
            texts("select 1; -- trailing;\n/* end */", DriverKind::Postgres),
            ["select 1"]
        );
    }

    #[test]
    fn postgres_dollar_quoting() {
        let sql = "create function f() returns int as $body$ begin return 1; end; $body$ language plpgsql; select $1";
        let out = texts(sql, DriverKind::Postgres);
        assert_eq!(out.len(), 2);
        assert!(out[0].ends_with("language plpgsql"));
        assert_eq!(out[1], "select $1");
    }

    #[test]
    fn postgres_escape_string() {
        assert_eq!(
            texts(r"select E'a\';b'; select 2", DriverKind::Postgres),
            [r"select E'a\';b'", "select 2"]
        );
    }

    #[test]
    fn mysql_delimiter_and_backslash() {
        let sql = "select 'a\\';b';\nDELIMITER //\ncreate procedure p() begin select 1; select 2; end//\nDELIMITER ;\nselect `a;b` from t;";
        let out = texts(sql, DriverKind::MySql);
        assert_eq!(out.len(), 3, "{out:?}");
        assert!(out[1].starts_with("create procedure") && out[1].ends_with("end"));
        assert_eq!(out[2], "select `a;b` from t");
    }

    #[test]
    fn sqlite_trigger_body() {
        let sql = "create trigger t after insert on a begin update b set x = case when 1 then 2 end; delete from c; end; select 1;";
        let out = texts(sql, DriverKind::Sqlite);
        assert_eq!(out.len(), 2, "{out:?}");
        assert!(out[0].ends_with("end"));
    }

    #[test]
    fn ignores_delimiter_inside_parentheses() {
        let sql = "create rule r as on insert to t do also (insert into a values (1); insert into b values (2)); select 1";
        assert_eq!(split(sql, DriverKind::Postgres).len(), 2);
    }

    #[test]
    fn statement_under_cursor() {
        let sql = "select 1;\nselect 2;\n";
        let at = |c| statement_at(sql, DriverKind::Postgres, c).unwrap().text;
        assert_eq!(at(3), "select 1");
        assert_eq!(at(12), "select 2");
        assert_eq!(at(sql.len()), "select 2");
    }

    #[test]
    fn multibyte_text() {
        assert_eq!(
            texts("select 'zażółć'; select 'ąę'", DriverKind::Postgres),
            ["select 'zażółć'", "select 'ąę'"]
        );
    }
}
