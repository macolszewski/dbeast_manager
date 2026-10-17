//! Lightweight SQL highlighter with a dark color scheme.

use super::theme;
use egui::Color32;
use egui::text::{LayoutJob, TextFormat};

const KEYWORDS: &[&str] = &[
    "ADD",
    "ALL",
    "ALTER",
    "AND",
    "ANY",
    "AS",
    "ASC",
    "BEGIN",
    "BETWEEN",
    "BY",
    "CASCADE",
    "CASE",
    "CAST",
    "CHECK",
    "COLUMN",
    "COMMIT",
    "CONSTRAINT",
    "CREATE",
    "CROSS",
    "DATABASE",
    "DECLARE",
    "DEFAULT",
    "DELETE",
    "DESC",
    "DISTINCT",
    "DO",
    "DROP",
    "ELSE",
    "END",
    "EXCEPT",
    "EXISTS",
    "EXPLAIN",
    "FALSE",
    "FETCH",
    "FOR",
    "FOREIGN",
    "FROM",
    "FULL",
    "FUNCTION",
    "GRANT",
    "GROUP",
    "HAVING",
    "IF",
    "ILIKE",
    "IN",
    "INDEX",
    "INNER",
    "INSERT",
    "INTERSECT",
    "INTO",
    "IS",
    "JOIN",
    "KEY",
    "LANGUAGE",
    "LATERAL",
    "LEFT",
    "LIKE",
    "LIMIT",
    "NOT",
    "NULL",
    "OFFSET",
    "ON",
    "OR",
    "ORDER",
    "OUTER",
    "OVER",
    "PARTITION",
    "PRIMARY",
    "PROCEDURE",
    "RECURSIVE",
    "REFERENCES",
    "RETURNING",
    "RETURNS",
    "REVOKE",
    "RIGHT",
    "ROLLBACK",
    "SCHEMA",
    "SELECT",
    "SET",
    "SHOW",
    "TABLE",
    "THEN",
    "TO",
    "TRANSACTION",
    "TRIGGER",
    "TRUE",
    "TRUNCATE",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "USE",
    "USING",
    "VALUES",
    "VIEW",
    "WHEN",
    "WHERE",
    "WINDOW",
    "WITH",
    "ANALYZE",
    "VACUUM",
    "REPLACE",
    "TEMP",
    "TEMPORARY",
    "SEQUENCE",
    "EXTENSION",
    "TYPE",
    "ENUM",
    "INT",
    "INTEGER",
    "BIGINT",
    "SMALLINT",
    "SERIAL",
    "BIGSERIAL",
    "TEXT",
    "VARCHAR",
    "CHAR",
    "BOOLEAN",
    "BOOL",
    "NUMERIC",
    "DECIMAL",
    "REAL",
    "DOUBLE",
    "PRECISION",
    "FLOAT",
    "DATE",
    "TIME",
    "TIMESTAMP",
    "TIMESTAMPTZ",
    "INTERVAL",
    "JSON",
    "JSONB",
    "UUID",
    "BYTEA",
    "BLOB",
    "AUTO_INCREMENT",
    "ENGINE",
    "COLLATE",
];

/// Highlighted layout of SQL text. A fold placeholder character is drawn
/// invisible and followed by `fold_width(c)` points of space for the summary
/// painted over it.
pub fn layout_with_folds(
    code: &str,
    font_size: f32,
    fold_width: &dyn Fn(char) -> Option<f32>,
) -> LayoutJob {
    let mut job = LayoutJob::default();
    let font = theme::mono(font_size);
    let leading = std::cell::Cell::new(0.0);
    let mut push = |text: &str, color: Color32, italic: bool| {
        let space = leading.take();
        job.append(
            text,
            space,
            TextFormat {
                font_id: font.clone(),
                color,
                italics: italic,
                ..Default::default()
            },
        );
    };

    let bytes = code.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        let start = i;
        let rest = &code[i..];
        if rest.starts_with("--") || c == b'#' {
            i += rest.find('\n').unwrap_or(rest.len());
            push(&code[start..i], theme::SQL_COMMENT, true);
        } else if let Some(body) = rest.strip_prefix("/*") {
            i += body.find("*/").map_or(rest.len(), |p| p + 4);
            push(&code[start..i], theme::SQL_COMMENT, true);
        } else if c == b'\'' || c == b'"' || c == b'`' {
            i += 1;
            while i < bytes.len() {
                if bytes[i] == c {
                    if bytes.get(i + 1) == Some(&c) {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            let color = if c == b'\'' {
                theme::SQL_STRING
            } else {
                theme::SQL_IDENT
            };
            push(&code[start..i], color, false);
        } else if c.is_ascii_digit() {
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'.') {
                i += 1;
            }
            push(&code[start..i], theme::SQL_NUMBER, false);
        } else if c.is_ascii_alphabetic() || c == b'_' {
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let word = &code[start..i];
            let upper = word.to_ascii_uppercase();
            let color = if KEYWORDS.contains(&upper.as_str()) {
                theme::SQL_KEYWORD
            } else if bytes.get(i) == Some(&b'(') {
                theme::SQL_FUNCTION
            } else {
                theme::TEXT
            };
            push(word, color, false);
        } else {
            let ch = rest.chars().next().unwrap_or(' ');
            i += ch.len_utf8();
            match fold_width(ch) {
                Some(width) => {
                    push(&code[start..i], Color32::TRANSPARENT, false);
                    leading.set(width);
                }
                None => push(&code[start..i], theme::TEXT, false),
            }
        }
    }
    job
}
