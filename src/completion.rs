//! Context-aware SQL completion over cached table metadata.

use crate::model::{DriverKind, TableMeta};
use crate::splitter;
use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemKind {
    Keyword,
    Schema,
    Table,
    View,
    Column,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub label: String,
    pub kind: ItemKind,
    /// Grey hint: column type and table, or the schema of a table.
    pub detail: String,
    pub insert: String,
}

#[derive(Debug, PartialEq)]
pub struct Completion {
    /// Byte range in the script that the chosen item replaces.
    pub replace: Range<usize>,
    pub items: Vec<Item>,
}

const KEYWORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "AND",
    "OR",
    "NOT",
    "NULL",
    "IS",
    "IN",
    "EXISTS",
    "BETWEEN",
    "LIKE",
    "ILIKE",
    "JOIN",
    "LEFT JOIN",
    "RIGHT JOIN",
    "INNER JOIN",
    "FULL JOIN",
    "CROSS JOIN",
    "ON",
    "USING",
    "GROUP BY",
    "ORDER BY",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "DISTINCT",
    "AS",
    "ASC",
    "DESC",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "INSERT INTO",
    "VALUES",
    "UPDATE",
    "SET",
    "DELETE FROM",
    "RETURNING",
    "WITH",
    "UNION",
    "UNION ALL",
    "EXCEPT",
    "INTERSECT",
    "CREATE TABLE",
    "ALTER TABLE",
    "DROP TABLE",
    "CREATE INDEX",
    "TRUNCATE",
    "EXPLAIN",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "COUNT",
    "SUM",
    "AVG",
    "MIN",
    "MAX",
    "COALESCE",
    "CAST",
    "TRUE",
    "FALSE",
];

/// Words after which a table name is expected.
const TABLE_CONTEXT: &[&str] = &["FROM", "JOIN", "UPDATE", "INTO", "TABLE", "TRUNCATE"];

/// Words that end a table reference list, so they are never taken as aliases.
const NOT_ALIAS: &[&str] = &[
    "WHERE",
    "ON",
    "USING",
    "JOIN",
    "LEFT",
    "RIGHT",
    "INNER",
    "FULL",
    "CROSS",
    "OUTER",
    "GROUP",
    "ORDER",
    "LIMIT",
    "SET",
    "VALUES",
    "HAVING",
    "UNION",
    "RETURNING",
    "NATURAL",
    "WINDOW",
    "OFFSET",
    "FOR",
];

pub fn complete(script: &str, cursor: usize, driver: DriverKind, meta: &[TableMeta]) -> Completion {
    complete_impl(script, cursor, driver, meta, None)
}

/// Resolve bare table names in the server's actual ordered search path.
/// Tables hidden by another schema are inserted with their schema qualifier.
pub fn complete_in_context(
    script: &str,
    cursor: usize,
    driver: DriverKind,
    meta: &[TableMeta],
    search_path: &[String],
) -> Completion {
    complete_impl(script, cursor, driver, meta, Some(search_path))
}

