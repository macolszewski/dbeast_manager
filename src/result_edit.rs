//! Conservative eligibility and physical column mapping for grid edits, both
//! in console results and in table tabs.
//! Run once per result, never while drawing cells. SQL only limits eligibility;
//! the protocol and the server catalog determine the actual target.

use crate::model::{
    DriverKind, OriginColumn, OriginTable, QueryResult, ResultEditMetadata, TableEditMetadata,
    Value,
};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

/// `sql` is the console statement whose grammar limits eligibility. Table tabs
/// pass `None`: DBeast generated their SELECT, and a user filter (even with a
/// subquery) cannot change which table the rows come from.
pub fn candidate<'a>(
    sql: Option<&str>,
    driver: DriverKind,
    data: &'a QueryResult,
) -> Result<&'a OriginTable, String> {
    if let Some(sql) = sql {
        simple_select(sql, driver)?;
    }
    let source = data.source.as_ref().ok_or(
        "Column origins are unavailable. Execute the query again to check whether it can be edited.",
    )?;
    if source.columns.len() != data.columns.len()
        || source.exact_columns.len() != data.columns.len()
    {
        return Err("Incomplete column origin metadata.".into());
    }
    let mut origins = source.columns.iter().flatten();
    let table = &origins
        .next()
        .ok_or("No physical table columns in this result.")?
        .table;
    if origins.any(|origin| &origin.table != table) {
        return Err("Columns from multiple tables cannot be edited in this result.".into());
    }
    Ok(table)
}

pub fn resolve(
    sql: Option<&str>,
    driver: DriverKind,
    data: &QueryResult,
    table: &TableEditMetadata,
) -> Result<ResultEditMetadata, String> {
    let origin = candidate(sql, driver, data)?;
    let same_table = match origin {
        OriginTable::Postgres(oid) => table.table.oid == Some(*oid),
        OriginTable::Named {
            schema,
            table: name,
        } => table.table.schema == *schema && table.table.name == *name,
    };
    if !same_table {
        return Err("Table metadata belongs to another result.".into());
    }
    if !table.transactional {
        return Err("This table cannot roll back a failed batch of edits.".into());
    }
    if table.primary_key.is_empty() {
        return Err(
            "The table has no primary key, so rows cannot be identified safely. Add a primary key to edit it.".into(),
        );
    }
    let source = data.source.as_ref().unwrap();
    let by_name: HashMap<_, _> = table.columns.iter().map(|c| (c.name.as_str(), c)).collect();
    let by_ordinal: HashMap<_, _> = table.columns.iter().map(|c| (c.ordinal, c)).collect();
    let mut positions = HashMap::new();
    let mut physical_columns = Vec::with_capacity(data.columns.len());
    let mut writable = Vec::with_capacity(data.columns.len());
    let mut comparable = Vec::with_capacity(data.columns.len());
    let mut update_default = Vec::with_capacity(data.columns.len());
    let mut column_specs = Vec::with_capacity(data.columns.len());
    let mut auto_increment = Vec::with_capacity(data.columns.len());
    for (index, origin) in source.columns.iter().enumerate() {
        let column = match origin.as_ref().map(|c| &c.column) {
            Some(OriginColumn::Postgres(ordinal)) => u32::try_from(*ordinal)
                .ok()
                .and_then(|n| by_ordinal.get(&n).copied()),
            Some(OriginColumn::Named(name)) => by_name.get(name.as_str()).copied(),
            None => None,
        };
        if origin.is_some() && column.is_none() {
            return Err(
                "A result column no longer exists in the table. Execute the query again.".into(),
            );
        }
        if let Some(column) = column
            && positions.insert(column.name.as_str(), index).is_some()
        {
            return Err("A physical column appears more than once in this result.".into());
        }
        let type_name = column.map(|c| c.type_name.as_str()).filter(|s| !s.is_empty()).unwrap_or(&data.columns[index].type_name);
        let mut spec = crate::cell_value::ColumnSpec::new(driver, type_name, column.is_some_and(|c| c.nullable));
        if let Some(column) = column && !column.enum_values.is_empty() {
            spec.kind = crate::cell_value::EditorKind::Enum(column.enum_values.clone());
        }
        column_specs.push(spec);
        let can_write = column.is_some_and(|c| !c.generated && source.exact_columns[index]);
        physical_columns.push(column.map(|c| c.name.clone()));
        writable.push(can_write);
        auto_increment.push(column.is_some_and(|c| c.auto_increment) && source.exact_columns[index]);
        comparable.push(column.is_some_and(|c| c.comparable) && source.exact_columns[index]);
        update_default.push(can_write && column.is_some_and(|c| c.update_default));
    }
    let mut primary_key = Vec::with_capacity(table.primary_key.len());
    for name in &table.primary_key {
        let index = *positions.get(name.as_str()).ok_or(
            "The result does not contain every primary key column. Include the full key in SELECT.",
        )?;
        let column = by_name[name.as_str()];
        if column.nullable || !column.comparable || !source.exact_columns[index] {
            return Err("The primary key cannot be represented and compared safely.".into());
        }
        if primary_key.contains(&index) {
            return Err("Duplicate column in primary key metadata.".into());
        }
        primary_key.push(index);
    }
    if !writable.iter().any(|value| *value) {
        return Err("This result contains no writable physical columns.".into());
    }
    validate_keys(data, &primary_key)?;
    Ok(ResultEditMetadata {
        column_specs,
        table: table.table.clone(),
        physical_columns,
        writable,
        comparable,
        update_default,
        auto_increment,
        primary_key,
        session_epoch: source.session_epoch,
    })
}

