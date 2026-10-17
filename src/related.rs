//! Physical foreign-key mapping and immutable navigation parameters.
//! Mapping visits columns only; capturing a row copies only the chosen key.

use crate::model::{
    CapturedRelatedQuery, DriverKind, ForeignKey, OriginColumn, OriginTable, PhysicalTable,
    QueryResult, RelatedDirection, ResolvedContext, SourceIdentity, Value,
};
use std::collections::{HashMap, HashSet};

pub fn candidate(data: &QueryResult) -> Result<&OriginTable, String> {
    let source = data
        .source
        .as_ref()
        .ok_or("Execute the query again to load related rows.")?;
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
        return Err("Related rows require one unambiguous source table.".into());
    }
    Ok(table)
}

pub fn matches_table(origin: &OriginTable, table: &PhysicalTable) -> bool {
    match origin {
        OriginTable::Postgres(oid) => table.oid == Some(*oid),
        OriginTable::Named {
            schema,
            table: name,
        } => schema == &table.schema && name == &table.name && table.oid.is_none(),
    }
}

pub fn relation_blocked_reason(driver: DriverKind, relation: &ForeignKey) -> Option<&'static str> {
    (driver == DriverKind::MySql && relation.columns.iter().any(|column| {
        column.source_type.eq_ignore_ascii_case("timestamp")
            || column.target_type.eq_ignore_ascii_case("timestamp")
    })).then_some("Related rows with MySQL TIMESTAMP keys require the original session time zone and are not supported yet.")
}

/// Resolve the ordered key against physical origins, independently of UPDATE
/// permissions, output labels, and the current table's primary key.
pub fn columns(
    data: &QueryResult,
    relation: &ForeignKey,
    direction: RelatedDirection,
) -> Result<Vec<usize>, String> {
    let table = candidate(data)?;
    let expected = match direction {
        RelatedDirection::Referenced => &relation.source,
        RelatedDirection::Referencing => &relation.target,
    };
    if !matches_table(table, expected) {
        return Err("This relationship belongs to another result table.".into());
    }
    if relation.columns.is_empty() {
        return Err("This relationship has no columns.".into());
    }
    let source = data.source.as_ref().unwrap();
    let mut positions = HashMap::with_capacity(source.columns.len());
    for (index, origin) in source.columns.iter().enumerate() {
        if let Some(origin) = origin {
            positions
                .entry(&origin.column)
                .and_modify(|position| *position = None)
                .or_insert(Some(index));
        }
    }
    relation
        .columns
        .iter()
        .map(|pair| {
            let origin = match direction {
                RelatedDirection::Referenced => &pair.source_origin,
                RelatedDirection::Referencing => &pair.target_origin,
            };
            let index = positions
                .get(origin)
                .ok_or("This result does not contain all columns of this relationship.")?
                .ok_or("A relationship column appears more than once in this result.")?;
            if !source.exact_columns[index] {
                return Err(
                    "This relationship contains a value that cannot be bound exactly.".into(),
                );
            }
            Ok(index)
        })
        .collect()
}

pub fn capture(
    data: &QueryResult,
    row: usize,
    relation: &ForeignKey,
    direction: RelatedDirection,
    source_identity: SourceIdentity,
    context: ResolvedContext,
) -> Result<CapturedRelatedQuery, String> {
    let indices = columns(data, relation, direction)?;
    let row = data
        .rows
        .get(row)
        .ok_or("The selected row is no longer available.")?;
    if row.len() != data.columns.len() {
        return Err("The selected row has incomplete data.".into());
    }
    let query = CapturedRelatedQuery {
        relation: relation.clone(),
        direction,
        values: indices
            .into_iter()
            .map(|index| row[index].clone())
            .collect(),
        source_identity,
        context,
    };
    validate_query(query.source_identity.driver, &query)?;
    Ok(query)
}

