//! SQL text generation: literals, identifiers and the parameterized DML that
//! applies grid edits. Every edit path (table tabs and console results) uses
//! [`bound_mutations`]; values are never interpolated into DML.

use crate::model::{BoundUpdate, QueryResult, ResultEditMetadata};
use crate::model::{DriverKind, Value};
use std::collections::{BTreeMap, HashMap, HashSet};

/// Build navigation solely from catalog identifiers and typed key parameters.
pub fn related_query(
    driver: DriverKind,
    query: &crate::model::CapturedRelatedQuery,
) -> Result<crate::model::BoundQuery, String> {
    use crate::model::RelatedDirection;
    crate::related::validate_query(driver, query)?;
    let relation = &query.relation;
    let parent = qualified(driver, Some(&relation.target.schema), &relation.target.name);
    let child = qualified(driver, Some(&relation.source.schema), &relation.source.name);
    let mut params = Vec::with_capacity(query.values.len());
    let key = relation
        .columns
        .iter()
        .zip(&query.values)
        .map(|(pair, value)| {
            format!(
                "p.{} = {}",
                ident(driver, &pair.target_name),
                bind(driver, &mut params, value.clone())
            )
        })
        .collect::<Vec<_>>()
        .join(" AND ");
    let sql = match query.direction {
        RelatedDirection::Referenced => format!("SELECT p.* FROM {parent} AS p WHERE {key}"),
        RelatedDirection::Referencing => {
            let child_affinity = if driver == DriverKind::Sqlite {
                "+"
            } else {
                ""
            };
            let join = relation
                .columns
                .iter()
                .map(|pair| {
                    format!(
                        "p.{} = {child_affinity}c.{}",
                        ident(driver, &pair.target_name),
                        ident(driver, &pair.source_name)
                    )
                })
                .collect::<Vec<_>>()
                .join(" AND ");
            format!(
                "SELECT c.* FROM {child} AS c WHERE EXISTS (SELECT 1 FROM {parent} AS p WHERE {key} AND {join})"
            )
        }
    };
    Ok(crate::model::BoundQuery { sql, params })
}

/// Compile one parameterized UPDATE per changed row. Output aliases never
/// become SQL identifiers, and WHERE always uses the original primary key.
pub fn bound_updates(
    driver: DriverKind,
    metadata: &ResultEditMetadata,
    data: &QueryResult,
    changes: &HashMap<(usize, usize), CellEdit>,
) -> Result<Vec<BoundUpdate>, String> {
    let columns = data.columns.len();
    if metadata.primary_key.is_empty()
        || metadata.physical_columns.len() != columns
        || metadata.writable.len() != columns
        || metadata.comparable.len() != columns
        || metadata.update_default.len() != columns
        || data
            .source
            .as_ref()
            .is_none_or(|s| s.session_epoch != metadata.session_epoch)
    {
        return Err(
            "The result no longer has valid edit metadata. Execute the query again.".into(),
        );
    }
    let mut keys = HashSet::new();
    for &key in &metadata.primary_key {
        if key >= columns || metadata.physical_columns[key].is_none() || !keys.insert(key) {
            return Err("Invalid primary key mapping.".into());
        }
    }
    let mut by_row: BTreeMap<usize, Vec<(usize, &CellEdit)>> = BTreeMap::new();
    for (&(row, col), edit) in changes {
        if col >= columns || !metadata.writable[col] || metadata.physical_columns[col].is_none() {
            return Err("A changed cell is read-only.".into());
        }
        if matches!(edit, CellEdit::Default) && !metadata.update_default[col] {
            return Err("Set DEFAULT is unavailable for this column.".into());
        }
        by_row.entry(row).or_default().push((col, edit));
    }
    let mut out = Vec::with_capacity(by_row.len());
    let table = qualified(driver, Some(&metadata.table.schema), &metadata.table.name);
    let equality = match driver {
        DriverKind::Postgres => "IS NOT DISTINCT FROM",
        DriverKind::MySql => "<=>",
        DriverKind::Sqlite => "IS",
    };
    for (row, mut edits) in by_row {
        let original = data
            .rows
            .get(row)
            .ok_or("The changed row is no longer available.")?;
        if original.len() != columns
            || metadata
                .primary_key
                .iter()
                .any(|&c| original[c] == Value::Null)
        {
            return Err("The changed row has an incomplete primary key.".into());
        }
        edits.sort_unstable_by_key(|(col, _)| *col);
        let mut params = Vec::with_capacity(edits.len() * 2 + keys.len());
        let mut assignments = Vec::with_capacity(edits.len());
        for &(col, edit) in &edits {
            if matches!(original[col], Value::Bytes(_)) && matches!(edit, CellEdit::Text(_)) {
                return Err("Binary values cannot be changed with the text editor.".into());
            }
            let value = match edit {
                CellEdit::Default => "DEFAULT".into(),
                CellEdit::Null => bind(driver, &mut params, Value::Null),
                CellEdit::Text(text) => bind(driver, &mut params, Value::Text(text.clone())),
            };
            assignments.push(format!(
                "{} = {value}",
                ident(driver, metadata.physical_columns[col].as_ref().unwrap())
            ));
        }
        let mut predicates = Vec::with_capacity(keys.len() + edits.len());
        // Old values of comparable changed columns detect a concurrent change.
        for &col in metadata.primary_key.iter().chain(
            edits
                .iter()
                .filter_map(|(c, _)| (!keys.contains(c) && metadata.comparable[*c]).then_some(c)),
        ) {
            let placeholder = bind(driver, &mut params, original[col].clone());
            predicates.push(format!(
                "{} {equality} {placeholder}",
                ident(driver, metadata.physical_columns[col].as_ref().unwrap())
            ));
        }
        out.push(BoundUpdate {
            sql: format!(
                "UPDATE {table} SET {} WHERE {}",
                assignments.join(", "),
                predicates.join(" AND ")
            ),
            params,
            source_row: row,
        });
    }
    Ok(out)
}