fn complete_impl(
    script: &str,
    cursor: usize,
    driver: DriverKind,
    meta: &[TableMeta],
    search_path: Option<&[String]>,
) -> Completion {
    let mut cursor = cursor.min(script.len());
    while !script.is_char_boundary(cursor) {
        cursor -= 1;
    }
    let before = &script[..cursor];
    let prefix_start = before
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_ident_char(*c))
        .last()
        .map_or(cursor, |(i, _)| i);
    let prefix = &script[prefix_start..cursor];
    let qualifier = before[..prefix_start]
        .strip_suffix('.')
        .and_then(|q| tokenize(q).pop());
    // One pass over metadata per completion, never a metadata scan per item.
    let mut visible = std::collections::HashMap::<&str, (usize, &TableMeta)>::new();
    if let Some(path) = search_path {
        for table in meta {
            if let Some(rank) = path.iter().position(|schema| schema == &table.schema) {
                let entry = visible.entry(table.name.as_str()).or_insert((rank, table));
                if rank < entry.0 {
                    *entry = (rank, table);
                }
            }
        }
    }
    let resolved_table = |schema: Option<&str>, name: &str| {
        if let Some(path) = search_path {
            find_table_in_context(meta, driver, schema, name, path)
        } else {
            find_table(meta, schema, name)
        }
    };
    let candidate = |table: &TableMeta| {
        let mut item = table_item(table, driver);
        if search_path.is_some()
            && !visible
                .get(table.name.as_str())
                .is_some_and(|(_, found)| std::ptr::eq(*found, table))
        {
            item.insert = format!("{}.{}", quote_if_needed(driver, &table.schema), item.insert);
        }
        item
    };

    let statement = splitter::statement_at(script, driver, cursor)
        .map(|s| &script[s.range.start.min(cursor)..s.range.end.max(cursor)])
        .unwrap_or_default();
    let refs = table_refs(statement);
    let in_scope: Vec<(&TableMeta, Option<&str>)> = refs
        .iter()
        .filter_map(|r| {
            resolved_table(r.schema.as_deref(), &r.table).map(|t| (t, r.alias.as_deref()))
        })
        .collect();

    let mut items = Vec::new();
    if let Some(q) = qualifier {
        // alias.col, table.col or schema.table
        let target = in_scope
            .iter()
            .find(|(t, alias)| {
                alias.is_some_and(|a| references_match(driver, a, &q))
                    || identifier_matches(driver, &t.name, &q)
            })
            .map(|(t, _)| *t)
            .or_else(|| {
                let (schema, name) = split_qualified(&q)
                    .map_or((None, q.as_str()), |(schema, name)| (Some(schema), name));
                resolved_table(schema, name)
            });
        if let Some(t) = target {
            items.extend(columns_of(t, driver));
        }
        items.extend(
            meta.iter()
                .filter(|t| identifier_matches(driver, &t.schema, &q))
                .map(|t| table_item(t, driver)),
        );
    } else if previous_word(&before[..prefix_start])
        .is_some_and(|w| TABLE_CONTEXT.contains(&w.as_str()))
    {
        items.extend(meta.iter().map(&candidate));
        let mut schemas: Vec<&str> = meta.iter().map(|t| t.schema.as_str()).collect();
        schemas.dedup();
        items.extend(schemas.into_iter().map(|s| Item {
            label: s.to_string(),
            kind: ItemKind::Schema,
            detail: "schema".into(),
            insert: quote_if_needed(driver, s),
        }));
    } else {
        let lower = prefix.chars().next().is_some_and(|c| c.is_lowercase());
        let keywords = |words: &[&str]| keyword_items(words, lower);
        let columns: Vec<Item> = in_scope
            .iter()
            .flat_map(|(t, _)| columns_of(t, driver))
            .collect();
        let stmt_start = splitter::statement_at(script, driver, cursor)
            .map_or(0, |s| s.range.start.min(prefix_start));
        let tokens = tokenize(&script[stmt_start..prefix_start]);
        let prev = tokens.last().map(|t| t.to_uppercase()).unwrap_or_default();
        let prev_is_column = tokens.last().is_some_and(|t| {
            let name = unquote(t.rsplit('.').next().unwrap_or(t));
            in_scope.iter().any(|(tab, _)| {
                tab.columns
                    .iter()
                    .any(|(c, _)| c.eq_ignore_ascii_case(&name))
            })
        });
        let after_term = ends_term(&prev);
        match clause(&tokens) {
            Some(Clause::OrderBy) => match prev.as_str() {
                "ASC" | "DESC" => {
                    items.extend(keywords(&["NULLS FIRST", "NULLS LAST", "LIMIT", "OFFSET"]))
                }
                "NULLS" => items.extend(keywords(&["FIRST", "LAST"])),
                "FIRST" | "LAST" => items.extend(keywords(&["LIMIT", "OFFSET"])),
                _ if after_term => items.extend(keywords(&[
                    "ASC",
                    "DESC",
                    "NULLS FIRST",
                    "NULLS LAST",
                    "LIMIT",
                    "OFFSET",
                ])),
                _ => items.extend(columns),
            },
            Some(Clause::GroupBy) if after_term => {
                items.extend(keywords(&["HAVING", "ORDER BY", "LIMIT"]))
            }
            Some(Clause::GroupBy) => items.extend(columns),
            Some(Clause::Condition { on }) => {
                if prev_is_column {
                    let mut ops = vec!["IS NULL", "IS NOT NULL", "IN", "NOT IN", "LIKE", "BETWEEN"];
                    if driver == DriverKind::Postgres {
                        ops.push("ILIKE");
                    }
                    items.extend(keywords(&ops));
                } else if prev == "IS" {
                    items.extend(keywords(&["NULL", "NOT NULL", "TRUE", "FALSE"]));
                } else if after_term {
                    items.extend(keywords(&["AND", "OR"]));
                    if on {
                        items.extend(keywords(&["JOIN", "LEFT JOIN", "WHERE"]));
                    }
                    items.extend(keywords(&["ORDER BY", "GROUP BY", "LIMIT"]));
                } else {
                    items.extend(columns);
                    items.extend(keywords(&[
                        "NOT", "EXISTS", "NULL", "TRUE", "FALSE", "CASE",
                    ]));
                }
            }
            Some(Clause::Select) if after_term => items.extend(keywords(&["FROM", "AS"])),
            Some(Clause::Select) => {
                items.extend(columns);
                items.extend(keywords(&[
                    "DISTINCT", "COUNT", "SUM", "AVG", "MIN", "MAX", "COALESCE", "CASE", "CAST",
                ]));
            }
            Some(Clause::Set) if prev_is_column => {}
            Some(Clause::Set) if after_term => items.extend(keywords(&["WHERE"])),
            Some(Clause::Set) => items.extend(columns),
            Some(Clause::Limit) if after_term => items.extend(keywords(&["OFFSET"])),
            Some(Clause::Limit) => {}
            None => {
                items.extend(columns);
                items.extend(keywords(KEYWORDS));
                items.extend(meta.iter().map(&candidate));
            }
        }
    }

    Completion {
        replace: prefix_start..cursor,
        items: rank(items, prefix),
    }
}