/// Validate persisted structure too. The backend separately verifies the live
/// catalog, source session and current namespace before executing this shape.
pub fn validate_query(driver: DriverKind, query: &CapturedRelatedQuery) -> Result<(), String> {
    if query.source_identity.driver != driver {
        return Err("The related query belongs to a different database driver.".into());
    }
    let relation = &query.relation;
    if let Some(reason) = relation_blocked_reason(driver, relation) {
        return Err(reason.into());
    }
    if relation.columns.is_empty() || relation.columns.len() != query.values.len() {
        return Err("The related query has an incomplete key.".into());
    }
    if query
        .values
        .iter()
        .any(|value| matches!(value, Value::Null))
    {
        return Err("This relationship contains NULL and has no complete key.".into());
    }
    if query.values.iter().any(|value| match value {
        Value::Float(number) => !number.is_finite(),
        Value::Decimal(number) => matches!(
            number.to_ascii_lowercase().as_str(),
            "nan" | "infinity" | "-infinity" | "+infinity" | "inf" | "-inf" | "+inf"
        ),
        _ => false,
    }) {
        return Err("This relationship contains a non-finite key.".into());
    }
    if query.context.temporary_schema.is_some() {
        return Err(
            "Related rows cannot reproduce a temporary namespace in a separate session.".into(),
        );
    }
    let valid_name = |name: &str| !name.is_empty() && !name.contains('\0');
    for table in [&relation.source, &relation.target] {
        if !valid_name(&table.schema)
            || !valid_name(&table.name)
            || (driver == DriverKind::Postgres && table.oid.is_none_or(|oid| oid == 0))
            || (driver != DriverKind::Postgres && table.oid.is_some())
        {
            return Err("The related query has an invalid table identity.".into());
        }
        if driver == DriverKind::Sqlite && table.schema != "main" {
            return Err("Related rows from temporary tables or attachments require the original session and are not supported yet.".into());
        }
    }
    if !valid_name(&relation.catalog_id) {
        return Err("The related query has no catalog identity.".into());
    }
    let mut source_columns = HashSet::with_capacity(relation.columns.len());
    let mut target_columns = HashSet::with_capacity(relation.columns.len());
    for pair in &relation.columns {
        for (name, origin) in [
            (&pair.source_name, &pair.source_origin),
            (&pair.target_name, &pair.target_origin),
        ] {
            let valid_origin = match (driver, origin) {
                (DriverKind::Postgres, OriginColumn::Postgres(ordinal)) => *ordinal > 0,
                (DriverKind::MySql | DriverKind::Sqlite, OriginColumn::Named(physical)) => {
                    physical == name
                }
                _ => false,
            };
            if !valid_name(name) || !valid_origin {
                return Err("The related query has an invalid column identity.".into());
            }
        }
        if !source_columns.insert(&pair.source_origin)
            || !target_columns.insert(&pair.target_origin)
        {
            return Err("The related query has repeated key columns.".into());
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::{Column, ColumnOrigin, DataSource, ForeignKeyColumn, ResultSource};

    pub(crate) fn fixture() -> (QueryResult, ForeignKey, SourceIdentity, ResolvedContext) {
        let source_table = PhysicalTable {
            schema: "main".into(),
            name: "invoices".into(),
            oid: None,
        };
        let target_table = PhysicalTable {
            schema: "main".into(),
            name: "accounts".into(),
            oid: None,
        };
        let relation = ForeignKey {
            name: "bill_to".into(),
            catalog_id: "0".into(),
            source: source_table,
            target: target_table,
            columns: [("account", "code"), ("tenant", "tenant")]
                .into_iter()
                .map(|(source, target)| ForeignKeyColumn {
                    source_name: source.into(),
                    source_origin: OriginColumn::Named(source.into()),
                    source_type: "TEXT".into(),
                    target_name: target.into(),
                    target_origin: OriginColumn::Named(target.into()),
                    target_type: "TEXT".into(),
                })
                .collect(),
        };
        let data = QueryResult {
            columns: ["tenant_alias", "account_alias"]
                .into_iter()
                .map(|name| Column {
                    name: name.into(),
                    type_name: "TEXT".into(),
                })
                .collect(),
            rows: vec![vec![Value::Int(7), Value::Text("A'\\Żółw; --".into())]],
            source: Some(ResultSource {
                session_epoch: 7,
                columns: ["tenant", "account"]
                    .into_iter()
                    .map(|name| {
                        Some(ColumnOrigin {
                            table: OriginTable::Named {
                                schema: "main".into(),
                                table: "invoices".into(),
                            },
                            column: OriginColumn::Named(name.into()),
                        })
                    })
                    .collect(),
                exact_columns: vec![true, true],
            }),
            ..Default::default()
        };
        let mut ds = DataSource::new(DriverKind::Sqlite);
        ds.file = "/tmp/related-fixture.sqlite".into();
        ds.read_only = true;
        (
            data,
            relation,
            ds.identity(),
            ResolvedContext {
                schemas: vec!["main".into()],
                ..Default::default()
            },
        )
    }

    #[test]
    fn ordered_physical_keys_ignore_aliases_and_do_not_require_primary_key_or_update_permission() {
        let (data, relation, identity, context) = fixture();
        assert_eq!(
            columns(&data, &relation, RelatedDirection::Referenced).unwrap(),
            [1, 0]
        );
        let query = capture(
            &data,
            0,
            &relation,
            RelatedDirection::Referenced,
            identity,
            context,
        )
        .unwrap();
        assert_eq!(
            query.values,
            [Value::Text("A'\\Żółw; --".into()), Value::Int(7)]
        );
        let generated = crate::sqlgen::related_query(DriverKind::Sqlite, &query).unwrap();
        assert!(!generated.sql.contains("Żółw"));
        assert_eq!(generated.params, query.values);
    }

    #[test]
    fn missing_lossy_ambiguous_null_and_restored_keys_are_rejected() {
        let (mut data, relation, identity, context) = fixture();
        data.source.as_mut().unwrap().exact_columns[0] = false;
        assert!(columns(&data, &relation, RelatedDirection::Referenced).is_err());
        data.source.as_mut().unwrap().exact_columns[0] = true;
        data.rows[0][0] = Value::Null;
        assert!(
            capture(
                &data,
                0,
                &relation,
                RelatedDirection::Referenced,
                identity,
                context
            )
            .is_err()
        );
        data.source.as_mut().unwrap().columns[0] = data.source.as_ref().unwrap().columns[1].clone();
        assert!(columns(&data, &relation, RelatedDirection::Referenced).is_err());
        data.source = None;
        assert!(candidate(&data).is_err());
    }

    #[test]
    fn persisted_parameters_remain_typed_and_malformed_shapes_are_rejected() {
        let (data, relation, identity, context) = fixture();
        let mut query = capture(
            &data,
            0,
            &relation,
            RelatedDirection::Referenced,
            identity,
            context,
        )
        .unwrap();
        query.values = vec![
            Value::Bytes(vec![0, 255, 39]),
            Value::Decimal("18446744073709551615".into()),
        ];
        let restored: CapturedRelatedQuery =
            serde_json::from_str(&serde_json::to_string(&query).unwrap()).unwrap();
        assert_eq!(restored.values, query.values);
        validate_query(DriverKind::Sqlite, &restored).unwrap();
        query.values[0] = Value::Float(f64::NAN);
        assert!(validate_query(DriverKind::Sqlite, &query).is_err());
        query.values.remove(0);
        assert!(validate_query(DriverKind::Sqlite, &query).is_err());
    }

    #[test]
    fn incoming_uses_parent_collation_and_affinity_without_interpolating_values() {
        let (data, relation, identity, context) = fixture();
        let mut query = capture(
            &data,
            0,
            &relation,
            RelatedDirection::Referenced,
            identity,
            context,
        )
        .unwrap();
        query.direction = RelatedDirection::Referencing;
        let generated = crate::sqlgen::related_query(DriverKind::Sqlite, &query).unwrap();
        assert!(generated.sql.contains("p.\"code\" = +c.\"account\""));
        assert!(generated.sql.contains("EXISTS"));
        query.source_identity.driver = DriverKind::MySql;
        query.relation.columns[0].source_type = "TIMESTAMP".into();
        assert!(validate_query(DriverKind::MySql, &query).is_err());
        query.source_identity.driver = DriverKind::Sqlite;
        validate_query(DriverKind::Sqlite, &query).unwrap();
    }
}