/// Pending grid changes of one result.
pub struct PendingEdits<'a> {
    pub changed: &'a HashMap<(usize, usize), CellEdit>,
    pub deleted: &'a HashSet<usize>,
    pub inserted: &'a [Vec<CellEdit>],
}

/// DELETE, UPDATE and INSERT statements for one result, in that order. Each
/// must change exactly one row when executed. DELETE identifies the row by its
/// original primary key and also requires every other exactly represented
/// column to still hold the value that was shown, so a concurrent change is
/// reported as a conflict instead of deleting a row the user did not see.
pub fn bound_mutations(
    driver: DriverKind,
    metadata: &ResultEditMetadata,
    data: &QueryResult,
    edits: &PendingEdits<'_>,
) -> Result<Vec<BoundUpdate>, String> {
    let changed: HashMap<(usize, usize), CellEdit> = edits
        .changed
        .iter()
        .filter(|((row, _), _)| !edits.deleted.contains(row))
        .map(|(key, edit)| (*key, edit.clone()))
        .collect();
    // Validates metadata and the session even when only rows are deleted or added.
    let updates = bound_updates(driver, metadata, data, &changed)?;
    let columns = data.columns.len();
    let table = qualified(driver, Some(&metadata.table.schema), &metadata.table.name);
    let equality = match driver {
        DriverKind::Postgres => "IS NOT DISTINCT FROM",
        DriverKind::MySql => "<=>",
        DriverKind::Sqlite => "IS",
    };
    let exact = data
        .source
        .as_ref()
        .map(|source| source.exact_columns.clone())
        .unwrap_or_default();
    let mut out = Vec::with_capacity(edits.deleted.len() + updates.len() + edits.inserted.len());
    let mut deleted: Vec<usize> = edits.deleted.iter().copied().collect();
    deleted.sort_unstable();
    for row in deleted {
        let original = data
            .rows
            .get(row)
            .filter(|original| original.len() == columns)
            .ok_or("A row marked for deletion is no longer available.")?;
        if metadata
            .primary_key
            .iter()
            .any(|&c| original[c] == Value::Null)
        {
            return Err("The deleted row has an incomplete primary key.".into());
        }
        let mut params = Vec::new();
        let predicates = metadata
            .primary_key
            .iter()
            .copied()
            .chain((0..columns).filter(|c| {
                !metadata.primary_key.contains(c)
                    && metadata.comparable[*c]
                    && exact.get(*c).copied().unwrap_or(false)
            }))
            .map(|col| {
                let placeholder = bind(driver, &mut params, original[col].clone());
                format!(
                    "{} {equality} {placeholder}",
                    ident(driver, metadata.physical_columns[col].as_ref().unwrap())
                )
            })
            .collect::<Vec<_>>()
            .join(" AND ");
        out.push(BoundUpdate {
            sql: format!("DELETE FROM {table} WHERE {predicates}"),
            params,
            source_row: row,
        });
    }
    out.extend(updates);
    for (index, values) in edits.inserted.iter().enumerate() {
        if values.len() != columns {
            return Err("A new row has incomplete data.".into());
        }
        let mut params = Vec::new();
        let mut names = Vec::new();
        let mut placeholders = Vec::new();
        for (col, edit) in values.iter().enumerate() {
            if matches!(edit, CellEdit::Default) {
                continue;
            }
            let Some(name) = metadata.physical_columns[col]
                .as_ref()
                .filter(|_| metadata.writable[col])
            else {
                return Err(format!(
                    "Column {} of a new row cannot be set; leave it at its default.",
                    data.columns[col].name
                ));
            };
            names.push(ident(driver, name));
            placeholders.push(bind(
                driver,
                &mut params,
                match edit {
                    CellEdit::Null => Value::Null,
                    CellEdit::Text(text) => Value::Text(text.clone()),
                    CellEdit::Default => unreachable!(),
                },
            ));
        }
        let sql = if names.is_empty() {
            match driver {
                DriverKind::MySql => format!("INSERT INTO {table} () VALUES ()"),
                _ => format!("INSERT INTO {table} DEFAULT VALUES"),
            }
        } else {
            format!(
                "INSERT INTO {table} ({}) VALUES ({})",
                names.join(", "),
                placeholders.join(", ")
            )
        };
        out.push(BoundUpdate {
            sql,
            params,
            source_row: data.rows.len() + index,
        });
    }
    Ok(out)
}