/// Borrow keys instead of allocating or serializing each fetched row.
fn validate_keys(data: &QueryResult, indices: &[usize]) -> Result<(), String> {
    let mut keys = HashSet::with_capacity(data.rows.len());
    for row in &data.rows {
        if row.len() != data.columns.len() {
            return Err("A result row has incomplete data.".into());
        }
        if indices.iter().any(|&i| match &row[i] {
            Value::Null => true,
            Value::Float(f) => !f.is_finite(),
            _ => false,
        }) {
            return Err("A row has a NULL or non-finite primary key.".into());
        }
        if !keys.insert(RowKey { row, indices }) {
            return Err("The result contains duplicate primary keys.".into());
        }
    }
    Ok(())
}

struct RowKey<'a> {
    row: &'a [Value],
    indices: &'a [usize],
}
impl PartialEq for RowKey<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.indices
            .iter()
            .zip(other.indices)
            .all(|(&a, &b)| self.row[a] == other.row[b])
    }
}
impl Eq for RowKey<'_> {}
impl Hash for RowKey<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        for &i in self.indices {
            let value = &self.row[i];
            std::mem::discriminant(value).hash(state);
            match value {
                Value::Null => {}
                Value::Bool(v) => v.hash(state),
                Value::Int(v) => v.hash(state),
                Value::Float(v) => (if *v == 0.0 { 0 } else { v.to_bits() }).hash(state),
                Value::Text(v) | Value::Decimal(v) => v.hash(state),
                Value::Bytes(v) => v.hash(state),
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Token<'a> {
    Word(&'a str),
    Quoted,
    Literal,
    Symbol(u8),
}
impl Token<'_> {
    fn is(self, word: &str) -> bool {
        matches!(self, Self::Word(s) if s.eq_ignore_ascii_case(word))
    }
    fn identifier(self) -> bool {
        matches!(self, Self::Word(_) | Self::Quoted)
    }
}

/// Replaying a write result (including a data-modifying CTE) is an execution,
/// not pagination. Unknown client syntax stays an explicit console operation.
pub fn can_replay(sql: &str, driver: DriverKind) -> bool {
    let Some(tokens) = tokenize(sql, driver) else {
        return false;
    };
    if !tokens.first().is_some_and(|token| {
        [
            "SELECT", "WITH", "VALUES", "TABLE", "SHOW", "EXPLAIN", "DESCRIBE", "DESC",
        ]
        .iter()
        .any(|word| token.is(word))
    }) {
        return false;
    }
    !tokens.iter().enumerate().any(|(index, token)| {
        [
            "INSERT", "UPDATE", "DELETE", "MERGE", "REPLACE", "CREATE", "ALTER", "DROP",
            "TRUNCATE", "GRANT", "REVOKE", "CALL", "DO", "COPY", "ANALYZE", "INTO",
        ]
        .iter()
        .any(|word| token.is(word))
            || (matches!(token, Token::Symbol(b';')) && index + 1 != tokens.len())
    })
}

/// Deliberately accepts a small grammar. Unknown syntax is read-only, not a
/// reason to guess a table or replay DML with RETURNING after a successful save.
pub fn simple_select(sql: &str, driver: DriverKind) -> Result<(), String> {
    const REASON: &str = "Only a simple SELECT from one table can be edited. Joins, subqueries, aggregates and compound queries are read-only.";
    let tokens = tokenize(sql, driver).ok_or(REASON)?;
    if !tokens.first().is_some_and(|t| t.is("SELECT")) {
        return Err(REASON.into());
    }
    let mut depth = 0usize;
    let mut from = None;
    for (index, &token) in tokens.iter().enumerate().skip(1) {
        if [
            "SELECT",
            "WITH",
            "JOIN",
            "UNION",
            "INTERSECT",
            "EXCEPT",
            "DISTINCT",
            "GROUP",
            "HAVING",
            "WINDOW",
            "OVER",
            "INTO",
            "RETURNING",
            "TABLESAMPLE",
            "PIVOT",
            "UNPIVOT",
        ]
        .iter()
        .any(|w| token.is(w))
        {
            return Err(REASON.into());
        }
        match token {
            Token::Symbol(b'(') => {
                // Function calls in the projection may be user-defined aggregates.
                if from.is_none() && tokens[index - 1].identifier() {
                    return Err(REASON.into());
                }
                depth += 1;
            }
            Token::Symbol(b')') => {
                depth = depth.checked_sub(1).ok_or(REASON)?;
            }
            _ if token.is("FROM") => {
                if depth != 0 || from.replace(index).is_some() {
                    return Err(REASON.into());
                }
            }
            Token::Symbol(b';') if index + 1 != tokens.len() => return Err(REASON.into()),
            _ => {}
        }
    }
    if depth != 0 {
        return Err(REASON.into());
    }
    let from = from.ok_or(REASON)?;
    if from <= 1 {
        return Err(REASON.into());
    }
    let mut i = from + 1;
    if !tokens.get(i).is_some_and(|t| t.identifier()) {
        return Err(REASON.into());
    }
    i += 1;
    // One optional schema/database qualifier. More parts require another grammar.
    if matches!(tokens.get(i), Some(Token::Symbol(b'.'))) {
        i += 1;
        if !tokens.get(i).is_some_and(|t| t.identifier()) {
            return Err(REASON.into());
        }
        i += 1;
    }
    let clause = |t: Token<'_>| {
        ["WHERE", "ORDER", "LIMIT", "OFFSET", "FETCH", "FOR"]
            .iter()
            .any(|w| t.is(w))
    };
    if tokens.get(i).is_some_and(|t| t.is("AS")) {
        i += 1;
        if !tokens.get(i).is_some_and(|t| t.identifier()) {
            return Err(REASON.into());
        }
        i += 1;
    } else if tokens.get(i).is_some_and(|t| t.identifier() && !clause(*t)) {
        i += 1;
    }
    if let Some(&token) = tokens.get(i)
        && !clause(token)
        && !matches!(token, Token::Symbol(b';'))
    {
        return Err(REASON.into());
    }
    Ok(())
}

