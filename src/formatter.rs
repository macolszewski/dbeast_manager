//! SQL reformatting: every clause keyword stands on its own line with its
//! content indented below it, list items and WHERE/HAVING conditions get a
//! line each, joins are indented under FROM, subqueries and window
//! definitions become indented blocks laid out the same way, and reserved
//! keywords are upper-cased.
//!
//! Only whitespace and the case of reserved keywords may change. Every result
//! is compared token by token with the input and rejected otherwise, so a
//! formatting bug can never alter a statement.

use crate::model::DriverKind;

const INDENT: usize = 4;

/// Reserved words written in upper case. Function names and words that are
/// commonly used as identifiers (key, name, status, first, ...) are left alone.
const KEYWORDS: &[&str] = &[
    "ALL",
    "ALTER",
    "AND",
    "AS",
    "ASC",
    "BETWEEN",
    "BY",
    "CASCADE",
    "CASE",
    "CHECK",
    "CONSTRAINT",
    "CREATE",
    "CROSS",
    "DEFAULT",
    "DELETE",
    "DESC",
    "DISTINCT",
    "DROP",
    "ELSE",
    "END",
    "EXCEPT",
    "EXISTS",
    "FALSE",
    "FOREIGN",
    "FROM",
    "FULL",
    "GROUP",
    "HAVING",
    "ILIKE",
    "IN",
    "INDEX",
    "INNER",
    "INSERT",
    "INTERSECT",
    "INTO",
    "IS",
    "JOIN",
    "LEFT",
    "LIKE",
    "LIMIT",
    "NATURAL",
    "NOT",
    "NULL",
    "NULLS",
    "OFFSET",
    "ON",
    "OR",
    "ORDER",
    "OUTER",
    "OVER",
    "PARTITION",
    "PRIMARY",
    "RECURSIVE",
    "REFERENCES",
    "RETURNING",
    "RIGHT",
    "SELECT",
    "SET",
    "TABLE",
    "THEN",
    "TRUE",
    "TRUNCATE",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "USING",
    "VALUES",
    "VIEW",
    "WHEN",
    "WHERE",
    "WITH",
];

fn is_keyword(word: &str) -> bool {
    KEYWORDS.contains(&word.to_ascii_uppercase().as_str())
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Word(String),
    /// String literal, quoted identifier or dollar-quoted body, kept verbatim.
    Quoted(String),
    Number(String),
    LineComment(String),
    BlockComment(String),
    Punct(String),
    Op(String),
}

impl Tok {
    fn text(&self) -> &str {
        match self {
            Tok::Word(s)
            | Tok::Quoted(s)
            | Tok::Number(s)
            | Tok::LineComment(s)
            | Tok::BlockComment(s)
            | Tok::Punct(s)
            | Tok::Op(s) => s,
        }
    }

    fn upper(&self) -> String {
        match self {
            Tok::Word(s) => s.to_ascii_uppercase(),
            _ => String::new(),
        }
    }

    fn is_word(&self, w: &str) -> bool {
        matches!(self, Tok::Word(s) if s.eq_ignore_ascii_case(w))
    }
}