fn bind(driver: DriverKind, params: &mut Vec<Value>, value: Value) -> String {
    params.push(value);
    match driver {
        DriverKind::Postgres => format!("${}", params.len()),
        _ => "?".into(),
    }
}

/// Quotes an identifier in the dialect's style.
pub fn ident(driver: DriverKind, name: &str) -> String {
    let q = if driver == DriverKind::MySql {
        '`'
    } else {
        '"'
    };
    crate::drivers::quote_ident(name, q)
}

pub fn qualified(driver: DriverKind, schema: Option<&str>, table: &str) -> String {
    match schema {
        Some(s) => format!("{}.{}", ident(driver, s), ident(driver, table)),
        None => ident(driver, table),
    }
}

/// String literal; MySQL also treats backslash as an escape character.
pub fn text_literal(driver: DriverKind, s: &str) -> String {
    let mut escaped = s.replace('\'', "''");
    if driver == DriverKind::MySql {
        escaped = escaped.replace('\\', "\\\\");
    }
    format!("'{escaped}'")
}

pub fn literal(driver: DriverKind, v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) if f.is_finite() => f.to_string(),
        Value::Float(f) => text_literal(driver, &f.to_string()),
        Value::Decimal(d) => d.clone(),
        Value::Text(s) => text_literal(driver, s),
        Value::Bytes(b) => {
            let hex: String = b.iter().map(|x| format!("{x:02X}")).collect();
            match driver {
                DriverKind::Postgres => format!("'\\x{hex}'::bytea"),
                _ => format!("X'{hex}'"),
            }
        }
    }
}

/// New value typed into a grid cell.
#[derive(Clone, Debug, PartialEq)]
pub enum CellEdit {
    Null,
    Text(String),
    /// Leave the column out of an INSERT so the database default applies.
    Default,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Column;

    fn cols(names: &[&str]) -> Vec<Column> {
        names
            .iter()
            .map(|n| Column {
                name: n.to_string(),
                type_name: String::new(),
            })
            .collect()
    }