fn tokenize(sql: &str, driver: DriverKind) -> Option<Vec<Token<'_>>> {
    let b = sql.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        match b[i] {
            c if c.is_ascii_whitespace() => i += 1,
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'#' if driver == DriverKind::MySql => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                // MySQL executable comments must never disappear from classification.
                if matches!(b.get(i + 2), Some(b'!' | b'+')) {
                    return None;
                }
                i += 2;
                let mut depth = 1;
                while i < b.len() && depth > 0 {
                    if b[i..].starts_with(b"/*") && driver == DriverKind::Postgres {
                        depth += 1;
                        i += 2;
                    } else if b[i..].starts_with(b"*/") {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                if depth != 0 {
                    return None;
                }
            }
            q @ (b'\'' | b'"' | b'`' | b'[') => {
                if q == b'`' && driver == DriverKind::Postgres {
                    return None;
                }
                if q == b'[' && driver != DriverKind::Sqlite {
                    return None;
                }
                let end = if q == b'[' { b']' } else { q };
                i += 1;
                let mut closed = false;
                while i < b.len() {
                    // String escape mode is session-dependent. Refuse ambiguity.
                    if b[i] == b'\\' && q == b'\'' {
                        return None;
                    }
                    if b[i] == end {
                        i += 1;
                        if b.get(i) == Some(&end) && q != b'[' {
                            i += 1;
                        } else {
                            closed = true;
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                if !closed {
                    return None;
                }
                out.push(if q == b'\'' {
                    Token::Literal
                } else {
                    Token::Quoted
                });
            }
            b'$' if driver == DriverKind::Postgres => {
                let start = i;
                i += 1;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                if b.get(i) != Some(&b'$') {
                    return None;
                }
                i += 1;
                let tag = &sql[start..i];
                i += sql[i..].find(tag)? + tag.len();
                out.push(Token::Literal);
            }
            c if c.is_ascii_alphabetic() || c == b'_' || c >= 128 => {
                let start = i;
                i += 1;
                while i < b.len()
                    && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] >= 128)
                {
                    i += 1;
                }
                out.push(Token::Word(&sql[start..i]));
            }
            c => {
                out.push(Token::Symbol(c));
                i += 1;
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;

    fn fixture() -> (QueryResult, TableEditMetadata) {
        let table = TableEditMetadata {
            table: PhysicalTable {
                schema: "main".into(),
                name: "orders".into(),
                oid: None,
            },
            columns: ["tenant", "id", "note", "generated"]
                .iter()
                .enumerate()
                .map(|(i, name)| PhysicalColumn {
                    enum_values: Vec::new(),
                    name: (*name).into(),
                    type_name: String::new(),
                    ordinal: i as u32 + 1,
                    nullable: false,
                    generated: i == 3,
                    auto_increment: i == 1,
                    update_default: false,
                    comparable: true,
                })
                .collect(),
            primary_key: vec!["tenant".into(), "id".into()],
            transactional: true,
        };
        let data = QueryResult {
            columns: [
                "alias_id",
                "same label",
                "same label",
                "derived",
                "calculated",
            ]
            .iter()
            .map(|s| Column {
                name: (*s).into(),
                type_name: "".into(),
            })
            .collect(),
            rows: vec![vec![
                Value::Int(7),
                Value::Int(2),
                Value::Text("original".into()),
                Value::Int(14),
                Value::Int(8),
            ]],
            source: Some(ResultSource {
                session_epoch: 4,
                columns: [
                    Some("id"),
                    Some("tenant"),
                    Some("note"),
                    Some("generated"),
                    None,
                ]
                .iter()
                .map(|name| {
                    name.map(|name| ColumnOrigin {
                        table: OriginTable::Named {
                            schema: "main".into(),
                            table: "orders".into(),
                        },
                        column: OriginColumn::Named(name.into()),
                    })
                })
                .collect(),
                exact_columns: vec![true; 5],
            }),
            ..Default::default()
        };
        (data, table)
    }

    #[test]
    fn cell_editor_maps_automatic_keys_by_origin() {
        let (data, table) = fixture();
        let metadata = resolve(None, DriverKind::Sqlite, &data, &table).unwrap();
        assert_eq!(metadata.auto_increment, [true, false, false, false, false]);
    }

    #[test]
    fn classifier_obeys_literals_comments_and_single_table_grammar() {
        for sql in [
            "SELECT id, note FROM main.orders o WHERE id > 1 ORDER BY id",
            "/* SELECT join */ select id, 'UNION' AS note, (id + 1) from \"join\"; -- end",
            "select id, $$join; select$$ from t",
        ] {
            assert!(simple_select(sql, DriverKind::Postgres).is_ok(), "{sql}");
        }
        for sql in [
            "DELETE FROM t RETURNING *",
            "WITH t AS (SELECT 1) SELECT * FROM t",
            "SELECT a.id FROM a JOIN b ON a.id=b.id",
            "SELECT id FROM a,b",
            "SELECT id FROM t UNION SELECT id FROM t",
            "SELECT id,(SELECT id FROM b) FROM a",
            "SELECT DISTINCT id FROM t",
            "SELECT id FROM t GROUP BY id",
            "SELECT id,count(*) FROM t",
            "SELECT id,my_aggregate(note) FROM t",
            "SELECT id FROM generate_series(1,10)",
            "SELECT * INTO copy FROM t",
            "SELECT id FROM t; DELETE FROM t",
            "SELECT * FROM (t)",
            "SELECT id FROM t /* unterminated",
        ] {
            assert!(simple_select(sql, DriverKind::Postgres).is_err(), "{sql}");
        }
        assert!(
            simple_select(
                "SELECT id FROM t /*! UNION SELECT id FROM u */",
                DriverKind::MySql
            )
            .is_err()
        );
        assert!(simple_select("SELECT [join] FROM [a]", DriverKind::Sqlite).is_ok());
        assert!(can_replay(
            "/* comment */ WITH t AS (SELECT 'delete' AS name) SELECT * FROM t",
            DriverKind::Postgres
        ));
        for sql in [
            "DELETE FROM t RETURNING *",
            "WITH t AS (DELETE FROM a RETURNING *) SELECT * FROM t",
            "SELECT * FROM t; DELETE FROM t",
            "EXPLAIN ANALYZE DELETE FROM t",
            "PRAGMA journal_mode=WAL",
        ] {
            assert!(!can_replay(sql, DriverKind::Postgres), "{sql}");
        }
    }

    #[test]
    fn physical_names_and_key_order_ignore_display_aliases() {
        let (data, table) = fixture();
        let mapped = resolve(
            Some("SELECT id,tenant,note,generated,id+1 FROM orders"),
            DriverKind::Sqlite,
            &data,
            &table,
        )
        .unwrap();
        assert_eq!(mapped.primary_key, [1, 0]);
        assert_eq!(mapped.physical_columns[0].as_deref(), Some("id"));
        assert_eq!(mapped.writable, [true, true, true, false, false]);
        assert_eq!(mapped.session_epoch, 4);
    }

    #[test]
    fn incomplete_lossy_nullable_duplicate_and_wrong_table_are_read_only() {
        let (data, table) = fixture();
        let check = |d: &QueryResult, t: &TableEditMetadata| {
            resolve(Some("SELECT * FROM orders"), DriverKind::Sqlite, d, t)
        };
        let mut d = data.clone();
        d.source.as_mut().unwrap().columns[1] = None;
        assert!(check(&d, &table).is_err());
        let mut d = data.clone();
        d.source.as_mut().unwrap().exact_columns[0] = false;
        assert!(check(&d, &table).is_err());
        let mut t = table.clone();
        t.columns[0].nullable = true;
        assert!(check(&data, &t).is_err());
        let mut d = data.clone();
        d.rows[0][0] = Value::Null;
        assert!(check(&d, &table).is_err());
        let mut d = data.clone();
        d.rows.push(d.rows[0].clone());
        assert!(check(&d, &table).is_err());
        let mut d = data.clone();
        d.source.as_mut().unwrap().columns[4] = d.source.as_ref().unwrap().columns[0].clone();
        assert!(check(&d, &table).is_err());
        let mut t = table.clone();
        t.table.name = "other".into();
        assert!(check(&data, &t).is_err());
        let mut t = table.clone();
        t.primary_key.clear();
        assert!(check(&data, &t).is_err());
        let mut t = table.clone();
        t.transactional = false;
        assert!(check(&data, &t).is_err());
        let restored: QueryResult =
            serde_json::from_str(&serde_json::to_string(&data).unwrap()).unwrap();
        assert!(check(&restored, &table).is_err());
    }

    #[test]
    #[ignore = "release microbenchmark of one-time result validation"]
    fn benchmark_result_edit_resolution() {
        use std::hint::black_box;
        use std::time::Instant;
        let (mut data, table) = fixture();
        let row = data.rows[0].clone();
        for count in [500, 5_000, 100_000] {
            data.rows = (0..count)
                .map(|id| {
                    let mut row = row.clone();
                    row[0] = Value::Int(id);
                    row
                })
                .collect();
            let sql = "SELECT id,tenant,note,generated,id+1 FROM orders";
            for _ in 0..3 {
                black_box(resolve(Some(sql), DriverKind::Sqlite, &data, &table).unwrap());
            }
            let mut samples = Vec::new();
            for _ in 0..9 {
                let start = Instant::now();
                black_box(resolve(Some(sql), DriverKind::Sqlite, &data, &table).unwrap());
                samples.push(start.elapsed().as_secs_f64() * 1_000.0);
            }
            samples.sort_by(f64::total_cmp);
            eprintln!(
                "result edit mapping + full PK validation, {count} rows × 5 columns: {:.3} ms median",
                samples[4]
            );
        }
    }
}
