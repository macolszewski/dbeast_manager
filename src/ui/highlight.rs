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
        // Adjacent text of one format is one section: fewer sections make
        // copying, hashing and laying out the job cheaper on every frame.
        if space == 0.0
            && let Some(last) = job.sections.last_mut()
            && last.format.color == color
            && last.format.italics == italic
            && last.byte_range.end.0 == job.text.len()
        {
            job.text.push_str(text);
            last.byte_range.end.0 = job.text.len();
            return;
        }
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
            let color = if KEYWORDS.iter().any(|keyword| keyword.eq_ignore_ascii_case(word)) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn color_at(job: &LayoutJob, needle: &str) -> Color32 {
        let at = job.text.find(needle).unwrap();
        job.sections
            .iter()
            .find(|s| s.byte_range.start.0 <= at && at < s.byte_range.end.0)
            .unwrap()
            .format
            .color
    }

    #[test]
    fn adjacent_tokens_of_one_color_share_a_section() {
        let sql = "select a.id, count(*) from orders a -- note\nwhere a.name = 'Łódź' and x = 42;";
        let job = layout_with_folds(sql, 13.0, &|_| None);
        assert_eq!(job.text, sql);
        assert_eq!(color_at(&job, "select"), theme::SQL_KEYWORD);
        assert_eq!(color_at(&job, "count"), theme::SQL_FUNCTION);
        assert_eq!(color_at(&job, "orders"), theme::TEXT);
        assert_eq!(color_at(&job, "-- note"), theme::SQL_COMMENT);
        assert_eq!(color_at(&job, "'Łódź'"), theme::SQL_STRING);
        assert_eq!(color_at(&job, "42"), theme::SQL_NUMBER);
        assert_eq!(color_at(&job, "WHERE".to_lowercase().as_str()), theme::SQL_KEYWORD);
        // "a.id, " is one plain section, not one per character.
        assert!(job.sections.len() < 30, "{} sections", job.sections.len());
        let mut covered = 0;
        for section in &job.sections {
            assert_eq!(section.byte_range.start.0, covered);
            covered = section.byte_range.end.0;
            assert!(section.byte_range.start.0 < section.byte_range.end.0);
        }
        assert_eq!(covered, sql.len());
        for (a, b) in job.sections.iter().zip(job.sections.iter().skip(1)) {
            assert!(
                a.format.color != b.format.color
                    || a.format.italics != b.format.italics
                    || b.leading_space > 0.0,
                "mergeable neighbours left apart"
            );
        }
    }

    #[test]
    fn fold_placeholders_keep_their_own_section_and_space() {
        let job = layout_with_folds("select 1\u{E000} from t", 13.0, &|c| {
            (c == '\u{E000}').then_some(40.0)
        });
        let placeholder = job
            .sections
            .iter()
            .position(|s| s.format.color == Color32::TRANSPARENT)
            .expect("placeholder section");
        assert_eq!(job.sections[placeholder + 1].leading_space, 40.0);
    }
}