    #[test]
    fn bound_update_preserves_aliases_original_key_and_optimistic_values() {
        use crate::model::{PhysicalTable, ResultSource};
        let data = QueryResult {
            columns: cols(&["alias", "alias"]),
            rows: vec![vec![Value::Int(7), Value::Text("old'value".into())]],
            source: Some(ResultSource {
                session_epoch: 9,
                columns: vec![None; 2],
                exact_columns: vec![true; 2],
            }),
            ..Default::default()
        };
        let mut metadata = ResultEditMetadata {
            table: PhysicalTable {
                schema: "odd\"schema".into(),
                name: "rows".into(),
                oid: None,
            },
            physical_columns: vec![Some("id".into()), Some("note".into())],
            writable: vec![true; 2],
            comparable: vec![true; 2],
            update_default: vec![true; 2],
            primary_key: vec![0],
            session_epoch: 9,
        };
        let text = "new'\\value; DELETE FROM rows; Łódź";
        let changes = HashMap::from([
            ((0, 0), CellEdit::Text("8".into())),
            ((0, 1), CellEdit::Text(text.into())),
        ]);
        for driver in DriverKind::ALL {
            let updates = bound_updates(driver, &metadata, &data, &changes).unwrap();
            assert_eq!(updates.len(), 1);
            assert!(!updates[0].sql.contains(text));
            assert!(!updates[0].sql.contains("alias"));
            assert_eq!(
                updates[0].params,
                [
                    Value::Text("8".into()),
                    Value::Text(text.into()),
                    Value::Int(7),
                    Value::Text("old'value".into())
                ]
            );
            if driver == DriverKind::Postgres {
                assert_eq!(
                    updates[0].sql,
                    "UPDATE \"odd\"\"schema\".\"rows\" SET \"id\" = $1, \"note\" = $2 WHERE \"id\" IS NOT DISTINCT FROM $3 AND \"note\" IS NOT DISTINCT FROM $4"
                );
            }
        }
        metadata.writable[1] = false;
        assert!(bound_updates(DriverKind::Sqlite, &metadata, &data, &changes).is_err());
        metadata.writable[1] = true;
        metadata.update_default[1] = false;
        assert!(
            bound_updates(
                DriverKind::Sqlite,
                &metadata,
                &data,
                &HashMap::from([((0, 1), CellEdit::Default)])
            )
            .is_err()
        );
        assert!(
            bound_updates(
                DriverKind::Sqlite,
                &metadata,
                &data,
                &HashMap::from([((1, 1), CellEdit::Null)])
            )
            .is_err()
        );
        metadata.session_epoch = 10;
        assert!(bound_updates(DriverKind::Sqlite, &metadata, &data, &changes).is_err());
        metadata.session_epoch = 9;
        let mut binary = data.clone();
        binary.rows[0][1] = Value::Bytes(vec![0, 255]);
        assert!(bound_updates(DriverKind::Sqlite, &metadata, &binary, &changes).is_err());
        let null_edit = HashMap::from([((0, 1), CellEdit::Null)]);
        let update = bound_updates(DriverKind::Sqlite, &metadata, &binary, &null_edit).unwrap();
        assert_eq!(update[0].params.last(), Some(&Value::Bytes(vec![0, 255])));
    }

    #[test]
    fn literals_per_dialect() {
        assert_eq!(
            text_literal(DriverKind::Postgres, r"it's C:\x"),
            r"'it''s C:\x'"
        );
        assert_eq!(
            text_literal(DriverKind::MySql, r"it's C:\x"),
            r"'it''s C:\\x'"
        );
        assert_eq!(
            literal(DriverKind::Postgres, &Value::Bytes(vec![0xde, 0xad])),
            r"'\xDEAD'::bytea"
        );
        assert_eq!(literal(DriverKind::Sqlite, &Value::Bytes(vec![1])), "X'01'");
        assert_eq!(
            literal(DriverKind::MySql, &Value::Decimal("10.50".into())),
            "10.50"
        );
        assert_eq!(
            literal(DriverKind::Postgres, &Value::Float(f64::NAN)),
            "'NaN'"
        );
    }
}