/// Clause the completion point is in, which decides what makes sense there.
#[derive(Debug, PartialEq)]
enum Clause {
    Select,
    /// WHERE, HAVING, or a join's ON condition.
    Condition {
        on: bool,
    },
    GroupBy,
    OrderBy,
    /// SET of an UPDATE.
    Set,
    Limit,
}

/// Finds the innermost clause keyword before the end of `tokens`, skipping
/// parenthesized parts that are already closed.
fn clause(tokens: &[String]) -> Option<Clause> {
    let mut depth = 0i32;
    for (i, t) in tokens.iter().enumerate().rev() {
        match t.as_str() {
            ")" => depth += 1,
            "(" if depth > 0 => depth -= 1,
            // Inside an open parenthesis: function arguments or a subquery start.
            "(" => return Some(Clause::Select),
            _ if depth > 0 => {}
            _ => {
                let upper = t.to_uppercase();
                let by_of =
                    |w: &str| upper == "BY" && i > 0 && tokens[i - 1].eq_ignore_ascii_case(w);
                match upper.as_str() {
                    "SELECT" | "DISTINCT" => return Some(Clause::Select),
                    "WHERE" | "HAVING" => return Some(Clause::Condition { on: false }),
                    "ON" => return Some(Clause::Condition { on: true }),
                    "SET" => return Some(Clause::Set),
                    "LIMIT" | "OFFSET" => return Some(Clause::Limit),
                    _ if by_of("ORDER") => return Some(Clause::OrderBy),
                    _ if by_of("GROUP") => return Some(Clause::GroupBy),
                    "FROM" | "JOIN" | "UPDATE" | "INTO" | "VALUES" | "RETURNING" => return None,
                    _ => {}
                }
            }
        }
    }
    None
}

/// Words that never end an expression, so what follows them is a new operand.
const OPERATOR_WORDS: &[&str] = &[
    "SELECT", "DISTINCT", "WHERE", "HAVING", "ON", "AND", "OR", "NOT", "BY", "SET", "IS", "IN",
    "LIKE", "ILIKE", "BETWEEN", "AS", "CASE", "WHEN", "THEN", "ELSE", "LIMIT", "OFFSET", "FROM",
    "JOIN", "EXISTS", "NULLS",
];