fn tokenize(sql: &str, driver: DriverKind) -> Vec<Tok> {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let at = |i: usize| chars.get(i).copied().unwrap_or('\0');
    while i < chars.len() {
        let c = chars[i];
        let start = i;
        let slice = |a: usize, b: usize| chars[a..b].iter().collect::<String>();
        if c.is_whitespace() {
            i += 1;
        } else if c == '-' && at(i + 1) == '-' || c == '#' && driver == DriverKind::MySql {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            out.push(Tok::LineComment(slice(start, i).trim_end().to_string()));
        } else if c == '/' && at(i + 1) == '*' {
            i += 2;
            while i < chars.len() && !(chars[i] == '*' && at(i + 1) == '/') {
                i += 1;
            }
            i = (i + 2).min(chars.len());
            out.push(Tok::BlockComment(slice(start, i)));
        } else if c == '\'' || c == '"' || c == '`' || ((c == 'E' || c == 'e') && at(i + 1) == '\'')
        {
            let quote = if c == 'E' || c == 'e' {
                i += 1;
                '\''
            } else {
                c
            };
            let backslash = driver == DriverKind::MySql || start != i;
            i += 1;
            while i < chars.len() {
                if backslash && chars[i] == '\\' {
                    i += 2;
                    continue;
                }
                if chars[i] == quote {
                    if at(i + 1) == quote {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            i = i.min(chars.len());
            out.push(Tok::Quoted(slice(start, i)));
        } else if c == '$' && driver == DriverKind::Postgres && dollar_tag(&chars[i..]).is_some() {
            let tag = dollar_tag(&chars[i..]).unwrap();
            let tag_chars: Vec<char> = tag.chars().collect();
            i += tag_chars.len();
            while i < chars.len() && !chars[i..].starts_with(&tag_chars) {
                i += 1;
            }
            i = (i + tag_chars.len()).min(chars.len());
            out.push(Tok::Quoted(slice(start, i)));
        } else if c.is_ascii_digit() || (c == '.' && at(i + 1).is_ascii_digit()) {
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '.') {
                // `1..2` is not a number; stop before a second dot.
                if chars[i] == '.' && at(i + 1) == '.' {
                    break;
                }
                i += 1;
            }
            out.push(Tok::Number(slice(start, i)));
        } else if c.is_alphabetic()
            || c == '_'
            || (c == '$' && at(i + 1).is_ascii_digit())
            || c == '@'
        {
            i += 1;
            while i < chars.len()
                && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$')
            {
                i += 1;
            }
            out.push(Tok::Word(slice(start, i)));
        } else if matches!(c, '(' | ')' | ',' | ';' | '.' | '[' | ']') {
            i += 1;
            out.push(Tok::Punct(c.to_string()));
        } else if c == ':' && at(i + 1) == ':' {
            i += 2;
            out.push(Tok::Punct("::".into()));
        } else {
            // Operators: longest run of operator characters, e.g. <=, ||, ->>, @>.
            i += 1;
            while i < chars.len()
                && "<>=!|&+-*/%~^@#?:".contains(chars[i])
                && !(chars[i] == '-' && at(i + 1) == '-')
            {
                i += 1;
            }
            out.push(Tok::Op(slice(start, i)));
        }
    }
    out
}

/// `$tag$` at the start of `chars`, if it opens a dollar-quoted string.
fn dollar_tag(chars: &[char]) -> Option<String> {
    let end = chars.iter().skip(1).position(|&c| c == '$')? + 1;
    let tag: String = chars[1..end].iter().collect();
    let valid = tag
        .chars()
        .enumerate()
        .all(|(i, c)| c == '_' || c.is_alphabetic() || (i > 0 && c.is_ascii_digit()));
    valid.then(|| format!("${tag}$"))
}

/// Block of output: a statement, or the inside of a multi-line parenthesis.
struct Block {
    /// Column of clause keywords; their content is one level deeper.
    indent: usize,
    /// Inline parentheses open inside this block (function calls, IN lists).
    inline_depth: usize,
    /// Commas break lines and align to this column (clause content, column lists).
    list_indent: Option<usize>,
    /// Clause being laid out (WHERE, ORDER BY, ...); empty before the first one.
    clause: String,
    /// A BETWEEN is waiting for its AND, which must stay on the same line.
    between: bool,
}

impl Block {
    fn new(indent: usize) -> Self {
        Self { indent, inline_depth: 0, list_indent: None, clause: String::new(), between: false }
    }

    /// Indentation of the current clause's content.
    fn content(&self) -> usize {
        if self.clause.is_empty() { self.indent } else { self.indent + INDENT }
    }
}

struct Formatter {
    out: String,
    blocks: Vec<Block>,
    /// Per open parenthesis: the indentation of the line it opened on, when it
    /// was laid out as a block; `None` for inline parentheses.
    parens: Vec<Option<usize>>,
    statement_first: String,
    /// Main command of the statement (SELECT, INSERT, UPDATE, DELETE), also after a WITH.
    verb: String,
    no_space_next: bool,
}

impl Formatter {
    fn block(&mut self) -> &mut Block {
        self.blocks.last_mut().unwrap()
    }

    fn line_start(&self) -> usize {
        let line = self.out.rsplit('\n').next().unwrap_or("");
        line.len() - line.trim_start().len()
    }

    fn at_line_start(&self) -> bool {
        self.out.rsplit('\n').next().unwrap_or("").trim().is_empty()
    }

    fn newline(&mut self, indent: usize) {
        let trimmed = self.out.trim_end_matches(' ').len();
        self.out.truncate(trimmed);
        if !self.out.is_empty() && !self.out.ends_with('\n') {
            self.out.push('\n');
        } else if self.out.ends_with('\n') {
            // Already on a fresh line: just indent.
        }
        self.out.push_str(&" ".repeat(indent));
    }

    fn push(&mut self, text: &str, space_before: bool) {
        if space_before && !self.at_line_start() && !self.no_space_next {
            self.out.push(' ');
        }
        self.no_space_next = false;
        self.out.push_str(text);
    }
}

/// Reformats a script. Returns an error, and changes nothing, when the result
/// would differ from the input in anything but whitespace and keyword case.
pub fn format(sql: &str, driver: DriverKind) -> Result<String, String> {
    let tokens = tokenize(sql, driver);
    let formatted = layout(&tokens);
    if canonical(&tokenize(&formatted, driver)) != canonical(&tokens) {
        return Err("formatting would change the statement, left as it was".into());
    }
    Ok(formatted)
}

/// Token stream with whitespace removed and keywords in one case, used to
/// prove that formatting kept the statement intact.
fn canonical(tokens: &[Tok]) -> Vec<String> {
    tokens
        .iter()
        .map(|t| match t {
            Tok::Word(w)
                if is_keyword(w)
                    || matches!(w.to_ascii_uppercase().as_str(), "KEY" | "FIRST" | "LAST") =>
            {
                w.to_ascii_uppercase()
            }
            other => other.text().to_string(),
        })
        .collect()
}

fn layout(tokens: &[Tok]) -> String {
    let mut f = Formatter {
        out: String::new(),
        blocks: vec![Block::new(0)],
        parens: Vec::new(),
        statement_first: String::new(),
        verb: String::new(),
        no_space_next: false,
    };
    let next_word = |i: usize, n: usize| tokens.get(i + n).map(Tok::upper).unwrap_or_default();
    let mut i = 0;
    while i < tokens.len() {
        let tok = &tokens[i];
        let prev = if i > 0 { Some(&tokens[i - 1]) } else { None };
        let upper = tok.upper();
        let at_block_level = f.blocks.last().unwrap().inline_depth == 0;
        match tok {
            Tok::LineComment(c) => {
                if !f.at_line_start() {
                    f.out.push(' ');
                }
                f.out.push_str(c);
                let indent = f.block().content();
                f.newline(indent);
            }
            Tok::BlockComment(c) => f.push(c, true),
            Tok::Punct(p) if p == ";" => {
                f.out.push(';');
                f.blocks.truncate(1);
                f.parens.clear();
                *f.block() = Block::new(0);
                f.statement_first.clear();
                f.verb.clear();
                if i + 1 < tokens.len() {
                    // Keep a trailing comment on the statement's line.
                    if let Some(Tok::LineComment(_)) = tokens.get(i + 1) {
                        i += 1;
                        f.out.push(' ');
                        f.out.push_str(tokens[i].text());
                    }
                    f.out.push('\n');
                    if i + 1 < tokens.len() {
                        f.out.push('\n');
                    }
                }
            }
            Tok::Punct(p) if p == "," => {
                f.out.push(',');
                let block = f.blocks.last().unwrap();
                match block.list_indent {
                    Some(col) if block.inline_depth == 0 => f.newline(col),
                    _ => {}
                }
            }
            Tok::Punct(p) if p == "(" => {
                // `INSERT INTO t (a, b)` and `CREATE TABLE t (...)` keep a space, calls `f(x)` do not.
                let before_prev = if i >= 2 { tokens.get(i - 2) } else { None };
                let names_table = before_prev
                    .is_some_and(|t| t.is_word("INTO") || t.is_word("TABLE") || t.is_word("VIEW"))
                    || (before_prev.is_some_and(|t| t.is_word("EXISTS"))
                        && i >= 5
                        && tokens[i - 5].is_word("TABLE"));
                let space = match prev {
                    Some(Tok::Word(w)) => is_keyword(w) || names_table,
                    Some(Tok::Punct(p)) => p == ",",
                    Some(Tok::Op(_)) => true,
                    Some(Tok::Quoted(q)) => !q.starts_with('"') && !q.starts_with('`'),
                    _ => true,
                };
                f.push("(", space);
                let subquery = matches!(next_word(i, 1).as_str(), "SELECT" | "WITH" | "VALUES");
                let window = prev.is_some_and(|p| p.is_word("OVER"));
                let column_list =
                    f.statement_first == "CREATE" && names_table && f.parens.is_empty();
                if subquery || window || column_list {
                    let open = f.line_start();
                    let indent = open + INDENT;
                    f.parens.push(Some(open));
                    let mut block = Block::new(indent);
                    block.list_indent = column_list.then_some(indent);
                    f.blocks.push(block);
                    f.newline(indent);
                } else {
                    f.parens.push(None);
                    f.block().inline_depth += 1;
                }
                f.no_space_next = true;
            }
            Tok::Punct(p) if p == ")" => {
                match f.parens.pop() {
                    Some(Some(open)) => {
                        f.blocks.pop();
                        f.newline(open);
                        f.out.push(')');
                    }
                    Some(None) => {
                        let b = f.block();
                        b.inline_depth = b.inline_depth.saturating_sub(1);
                        f.out.push(')');
                    }
                    None => f.out.push(')'),
                }
                f.no_space_next = false;
            }
            Tok::Punct(p) if p == "." || p == "::" || p == "[" || p == "]" => {
                f.out.push_str(p);
                f.no_space_next = p != "]";
            }
            Tok::Op(op) => {
                let unary = (op == "-" || op == "+")
                    && match prev {
                        None => true,
                        Some(Tok::Op(_)) => true,
                        Some(Tok::Punct(p)) => p == "(" || p == ",",
                        Some(Tok::Word(w)) => is_keyword(w),
                        _ => false,
                    };
                let star_list =
                    op == "*" && matches!(prev, Some(Tok::Punct(p)) if p == "(" || p == ".");
                f.push(op, !star_list);
                if unary {
                    f.no_space_next = true;
                }
            }
            Tok::Word(w) => {
                if f.statement_first.is_empty() {
                    f.statement_first = upper.clone();
                }
                let main_command = f.blocks.len() == 1
                    && matches!(upper.as_str(), "SELECT" | "INSERT" | "UPDATE" | "DELETE")
                    && (f.statement_first == upper || f.verb == "WITH");
                let leading_with = f.verb.is_empty() && upper == "WITH";
                if main_command || leading_with {
                    f.verb = upper.clone();
                }
                // Words that are keywords only in context, otherwise often column names.
                let contextual = match upper.as_str() {
                    "KEY" => prev.is_some_and(|p| p.is_word("PRIMARY") || p.is_word("FOREIGN")),
                    "FIRST" | "LAST" => prev.is_some_and(|p| p.is_word("NULLS")),
                    _ => false,
                };
                let text = if is_keyword(w) || contextual {
                    upper.clone()
                } else {
                    w.clone()
                };
                let clause = at_block_level
                    && (main_command
                        || leading_with
                        || starts_clause(&upper, prev, tokens.get(i + 1), &f.statement_first, &f.verb));
                if clause {
                    let indent = f.block().indent;
                    match clause_layout(&upper, &next_word(i, 1)) {
                        ClauseLayout::Join => {
                            // Joins continue the FROM content.
                            f.newline(indent + INDENT);
                            f.push(&text, true);
                        }
                        ClauseLayout::Alone(words) => {
                            f.block().clause.clear();
                            f.block().list_indent = None;
                            f.newline(indent);
                            f.push(&text, true);
                            for _ in 0..words {
                                i += 1;
                                f.push(&tokens[i].upper(), true);
                            }
                        }
                        ClauseLayout::WithContent(words) => {
                            f.newline(indent);
                            f.push(&text, true);
                            for _ in 0..words {
                                i += 1;
                                f.push(&tokens[i].upper(), true);
                            }
                            let block = f.block();
                            block.clause = upper.clone();
                            block.between = false;
                            block.list_indent = Some(indent + INDENT);
                            f.newline(indent + INDENT);
                        }
                    }
                    i += 1;
                    continue;
                }
                // Conditions of WHERE and HAVING get a line each, except BETWEEN's AND.
                let block = f.blocks.last().unwrap();
                let condition = at_block_level && matches!(block.clause.as_str(), "WHERE" | "HAVING");
                if condition && upper == "BETWEEN" {
                    f.block().between = true;
                } else if condition && (upper == "AND" || upper == "OR") {
                    if upper == "AND" && block.between {
                        f.block().between = false;
                    } else {
                        let content = block.content();
                        f.newline(content);
                    }
                }
                f.push(&text, true);
            }
            Tok::Quoted(q) => f.push(q, true),
            Tok::Number(n) => f.push(n, true),
            Tok::Punct(p) => f.push(p, true),
        }
        i += 1;
    }
    let mut out = f.out.trim_end().to_string();
    out.push('\n');
    out.trim_start_matches('\n').to_string()
}

enum ClauseLayout {
    /// Keyword phrase on its own line, content indented below; the number is
    /// how many following words belong to the phrase (ORDER BY, DELETE FROM).
    WithContent(usize),
    /// Keyword phrase alone on its line: UNION ALL, WITH.
    Alone(usize),
    /// JOIN variants, indented under FROM with their condition on the line.
    Join,
}

fn clause_layout(upper: &str, next: &str) -> ClauseLayout {
    match upper {
        "GROUP" | "ORDER" | "PARTITION" => ClauseLayout::WithContent(1),
        "INSERT" if next == "INTO" => ClauseLayout::WithContent(1),
        "DELETE" if next == "FROM" => ClauseLayout::WithContent(1),
        "SELECT" if next == "DISTINCT" => ClauseLayout::WithContent(1),
        "UNION" | "EXCEPT" | "INTERSECT" if matches!(next, "ALL" | "DISTINCT") => ClauseLayout::Alone(1),
        "UNION" | "EXCEPT" | "INTERSECT" | "WITH" => ClauseLayout::Alone(0),
        "JOIN" | "LEFT" | "RIGHT" | "FULL" | "INNER" | "CROSS" | "NATURAL" => ClauseLayout::Join,
        _ => ClauseLayout::WithContent(0),
    }
}

/// Whether a word starts a new line as a clause keyword.
fn starts_clause(
    upper: &str,
    prev: Option<&Tok>,
    next: Option<&Tok>,
    statement_first: &str,
    verb: &str,
) -> bool {
    let prev_upper = prev.map(Tok::upper).unwrap_or_default();
    let next_upper = next.map(Tok::upper).unwrap_or_default();
    match upper {
        "SELECT" => true,
        "FROM" => {
            !matches!(prev_upper.as_str(), "DELETE" | "DISTINCT") && statement_first != "TRUNCATE"
        }
        "WHERE" | "HAVING" | "LIMIT" | "OFFSET" | "RETURNING" | "WINDOW" | "VALUES" => true,
        "GROUP" | "ORDER" | "PARTITION" => next_upper == "BY",
        "UNION" | "EXCEPT" | "INTERSECT" => true,
        "SET" => verb == "UPDATE",
        "USING" => verb == "DELETE",
        "JOIN" => !matches!(
            prev_upper.as_str(),
            "LEFT" | "RIGHT" | "FULL" | "INNER" | "CROSS" | "OUTER" | "NATURAL"
        ),
        "LEFT" | "RIGHT" | "FULL" | "INNER" | "CROSS" | "NATURAL" => {
            matches!(next_upper.as_str(), "JOIN" | "OUTER") && prev_upper != "NATURAL"
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(sql: &str) -> String {
        format(sql, DriverKind::Postgres).unwrap()
    }

    #[test]
    fn cte_with_window_and_subquery() {
        let sql = "-- remove duplicates\nwith ranked as (select id, row_number() over (partition by regexp_replace(translation_key, '^\\s+|\\s+$', '', 'g'), language_code order by last_change desc, id) as rn from translations_translation where translation_key in (select translation_key from translations_translation where translation_key ~ '^\\s|\\s$'))\ndelete from translations_translation t using ranked r where t.id = r.id and r.rn > 1;";
        let expected = "\
-- remove duplicates
WITH ranked AS (
    SELECT
        id,
        row_number() OVER (
            PARTITION BY
                regexp_replace(translation_key, '^\\s+|\\s+$', '', 'g'),
                language_code
            ORDER BY
                last_change DESC,
                id
        ) AS rn
    FROM
        translations_translation
    WHERE
        translation_key IN (
            SELECT
                translation_key
            FROM
                translations_translation
            WHERE
                translation_key ~ '^\\s|\\s$'
        )
)
DELETE FROM
    translations_translation t
USING
    ranked r
WHERE
    t.id = r.id
    AND r.rn > 1;
";
        assert_eq!(fmt(sql), expected);
    }

    #[test]
    fn clause_content_is_indented() {
        let sql = "SELECT * FROM dzialki WHERE identyfika IN ('ass', 'asds', 'ddf') and pow between 1 and 2 or aktywna LIMIT 5;";
        let expected = "\
SELECT
    *
FROM
    dzialki
WHERE
    identyfika IN ('ass', 'asds', 'ddf')
    AND pow BETWEEN 1 AND 2
    OR aktywna
LIMIT
    5;
";
        assert_eq!(fmt(sql), expected);
    }

    #[test]
    fn several_statements_joins_and_set() {
        let sql = "select a.x, count(*) from a left join b on a.id = b.a_id group by a.x having count(*) > 1 order by 2 desc limit 10; update shop.orders set total = total::numeric(12,2) * 2, paid = true where id = $1";
        let expected = "\
SELECT
    a.x,
    count(*)
FROM
    a
    LEFT JOIN b ON a.id = b.a_id
GROUP BY
    a.x
HAVING
    count(*) > 1
ORDER BY
    2 DESC
LIMIT
    10;

UPDATE
    shop.orders
SET
    total = total::numeric(12, 2) * 2,
    paid = TRUE
WHERE
    id = $1
";
        assert_eq!(fmt(sql), expected);
    }

    #[test]
    fn insert_create_and_literals_are_kept() {
        let sql = "insert into t (a, b) values (1, -2), ('x;y', E'it\\'s');\ncreate table t2 (id int primary key, name text not null default 'n/a');\ncreate function f() returns int as $$ select   1; $$ language sql;\ngrant insert, update on t to u;";
        let out = fmt(sql);
        assert!(
            out.starts_with("INSERT INTO\n    t (a, b)\nVALUES\n    (1, -2),\n    ('x;y', E'it\\'s');\n\n"),
            "{out}"
        );
        assert!(out.contains("CREATE TABLE t2 (\n    id int PRIMARY KEY,\n    name text NOT NULL DEFAULT 'n/a'\n);"), "{out}");
        assert!(out.contains("$$ select   1; $$"), "{out}");
        assert!(out.contains("INSERT, UPDATE ON t"), "privileges are not clauses: {out}");
    }

    #[test]
    fn identifiers_keep_their_case_and_formatting_is_stable() {
        let sql = "select \"Name\", status, key, first from `t` where x is not null";
        let out = format(sql, DriverKind::MySql).unwrap();
        assert!(out.contains("status,\n    key,\n    first"), "{out}");
        let again = format(&out, DriverKind::MySql).unwrap();
        assert_eq!(out, again, "formatting is idempotent");
    }

    #[test]
    fn set_operators_stand_on_their_own_line() {
        assert_eq!(
            fmt("select id from a union all select id from b"),
            "SELECT\n    id\nFROM\n    a\nUNION ALL\nSELECT\n    id\nFROM\n    b\n"
        );
    }

    #[test]
    fn real_scripts_format_and_stay_stable() {
        for (sql, driver) in [
            (include_str!("../tests/seed_pg.sql"), DriverKind::Postgres),
            (include_str!("../tests/seed_mysql.sql"), DriverKind::MySql),
        ] {
            let once = format(sql, driver).unwrap();
            assert_eq!(
                format(&once, driver).unwrap(),
                once,
                "second pass changes nothing"
            );
            let statements = |s: &str| crate::splitter::split(s, driver).len();
            assert_eq!(statements(&once), statements(sql));
        }
    }

    #[test]
    fn rejects_results_that_change_tokens() {
        let tokens = tokenize("select a", DriverKind::Postgres);
        let changed = tokenize("select b", DriverKind::Postgres);
        assert_ne!(canonical(&tokens), canonical(&changed));
        let recased = tokenize("SELECT a", DriverKind::Postgres);
        assert_eq!(canonical(&tokens), canonical(&recased));
    }
}