/// True when the previous token completes an operand (a column, value or
/// closing parenthesis), so a keyword or operator is expected next.
fn ends_term(prev: &str) -> bool {
    if prev.is_empty() || OPERATOR_WORDS.contains(&prev) {
        return false;
    }
    prev == ")"
        || prev == "*"
        || prev.starts_with(|c: char| {
            c.is_alphanumeric() || c == '_' || c == '"' || c == '`' || c == '\''
        })
}

fn keyword_items(words: &[&str], lower: bool) -> Vec<Item> {
    words
        .iter()
        .map(|k| Item {
            label: k.to_string(),
            kind: ItemKind::Keyword,
            detail: String::new(),
            insert: if lower {
                k.to_lowercase()
            } else {
                k.to_string()
            },
        })
        .collect()
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn unquote(s: &str) -> String {
    if let Some(quote) = s.chars().next().filter(|c| matches!(c, '"' | '`'))
        && s.len() >= 2
        && s.ends_with(quote)
    {
        s[1..s.len() - 1].replace(&format!("{quote}{quote}"), &quote.to_string())
    } else {
        s.to_string()
    }
}

/// The word right before the completion point, ignoring whitespace.
fn previous_word(text: &str) -> Option<String> {
    let word: String = text
        .trim_end()
        .chars()
        .rev()
        .take_while(|c| c.is_alphabetic())
        .collect();
    (!word.is_empty()).then(|| word.chars().rev().collect::<String>().to_uppercase())
}

fn find_table<'a>(
    meta: &'a [TableMeta],
    schema: Option<&str>,
    name: &str,
) -> Option<&'a TableMeta> {
    let matches = |t: &&TableMeta| {
        t.name.eq_ignore_ascii_case(&unquote(name))
            && schema.is_none_or(|s| t.schema.eq_ignore_ascii_case(&unquote(s)))
    };
    // Prefer the default schemas when a bare name exists in several.
    meta.iter()
        .filter(matches)
        .min_by_key(|t| !matches!(t.schema.as_str(), "public" | "main"))
}

fn references_match(driver: DriverKind, left: &str, right: &str) -> bool {
    let actual = if left.starts_with(['"', '`']) {
        unquote(left)
    } else if driver == DriverKind::Postgres {
        left.to_ascii_lowercase()
    } else {
        left.to_owned()
    };
    identifier_matches(driver, &actual, right)
}

fn identifier_matches(driver: DriverKind, actual: &str, reference: &str) -> bool {
    if reference.starts_with(['"', '`']) {
        actual == unquote(reference)
    } else if driver == DriverKind::Postgres {
        actual.len() == reference.len()
            && actual
                .bytes()
                .zip(reference.bytes())
                .all(|(actual, typed)| actual == typed.to_ascii_lowercase())
    } else {
        actual.eq_ignore_ascii_case(reference)
    }
}

fn find_table_in_context<'a>(
    meta: &'a [TableMeta],
    driver: DriverKind,
    schema: Option<&str>,
    name: &str,
    path: &[String],
) -> Option<&'a TableMeta> {
    if let Some(schema) = schema {
        return meta.iter().find(|t| {
            identifier_matches(driver, &t.schema, schema)
                && identifier_matches(driver, &t.name, name)
        });
    }
    path.iter().find_map(|schema| {
        meta.iter()
            .find(|t| t.schema == *schema && identifier_matches(driver, &t.name, name))
    })
}

/// Find the last separator outside quoted identifiers, including escaped quotes.
fn split_qualified(name: &str) -> Option<(&str, &str)> {
    let mut quote = None;
    let mut last = None;
    let mut chars = name.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        if let Some(q) = quote {
            if ch == q {
                if chars.peek().is_some_and(|(_, next)| *next == q) {
                    chars.next();
                } else {
                    quote = None;
                }
            }
        } else if matches!(ch, '"' | '`') {
            quote = Some(ch);
        } else if ch == '.' {
            last = Some(index);
        }
    }
    last.map(|index| (&name[..index], &name[index + 1..]))
}

fn table_item(t: &TableMeta, driver: DriverKind) -> Item {
    Item {
        label: t.name.clone(),
        kind: if t.is_view {
            ItemKind::View
        } else {
            ItemKind::Table
        },
        detail: t.schema.clone(),
        insert: quote_if_needed(driver, &t.name),
    }
}

fn columns_of(t: &TableMeta, driver: DriverKind) -> Vec<Item> {
    t.columns
        .iter()
        .map(|(name, ty)| Item {
            label: name.clone(),
            kind: ItemKind::Column,
            detail: format!("{ty}  {}", t.name),
            insert: quote_if_needed(driver, name),
        })
        .collect()
}

fn quote_if_needed(driver: DriverKind, name: &str) -> String {
    let plain = name
        .chars()
        .next()
        .is_some_and(|c| c.is_alphabetic() || c == '_')
        && name.chars().all(is_ident_char)
        && (driver != DriverKind::Postgres || !name.chars().any(char::is_uppercase))
        && !KEYWORDS
            .iter()
            .chain(NOT_ALIAS)
            .chain(&[
                "USER",
                "CURRENT_USER",
                "SESSION_USER",
                "PRIMARY",
                "REFERENCES",
                "CHECK",
            ])
            .any(|word| name.eq_ignore_ascii_case(word));
    if plain {
        name.to_string()
    } else {
        crate::sqlgen::ident(driver, name)
    }
}

/// Prefix matches first, then substring matches; duplicates removed, capped.
fn rank(items: Vec<Item>, prefix: &str) -> Vec<Item> {
    let p = prefix.to_lowercase();
    let mut scored: Vec<(u8, usize, Item)> = items
        .into_iter()
        .enumerate()
        .filter_map(|(i, item)| {
            let l = item.label.to_lowercase();
            let score = if l.starts_with(&p) {
                0
            } else if p.len() >= 2 && l.contains(&p) {
                1
            } else {
                return None;
            };
            // An exactly typed keyword needs no popup entry.
            (!(item.kind == ItemKind::Keyword && l == p)).then_some((score, i, item))
        })
        .collect();
    scored.sort_by_key(|(score, i, _)| (*score, *i));
    let mut seen = std::collections::HashSet::new();
    scored
        .into_iter()
        .map(|(_, _, item)| item)
        .filter(|item| seen.insert((item.label.clone(), item.detail.clone())))
        .take(50)
        .collect()
}

#[derive(Debug, PartialEq)]
struct TableRef {
    schema: Option<String>,
    table: String,
    alias: Option<String>,
}

/// Table references with aliases from FROM/JOIN/UPDATE/INTO clauses.
fn table_refs(sql: &str) -> Vec<TableRef> {
    let tokens = tokenize(sql);
    let mut refs = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let upper = tokens[i].to_uppercase();
        if !TABLE_CONTEXT.contains(&upper.as_str()) {
            i += 1;
            continue;
        }
        i += 1;
        // FROM a x, b AS y
        while let Some(name) = tokens.get(i).filter(|t| {
            t.chars()
                .next()
                .is_some_and(|c| c.is_alphabetic() || c == '_' || c == '"' || c == '`')
        }) {
            let (schema, table) = match split_qualified(name) {
                Some((s, t)) => (Some(s.to_owned()), t.to_owned()),
                None => (None, name.clone()),
            };
            i += 1;
            if tokens.get(i).is_some_and(|t| t.eq_ignore_ascii_case("AS")) {
                i += 1;
            }
            let alias = tokens
                .get(i)
                .filter(|t| {
                    (t.chars().all(is_ident_char) || t.starts_with(['"', '`']))
                        && !NOT_ALIAS.contains(&t.to_uppercase().as_str())
                })
                .cloned();
            if alias.is_some() {
                i += 1;
            }
            refs.push(TableRef {
                schema,
                table,
                alias,
            });
            if tokens.get(i).map(String::as_str) == Some(",") {
                i += 1;
            } else {
                break;
            }
        }
    }
    refs
}

/// Words (with dotted and quoted parts glued together) and single punctuation.
fn tokenize(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in sql.chars() {
        if let Some(q) = quote {
            cur.push(c);
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '`' => {
                quote = Some(c);
                cur.push(c);
            }
            // A string literal is one token, so its words are not taken for names.
            '\'' => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                quote = Some(c);
                cur.push(c);
            }
            c if is_ident_char(c) || c == '.' => cur.push(c),
            c => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                if !c.is_whitespace() {
                    out.push(c.to_string());
                }
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> Vec<TableMeta> {
        let t = |schema: &str, name: &str, cols: &[&str]| TableMeta {
            schema: schema.into(),
            name: name.into(),
            is_view: false,
            columns: cols
                .iter()
                .map(|c| (c.to_string(), "int".to_string()))
                .collect(),
        };
        vec![
            t("shop", "customers", &["id", "email", "name"]),
            t("shop", "orders", &["id", "customer_id", "total"]),
            t("public", "Users", &["id", "login"]),
        ]
    }

    fn labels(sql: &str) -> Vec<String> {
        let cursor = sql.find('|').unwrap();
        let script = sql.replace('|', "");
        complete(&script, cursor, DriverKind::Postgres, &meta())
            .items
            .into_iter()
            .map(|i| i.label)
            .collect()
    }

    #[test]
    fn tables_after_from() {
        assert_eq!(labels("select * from ord|"), ["orders"]);
        assert!(labels("select * from |").contains(&"customers".to_string()));
    }

    #[test]
    fn columns_through_alias() {
        assert_eq!(
            labels("select o.| from shop.orders o"),
            ["id", "customer_id", "total"]
        );
        assert_eq!(
            labels(
                "select c.em| from shop.customers as c join shop.orders o on o.customer_id = c.id"
            ),
            ["email"]
        );
    }

    #[test]
    fn columns_of_tables_in_scope_and_keywords() {
        let out = labels("select cust| from orders");
        assert_eq!(out[0], "customer_id");
        let out = labels("sel|");
        assert_eq!(out[0], "SELECT");
    }

    #[test]
    fn tables_in_schema() {
        assert_eq!(labels("select * from shop.c|"), ["customers"]);
    }

    #[test]
    fn replace_range_and_quoting() {
        let c = complete("select * from Use", 17, DriverKind::Postgres, &meta());
        assert_eq!(c.replace, 14..17);
        assert_eq!(c.items[0].insert, "\"Users\"");
    }

    #[test]
    fn filter_field_context() {
        // Table editor fields are completed after a synthetic SELECT ... WHERE prefix.
        assert_eq!(
            labels(r#"SELECT * FROM "shop"."customers" WHERE em|"#),
            ["email"]
        );
        assert_eq!(
            labels(r#"SELECT * FROM "shop"."orders" ORDER BY tot|"#),
            ["total"]
        );
    }

    #[test]
    fn order_by_after_a_column_offers_direction() {
        assert_eq!(
            labels("select * from shop.orders order by total d|"),
            ["DESC"]
        );
        assert!(labels("select * from shop.orders order by total s|").is_empty());
        assert_eq!(
            labels("select * from shop.orders order by total desc n|"),
            ["NULLS FIRST", "NULLS LAST"]
        );
        let after_by = labels("select * from shop.orders order by |");
        assert_eq!(after_by, ["id", "customer_id", "total"]);
        assert_eq!(
            labels("select * from shop.orders order by total, c|"),
            ["customer_id"]
        );
    }

    #[test]
    fn where_clause_positions() {
        let ops = labels("select * from shop.orders where total |");
        assert!(
            ops.contains(&"IS NULL".to_string()) && ops.contains(&"IN".to_string()),
            "{ops:?}"
        );
        assert!(!ops.contains(&"total".to_string()));
        assert_eq!(
            labels("select * from shop.orders where total > 5 a|"),
            ["AND"]
        );
        assert_eq!(
            labels("select * from shop.orders where id = 'paid' o|"),
            ["OR", "ORDER BY"]
        );
        // Prefix matches first, then names containing the text.
        assert_eq!(
            labels("select * from shop.orders where to|"),
            ["total", "customer_id"]
        );
        assert_eq!(
            labels("select * from shop.orders where total is n|"),
            ["NULL", "NOT NULL"]
        );
    }

    #[test]
    fn select_list_and_group_by() {
        assert_eq!(labels("select id f| from shop.orders"), ["FROM"]);
        assert_eq!(labels("select id, cu| from shop.orders"), ["customer_id"]);
        assert_eq!(labels("select count(cu| from shop.orders"), ["customer_id"]);
        assert_eq!(
            labels("select id from shop.orders group by id h|"),
            ["HAVING"]
        );
    }

    #[test]
    fn aliases_in_comma_list() {
        let refs = table_refs("select * from shop.orders o, customers AS c where o.id = 1");
        assert_eq!(
            refs[0],
            TableRef {
                schema: Some("shop".into()),
                table: "orders".into(),
                alias: Some("o".into())
            }
        );
        assert_eq!(refs[1].alias.as_deref(), Some("c"));
    }
}

#[cfg(test)]
mod context_tests {
    use super::*;

    fn table(schema: &str, name: &str, column: &str) -> TableMeta {
        TableMeta {
            schema: schema.into(),
            name: name.into(),
            is_view: false,
            columns: vec![(column.into(), "int".into())],
        }
    }

    fn scoped(sql: &str, meta: &[TableMeta], path: &[&str]) -> Vec<Item> {
        let cursor = sql.find('|').unwrap();
        complete_in_context(
            &sql.replace('|', ""),
            cursor,
            DriverKind::Postgres,
            meta,
            &path.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        )
        .items
    }

    #[test]
    fn ordered_path_resolves_columns_and_qualifies_shadowed_tables() {
        let meta = vec![
            table("public", "items", "public_col"),
            table("sales", "items", "sales_col"),
        ];
        let items = scoped("select i.| from items i", &meta, &["sales", "public"]);
        assert_eq!(
            items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(),
            ["sales_col"]
        );
        let candidates = scoped("select * from ite|", &meta, &["sales", "public"]);
        assert_eq!(candidates[0].insert, "public.items");
        assert_eq!(candidates[1].insert, "items");
        let explicit = scoped("select i.| from public.items i", &meta, &["sales"]);
        assert_eq!(explicit[0].label, "public_col");
        let qualified = scoped("select * from public.it|", &meta, &["sales"]);
        assert_eq!(qualified[0].insert, "items");
    }

    #[test]
    fn unknown_path_never_infers_a_public_table() {
        let meta = vec![table("public", "items", "secret_col")];
        assert!(scoped("select i.| from items i", &meta, &[]).is_empty());
        assert_eq!(
            scoped("select * from it|", &meta, &[])[0].insert,
            "public.items"
        );
    }

    #[test]
    fn quoted_case_dots_spaces_and_escaped_quotes_resolve_exactly() {
        let meta = vec![
            table("Sales.EU", "Order.Items", "right_col"),
            table("public", "Items", "mixed_col"),
            table("public", "items", "lower_col"),
            table("A\" B", "Żółw", "unicode_col"),
        ];
        assert_eq!(
            scoped(
                "select o.| from \"Sales.EU\".\"Order.Items\" o",
                &meta,
                &["public"]
            )[0]
            .label,
            "right_col"
        );
        assert_eq!(
            scoped("select i.| from \"Items\" i", &meta, &["public"])[0].label,
            "mixed_col"
        );
        assert_eq!(
            scoped("select i.| from ITEMS i", &meta, &["public"])[0].label,
            "lower_col"
        );
        assert_eq!(
            scoped("select * from \"Sales.EU\".Ord|", &meta, &["public"])[0].insert,
            "\"Order.Items\""
        );
        assert_eq!(
            scoped(
                "select \"my alias\".| from \"A\"\" B\".\"Żółw\" \"my alias\"",
                &meta,
                &["public"]
            )[0]
            .label,
            "unicode_col"
        );
    }
    #[test]
    fn postgres_aliases_fold_both_references_but_preserve_quoted_case() {
        let meta = vec![table("public", "items", "item_id")];
        for sql in [
            "select x.| from items X",
            "select X.| from items X",
            "select x.| from items AS x",
            "select \"X\".| from items AS \"X\"",
            "select public.items.| from public.items",
        ] {
            assert_eq!(scoped(sql, &meta, &["public"])[0].label, "item_id", "{sql}");
        }
        assert!(scoped("select x.| from items AS \"X\"", &meta, &["public"]).is_empty());
    }
    #[test]
    fn reserved_schema_names_are_quoted_when_qualifying_candidates() {
        let meta = vec![table("order", "items", "item_id")];
        assert_eq!(
            scoped("select * from ite|", &meta, &["public"])[0].insert,
            "\"order\".items"
        );
    }
}
