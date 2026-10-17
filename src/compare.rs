//! Read-only comparison of fetched result snapshots, using exact typed keys.
//!
//! This does not implement database collation or numeric coercion. Decimal
//! spelling and scale are significant, all NaNs compare equal outside keys,
//! and positive/negative floating-point zero compare equal. NULL and NaN keys
//! are rejected. No values or composite keys are copied into the row index.

use crate::{
    filter::FilterValue,
    model::{QueryResult, Value},
};
use std::{
    collections::{
        HashMap, HashSet,
        hash_map::{Entry, RandomState},
    },
    hash::{BuildHasher, Hash, Hasher},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnPair {
    pub left: usize,
    pub right: usize,
}

#[derive(Clone, Debug)]
pub struct CompareSpec {
    pub columns: Vec<ColumnPair>,
    /// Indices into `columns`, in composite-key order.
    pub key_pairs: Vec<usize>,
}

pub struct CompareRequest {
    pub request_id: u64,
    pub left: Arc<QueryResult>,
    pub right: Arc<QueryResult>,
    pub spec: CompareSpec,
    pub cancelled: Arc<AtomicBool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Difference {
    Equal,
    Changed,
    LeftOnly,
    RightOnly,
}

#[derive(Debug)]
pub struct ComparedRow {
    pub left: Option<usize>,
    pub right: Option<usize>,
    pub difference: Difference,
    changed_words: Option<usize>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct CompareCounts {
    pub equal: usize,
    pub changed: usize,
    pub left_only: usize,
    pub right_only: usize,
}

#[derive(Debug)]
pub struct ComparedData {
    pub left: Arc<QueryResult>,
    pub right: Arc<QueryResult>,
    pub spec: CompareSpec,
    pub rows: Vec<ComparedRow>,
    pub counts: CompareCounts,
    /// Worker CPU/wall duration, excluding time queued for a worker.
    pub elapsed: Duration,
    changed_columns: Vec<u64>,
}

impl ComparedData {
    /// Only matched, changed rows have changed cells. Missing sides are
    /// represented by the row status; invalid indices return false.
    pub fn cell_changed(&self, row: usize, pair: usize) -> bool {
        if pair >= self.spec.columns.len() {
            return false;
        }
        self.rows
            .get(row)
            .and_then(|row| row.changed_words)
            .and_then(|offset| self.changed_columns.get(offset + pair / 64))
            .is_some_and(|word| word & (1 << (pair % 64)) != 0)
    }
}

/// Propose only exact, case-sensitive names occurring once on each side.
/// The output follows left-column order and never inspects row values.
pub fn matching_columns(left: &QueryResult, right: &QueryResult) -> Vec<ColumnPair> {
    fn unique(data: &QueryResult) -> HashMap<&str, Option<usize>> {
        let mut names = HashMap::with_capacity(data.columns.len());
        for (index, column) in data.columns.iter().enumerate() {
            names
                .entry(column.name.as_str())
                .and_modify(|old| *old = None)
                .or_insert(Some(index));
        }
        names
    }
    let left_names = unique(left);
    let right_names = unique(right);
    left.columns
        .iter()
        .enumerate()
        .filter_map(|(index, column)| {
            if left_names.get(column.name.as_str()) != Some(&Some(index)) {
                return None;
            }
            right_names
                .get(column.name.as_str())
                .copied()
                .flatten()
                .map(|right| ColumnPair { left: index, right })
        })
        .collect()
}

/// Validate only columns and key mapping, without scanning rows. The worker
/// repeats this validation and also checks row shape and key uniqueness.
pub fn validate_spec(
    left: &QueryResult,
    right: &QueryResult,
    spec: &CompareSpec,
) -> Result<(), String> {
    if left.columns.is_empty() || right.columns.is_empty() {
        return Err("Both snapshots must have result columns.".into());
    }
    if spec.columns.is_empty() {
        return Err("Map at least one column pair before comparing.".into());
    }
    if spec.key_pairs.is_empty() {
        return Err("Choose at least one mapped column pair as the comparison key.".into());
    }
    let mut left_seen = HashSet::with_capacity(spec.columns.len());
    let mut right_seen = HashSet::with_capacity(spec.columns.len());
    for (index, pair) in spec.columns.iter().enumerate() {
        if pair.left >= left.columns.len() || pair.right >= right.columns.len() {
            return Err(format!(
                "Mapped column pair {} is outside the snapshot columns.",
                index + 1
            ));
        }
        if !left_seen.insert(pair.left) || !right_seen.insert(pair.right) {
            return Err("Each column can occur in only one mapped pair.".into());
        }
    }
    let mut key_seen = HashSet::with_capacity(spec.key_pairs.len());
    for &pair in &spec.key_pairs {
        if pair >= spec.columns.len() {
            return Err(format!(
                "Key pair {} is outside the column mapping.",
                pair.saturating_add(1)
            ));
        }
        if !key_seen.insert(pair) {
            return Err("Each mapped pair can occur in the comparison key only once.".into());
        }
    }
    Ok(())
}

const CANCELLED: &str = "Data comparison cancelled.";
const BYTE_CHUNK: usize = 64 * 1024;

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), String> {
    if cancelled.load(Ordering::Relaxed) {
        Err(CANCELLED.into())
    } else {
        Ok(())
    }
}

/// Borrowed composite key plus a precomputed, randomized fingerprint. Hashing
/// the input separately permits cancellation while reading large text/BLOBs.
/// Equality still compares complete typed values, including on hash collisions.
#[derive(Clone, Copy)]
struct RowKey<'a> {
    row: &'a [Value],
    columns: &'a [usize],
    fingerprint: u64,
}

impl PartialEq for RowKey<'_> {
    fn eq(&self, other: &Self) -> bool {
        // HashMap equality cannot return a cancellation error. Full value
        // comparisons here remain bounded by one key lookup; the caller polls
        // immediately before/after it, and hashing polls every 64 KiB.
        self.columns.len() == other.columns.len()
            && self
                .columns
                .iter()
                .zip(other.columns)
                .all(|(&left, &right)| {
                    FilterValue::from_value(&self.row[left])
                        == FilterValue::from_value(&other.row[right])
                })
    }
}
impl Eq for RowKey<'_> {}
impl Hash for RowKey<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.fingerprint.hash(state);
    }
}

fn hash_bytes(
    bytes: &[u8],
    hasher: &mut impl Hasher,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    bytes.len().hash(hasher);
    for chunk in bytes.chunks(BYTE_CHUNK) {
        check_cancelled(cancelled)?;
        hasher.write(chunk);
    }
    Ok(())
}

fn row_key<'a>(
    data: &'a QueryResult,
    row_index: usize,
    columns: &'a [usize],
    side: &str,
    random: &RandomState,
    cancelled: &AtomicBool,
) -> Result<RowKey<'a>, String> {
    check_cancelled(cancelled)?;
    let row = &data.rows[row_index];
    if row.len() != data.columns.len() {
        return Err(format!(
            "{side} row {} has {} values; expected {} columns.",
            row_index + 1,
            row.len(),
            data.columns.len()
        ));
    }
    let mut hasher = random.build_hasher();
    columns.len().hash(&mut hasher);
    for &column in columns {
        check_cancelled(cancelled)?;
        let value = &row[column];
        if matches!(value, Value::Null) || matches!(value, Value::Float(value) if value.is_nan()) {
            return Err(format!(
                "{side} row {}, column {} has a NULL or NaN comparison key.",
                row_index + 1,
                column + 1
            ));
        }
        // Tags disambiguate byte-like types; scalar FilterValue hashing already
        // preserves their types and canonicalizes signed floating-point zero.
        match value {
            Value::Text(text) => {
                0_u8.hash(&mut hasher);
                hash_bytes(text.as_bytes(), &mut hasher, cancelled)?;
            }
            Value::Decimal(text) => {
                1_u8.hash(&mut hasher);
                hash_bytes(text.as_bytes(), &mut hasher, cancelled)?;
            }
            Value::Bytes(bytes) => {
                2_u8.hash(&mut hasher);
                hash_bytes(bytes, &mut hasher, cancelled)?;
            }
            _ => {
                3_u8.hash(&mut hasher);
                FilterValue::from_value(value).hash(&mut hasher);
            }
        }
    }
    Ok(RowKey {
        row,
        columns,
        fingerprint: hasher.finish(),
    })
}

fn insert_unique<'a>(
    index: &mut HashMap<RowKey<'a>, usize>,
    key: RowKey<'a>,
    row: usize,
    side: &str,
) -> Result<(), String> {
    match index.entry(key) {
        Entry::Vacant(entry) => {
            entry.insert(row);
            Ok(())
        }
        Entry::Occupied(entry) => Err(format!(
            "{side} rows {} and {} have the same comparison key.",
            entry.get() + 1,
            row + 1
        )),
    }
}

fn bytes_equal(left: &[u8], right: &[u8], cancelled: &AtomicBool) -> Result<bool, String> {
    if left.len() != right.len() {
        return Ok(false);
    }
    for (left, right) in left.chunks(BYTE_CHUNK).zip(right.chunks(BYTE_CHUNK)) {
        check_cancelled(cancelled)?;
        if left != right {
            return Ok(false);
        }
    }
    Ok(true)
}

fn values_equal(left: &Value, right: &Value, cancelled: &AtomicBool) -> Result<bool, String> {
    check_cancelled(cancelled)?;
    match (left, right) {
        (Value::Text(left), Value::Text(right)) | (Value::Decimal(left), Value::Decimal(right)) => {
            bytes_equal(left.as_bytes(), right.as_bytes(), cancelled)
        }
        (Value::Bytes(left), Value::Bytes(right)) => bytes_equal(left, right, cancelled),
        _ => Ok(FilterValue::from_value(left) == FilterValue::from_value(right)),
    }
}

/// Compare two fetched snapshots. Output order is left rows followed by
/// unmatched right rows. Keys are unique on each side, even in unmatched rows.
/// Memory is proportional to row indices and changed-row bitmaps; snapshots
/// remain shared through Arc, with no copied values or allocated per-row keys.
pub fn compare(request: CompareRequest) -> Result<ComparedData, String> {
    let started = Instant::now();
    let CompareRequest {
        left,
        right,
        spec,
        cancelled,
        ..
    } = request;
    check_cancelled(&cancelled)?;
    validate_spec(&left, &right, &spec)?;
    let left_key: Vec<_> = spec
        .key_pairs
        .iter()
        .map(|&pair| spec.columns[pair].left)
        .collect();
    let right_key: Vec<_> = spec
        .key_pairs
        .iter()
        .map(|&pair| spec.columns[pair].right)
        .collect();
    let random = RandomState::new();
    let mut right_index = HashMap::with_capacity(right.rows.len());
    for row in 0..right.rows.len() {
        let key = row_key(&right, row, &right_key, "Right", &random, &cancelled)?;
        insert_unique(&mut right_index, key, row, "Right")?;
    }
    let mut left_index = HashMap::with_capacity(left.rows.len());
    let mut right_matched = vec![false; right.rows.len()];
    let mut rows = Vec::with_capacity(left.rows.len().max(right.rows.len()));
    let mut changed_columns = Vec::new();
    let mut counts = CompareCounts::default();
    let words_per_row = spec.columns.len().div_ceil(64);
    for left_row in 0..left.rows.len() {
        let key = row_key(&left, left_row, &left_key, "Left", &random, &cancelled)?;
        insert_unique(&mut left_index, key, left_row, "Left")?;
        let right_row = right_index.get(&key).copied();
        check_cancelled(&cancelled)?;
        let Some(right_row) = right_row else {
            counts.left_only += 1;
            rows.push(ComparedRow {
                left: Some(left_row),
                right: None,
                difference: Difference::LeftOnly,
                changed_words: None,
            });
            continue;
        };
        right_matched[right_row] = true;
        let offset = changed_columns.len();
        let new_len = offset
            .checked_add(words_per_row)
            .ok_or("The comparison bitmap is too large.")?;
        changed_columns.resize(new_len, 0_u64);
        let mut changed = false;
        for (pair_index, pair) in spec.columns.iter().enumerate() {
            if !values_equal(
                &left.rows[left_row][pair.left],
                &right.rows[right_row][pair.right],
                &cancelled,
            )? {
                changed_columns[offset + pair_index / 64] |= 1 << (pair_index % 64);
                changed = true;
            }
        }
        let difference = if changed {
            counts.changed += 1;
            Difference::Changed
        } else {
            changed_columns.truncate(offset);
            counts.equal += 1;
            Difference::Equal
        };
        rows.push(ComparedRow {
            left: Some(left_row),
            right: Some(right_row),
            difference,
            changed_words: changed.then_some(offset),
        });
    }
    for (row, matched) in right_matched.into_iter().enumerate() {
        check_cancelled(&cancelled)?;
        if !matched {
            counts.right_only += 1;
            rows.push(ComparedRow {
                left: None,
                right: Some(row),
                difference: Difference::RightOnly,
                changed_words: None,
            });
        }
    }
    check_cancelled(&cancelled)?;
    drop(left_index);
    drop(right_index);
    Ok(ComparedData {
        left,
        right,
        spec,
        rows,
        counts,
        elapsed: started.elapsed(),
        changed_columns,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Column;
    use std::{hint::black_box, mem::size_of};

    fn data(names: &[&str], rows: Vec<Vec<Value>>) -> Arc<QueryResult> {
        Arc::new(QueryResult {
            columns: names
                .iter()
                .map(|name| Column {
                    name: (*name).into(),
                    type_name: String::new(),
                })
                .collect(),
            rows,
            ..Default::default()
        })
    }

    fn request(
        left: Arc<QueryResult>,
        right: Arc<QueryResult>,
        key_pairs: Vec<usize>,
    ) -> CompareRequest {
        let columns = (0..left.columns.len())
            .map(|column| ColumnPair {
                left: column,
                right: column,
            })
            .collect();
        CompareRequest {
            request_id: 1,
            left,
            right,
            spec: CompareSpec { columns, key_pairs },
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    fn text(value: &str) -> Value {
        Value::Text(value.into())
    }
    fn decimal(value: &str) -> Value {
        Value::Decimal(value.into())
    }

    #[test]
    fn composite_mapping_pairs_rows_by_key_and_retains_snapshots() {
        let left = data(
            &["tenant", "id", "name", "amount", "payload", "note"],
            vec![
                vec![
                    Value::Int(7),
                    Value::Int(1),
                    text("Ada"),
                    decimal("10.00"),
                    Value::Bytes(vec![1; 64]),
                    text("Łódź\n\t"),
                ],
                vec![
                    Value::Int(7),
                    Value::Int(2),
                    text("Olek"),
                    decimal("20.0"),
                    Value::Bytes(vec![2; 64]),
                    Value::Null,
                ],
                vec![
                    Value::Int(8),
                    Value::Int(1),
                    text("Iga"),
                    decimal("30"),
                    Value::Bytes(vec![]),
                    text("<null>"),
                ],
                vec![
                    Value::Int(7),
                    Value::Int(3),
                    text("Lena"),
                    decimal("40"),
                    Value::Bytes(vec![3]),
                    text("left"),
                ],
            ],
        );
        let mut changed_bytes = vec![2; 64];
        changed_bytes[63] = 9;
        let right = data(
            &["name", "id", "tenant", "balance", "payload", "note"],
            vec![
                vec![
                    text("Iga"),
                    Value::Int(1),
                    Value::Int(8),
                    decimal("30"),
                    Value::Bytes(vec![]),
                    text("<null>"),
                ],
                vec![
                    text("Olek changed"),
                    Value::Int(2),
                    Value::Int(7),
                    decimal("20.00"),
                    Value::Bytes(changed_bytes),
                    text("<null>"),
                ],
                vec![
                    text("New"),
                    Value::Int(4),
                    Value::Int(7),
                    decimal("50"),
                    Value::Bytes(vec![]),
                    text("right"),
                ],
                vec![
                    text("Ada"),
                    Value::Int(1),
                    Value::Int(7),
                    decimal("10.00"),
                    Value::Bytes(vec![1; 64]),
                    text("Łódź\n\t"),
                ],
            ],
        );
        let mut req = request(left.clone(), right.clone(), vec![0, 1]);
        req.spec.columns = [(0, 2), (1, 1), (2, 0), (3, 3), (4, 4), (5, 5)]
            .map(|(left, right)| ColumnPair { left, right })
            .into();
        let result = compare(req).unwrap();
        assert!(Arc::ptr_eq(&result.left, &left));
        assert!(Arc::ptr_eq(&result.right, &right));
        assert_eq!(
            result
                .rows
                .iter()
                .map(|row| (row.left, row.right, row.difference))
                .collect::<Vec<_>>(),
            vec![
                (Some(0), Some(3), Difference::Equal),
                (Some(1), Some(1), Difference::Changed),
                (Some(2), Some(0), Difference::Equal),
                (Some(3), None, Difference::LeftOnly),
                (None, Some(2), Difference::RightOnly),
            ]
        );
        assert_eq!(
            result.counts,
            CompareCounts {
                equal: 2,
                changed: 1,
                left_only: 1,
                right_only: 1
            }
        );
        for row in 0..result.rows.len() {
            for pair in 0..6 {
                assert_eq!(result.cell_changed(row, pair), row == 1 && pair >= 2);
            }
        }
        assert!(!result.cell_changed(usize::MAX, 0));
        assert!(!result.cell_changed(0, usize::MAX));
        let weak = Arc::downgrade(&left);
        drop(left);
        drop(right);
        assert!(weak.upgrade().is_some());
        drop(result);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn suggestions_require_unique_exact_names_and_follow_left_order() {
        let left = data(&["id", "dup", "dup", "Name", "name", "left"], vec![]);
        let right = data(&["name", "id", "dup", "Name", "Name", "right"], vec![]);
        assert_eq!(
            matching_columns(&left, &right),
            vec![
                ColumnPair { left: 0, right: 1 },
                ColumnPair { left: 4, right: 0 }
            ]
        );
    }

    #[test]
    fn empty_pages_and_one_sided_rows_still_validate_their_keys() {
        let empty = data(&["key"], vec![]);
        assert!(
            compare(request(empty.clone(), empty.clone(), vec![0]))
                .unwrap()
                .rows
                .is_empty()
        );
        let values = data(&["key"], vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
        let left = compare(request(values.clone(), empty.clone(), vec![0])).unwrap();
        assert_eq!(left.counts.left_only, 2);
        assert!(!left.cell_changed(0, 0));
        let right = compare(request(empty.clone(), values, vec![0])).unwrap();
        assert_eq!(right.counts.right_only, 2);
        for (side, invalid) in [("Left", true), ("Right", false)] {
            let null = data(&["key"], vec![vec![Value::Null]]);
            let req = if invalid {
                request(null, empty.clone(), vec![0])
            } else {
                request(empty.clone(), null, vec![0])
            };
            assert!(
                compare(req)
                    .unwrap_err()
                    .contains(&format!("{side} row 1, column 1"))
            );
        }
    }

    #[test]
    fn invalid_mapping_and_malformed_rows_fail_without_panicking() {
        let empty = data(&["a", "b"], vec![]);
        let valid = CompareSpec {
            columns: vec![
                ColumnPair { left: 0, right: 0 },
                ColumnPair { left: 1, right: 1 },
            ],
            key_pairs: vec![0],
        };
        let mut specs = vec![
            CompareSpec {
                columns: vec![],
                key_pairs: vec![0],
            },
            CompareSpec {
                columns: valid.columns.clone(),
                key_pairs: vec![],
            },
        ];
        for (left, right) in [(2, 1), (1, 2), (0, 1), (1, 0)] {
            specs.push(CompareSpec {
                columns: vec![valid.columns[0].clone(), ColumnPair { left, right }],
                key_pairs: vec![0],
            });
        }
        specs.push(CompareSpec {
            columns: valid.columns.clone(),
            key_pairs: vec![2],
        });
        specs.push(CompareSpec {
            columns: valid.columns.clone(),
            key_pairs: vec![usize::MAX],
        });
        specs.push(CompareSpec {
            columns: valid.columns.clone(),
            key_pairs: vec![0, 0],
        });
        for spec in specs {
            let mut req = request(empty.clone(), empty.clone(), vec![0]);
            req.spec = spec;
            assert!(compare(req).is_err());
        }
        let no_columns = data(&[], vec![]);
        assert!(compare(request(no_columns.clone(), no_columns, vec![0])).is_err());
        for (bad_row, side) in [
            (vec![Value::Int(1)], "Left"),
            (vec![Value::Int(1), Value::Int(2), Value::Int(3)], "Right"),
        ] {
            let malformed = data(&["a", "b"], vec![bad_row]);
            let req = if side == "Left" {
                request(malformed, empty.clone(), vec![0])
            } else {
                request(empty.clone(), malformed, vec![0])
            };
            assert!(
                compare(req)
                    .unwrap_err()
                    .contains(&format!("{side} row 1 has"))
            );
        }
    }

    #[test]
    fn duplicate_null_and_nan_keys_report_side_and_rows() {
        let empty = data(&["key"], vec![]);
        for is_left in [true, false] {
            let side = if is_left { "Left" } else { "Right" };
            for values in [
                vec![Value::Int(1), Value::Int(2), Value::Int(1)],
                vec![Value::Float(0.0), Value::Float(-0.0)],
            ] {
                let last = values.len();
                let duplicate = data(
                    &["key"],
                    values.into_iter().map(|value| vec![value]).collect(),
                );
                let req = if is_left {
                    request(duplicate, empty.clone(), vec![0])
                } else {
                    request(empty.clone(), duplicate, vec![0])
                };
                assert!(
                    compare(req)
                        .unwrap_err()
                        .contains(&format!("{side} rows 1 and {last}"))
                );
            }
            for invalid in [Value::Null, Value::Float(f64::NAN)] {
                let invalid = data(&["key"], vec![vec![invalid]]);
                let req = if is_left {
                    request(invalid, empty.clone(), vec![0])
                } else {
                    request(empty.clone(), invalid, vec![0])
                };
                assert!(
                    compare(req)
                        .unwrap_err()
                        .contains(&format!("{side} row 1, column 1"))
                );
            }
        }
    }

    #[test]
    fn exact_types_full_payloads_and_float_canonicalization() {
        let long = "λ".repeat(200);
        let left_values = vec![
            Value::Int(1),
            Value::Int(9_007_199_254_740_993),
            decimal("18446744073709551615"),
            decimal("1.0"),
            Value::Bool(true),
            Value::Null,
            Value::Null,
            text(&long),
            text("é"),
            text("space "),
            Value::Bytes(vec![1; 100]),
            Value::Float(-0.0),
            Value::Float(f64::from_bits(0x7ff8_0000_0000_0001)),
            Value::Float(f64::INFINITY),
        ];
        let mut bytes = vec![1; 100];
        bytes[99] = 2;
        let right_values = vec![
            text("1"),
            Value::Int(9_007_199_254_740_992),
            decimal("18446744073709551614"),
            decimal("1.00"),
            Value::Int(1),
            text("<null>"),
            Value::Null,
            text(&format!("{long}x")),
            text("e\u{301}"),
            text("space"),
            Value::Bytes(bytes),
            Value::Float(0.0),
            Value::Float(f64::from_bits(0xfff0_0000_0000_0001)),
            Value::Float(f64::INFINITY),
        ];
        let rows = |values: Vec<Value>| {
            values
                .into_iter()
                .enumerate()
                .map(|(id, value)| vec![Value::Int(id as i64), value])
                .collect()
        };
        let left = data(&["key", "value"], rows(left_values));
        let right = data(&["key", "value"], rows(right_values));
        let result = compare(request(left, right, vec![0])).unwrap();
        for row in 0..14 {
            assert_eq!(result.cell_changed(row, 1), ![6, 11, 12, 13].contains(&row));
            assert!(!result.cell_changed(row, 0));
        }
        let keys = vec![
            Value::Int(1),
            text("1"),
            Value::Float(1.0),
            decimal("1"),
            Value::Float(f64::INFINITY),
            Value::Float(f64::NEG_INFINITY),
            Value::Float(-0.0),
        ];
        let left = data(
            &["key"],
            keys.iter().cloned().map(|value| vec![value]).collect(),
        );
        let right = data(
            &["key"],
            keys.into_iter().rev().map(|value| vec![value]).collect(),
        );
        assert_eq!(
            compare(request(left, right, vec![0])).unwrap().counts.equal,
            7
        );
    }

    #[test]
    fn bitmap_crosses_word_boundaries_without_leaking_between_rows() {
        let names: Vec<_> = (0..129).map(|index| format!("c{index}")).collect();
        let refs: Vec<_> = names.iter().map(String::as_str).collect();
        let left = data(
            &refs,
            (0..3)
                .map(|row| {
                    (0..129)
                        .map(|column| Value::Int(if column == 1 { row } else { column as i64 }))
                        .collect()
                })
                .collect(),
        );
        let mut right = (*left).clone();
        for column in [0, 63, 64, 128] {
            right.rows[0][column] = Value::Int(-1);
        }
        right.rows[2][64] = Value::Int(-2);
        let result = compare(request(left, Arc::new(right), vec![1])).unwrap();
        assert_eq!(result.changed_columns.len(), 6);
        for row in 0..3 {
            for pair in 0..129 {
                assert_eq!(
                    result.cell_changed(row, pair),
                    (row == 0 && [0, 63, 64, 128].contains(&pair)) || (row == 2 && pair == 64)
                );
            }
        }
    }

    #[test]
    fn fingerprint_collisions_still_compare_complete_keys() {
        let first = vec![Value::Int(1), text("same prefix A")];
        let second = vec![Value::Int(1), text("same prefix B")];
        let columns = [0, 1];
        let first = RowKey {
            row: &first,
            columns: &columns,
            fingerprint: 7,
        };
        let second = RowKey {
            row: &second,
            columns: &columns,
            fingerprint: 7,
        };
        let mut index = HashMap::new();
        insert_unique(&mut index, first, 0, "Left").unwrap();
        insert_unique(&mut index, second, 1, "Left").unwrap();
        assert_eq!(index.get(&first), Some(&0));
        assert_eq!(index.get(&second), Some(&1));
    }

    #[test]
    fn large_text_and_binary_keys_use_the_tail_and_keep_their_types() {
        let prefix = "a".repeat(BYTE_CHUNK);
        let keys = vec![
            text(&format!("{prefix}A")),
            text(&format!("{prefix}B")),
            Value::Bytes(format!("{prefix}A").into_bytes()),
        ];
        let left = data(
            &["key"],
            keys.iter().cloned().map(|key| vec![key]).collect(),
        );
        let right = data(
            &["key"],
            keys.into_iter().rev().map(|key| vec![key]).collect(),
        );
        let result = compare(request(left, right, vec![0])).unwrap();
        assert_eq!(result.counts.equal, 3);
        assert_eq!(result.rows[0].right, Some(2));
        assert_eq!(result.rows[2].right, Some(0));
    }

    #[test]
    fn cancellation_precedes_validation_and_interrupts_large_key_hashing() {
        let empty = data(&[], vec![]);
        let req = request(empty.clone(), empty, vec![]);
        req.cancelled.store(true, Ordering::Relaxed);
        assert_eq!(compare(req).unwrap_err(), CANCELLED);
        struct CancelOnChunk<'a> {
            cancelled: &'a AtomicBool,
            chunks: usize,
        }
        impl Hasher for CancelOnChunk<'_> {
            fn finish(&self) -> u64 {
                0
            }
            fn write(&mut self, bytes: &[u8]) {
                if bytes.len() == BYTE_CHUNK {
                    self.chunks += 1;
                    self.cancelled.store(true, Ordering::Relaxed);
                }
            }
        }
        let cancelled = AtomicBool::new(false);
        let mut hasher = CancelOnChunk {
            cancelled: &cancelled,
            chunks: 0,
        };
        assert_eq!(
            hash_bytes(&vec![7; BYTE_CHUNK * 3], &mut hasher, &cancelled).unwrap_err(),
            CANCELLED
        );
        assert_eq!(hasher.chunks, 1);
    }

    fn fixture(
        row_count: usize,
        columns: usize,
        all_changed: bool,
        blob_size: usize,
    ) -> (Arc<QueryResult>, Arc<QueryResult>) {
        let names: Vec<_> = (0..columns).map(|column| format!("c{column}")).collect();
        let refs: Vec<_> = names.iter().map(String::as_str).collect();
        let rows = (0..row_count)
            .map(|row| {
                (0..columns)
                    .map(|column| match column {
                        0 => Value::Int(row as i64),
                        1 => text(&format!("Customer {row:06} Łódź")),
                        2 => decimal(&format!("18446744073709551615.{row:06}")),
                        3 => Value::Float(row as f64 / 7.0),
                        4 => Value::Bytes(vec![(row % 251) as u8; blob_size]),
                        5 => Value::Null,
                        _ => Value::Int((row + column) as i64),
                    })
                    .collect()
            })
            .collect();
        let left = data(&refs, rows);
        let mut right = (*left).clone();
        for (row, values) in right.rows.iter_mut().enumerate() {
            if all_changed || row % 100 == 0 {
                values[1] = text(&format!("Changed {row}"));
            }
        }
        right.rows.reverse();
        (left, Arc::new(right))
    }

    #[test]
    #[ignore = "release CPU benchmark; no database or GPU"]
    fn benchmark() {
        let mut cases = Vec::new();
        for rows in [500, 5_000, 100_000] {
            for all_changed in [false, true] {
                cases.push((rows, 8, all_changed, 32, false));
            }
        }
        cases.extend([
            (5_000, 129, false, 32, false),
            (5_000, 129, true, 32, false),
            (5_000, 8, false, 4096, false),
            (5_000, 8, false, 4096, true),
        ]);
        for (row_count, columns, all_changed, blob_size, blob_key) in cases {
            let (left, right) = fixture(row_count, columns, all_changed, blob_size);
            let payload_bytes: usize = left
                .rows
                .iter()
                .chain(&right.rows)
                .flatten()
                .map(|value| match value {
                    Value::Text(value) | Value::Decimal(value) => value.len(),
                    Value::Bytes(value) => value.len(),
                    _ => 0,
                })
                .sum();
            let make_request = || {
                request(
                    left.clone(),
                    right.clone(),
                    if blob_key { vec![0, 4] } else { vec![0] },
                )
            };
            black_box(compare(make_request()).unwrap());
            let mut samples = Vec::new();
            let mut buffers = 0;
            let mut changed = 0;
            for _ in 0..7 {
                let started = Instant::now();
                let result = compare(make_request()).unwrap();
                samples.push(started.elapsed().as_secs_f64() * 1000.0);
                buffers = result.rows.capacity() * size_of::<ComparedRow>()
                    + result.changed_columns.capacity() * size_of::<u64>();
                changed = result.counts.changed;
                black_box(result);
            }
            samples.sort_by(f64::total_cmp);
            let mut clone_samples = Vec::new();
            for _ in 0..7 {
                let started = Instant::now();
                for _ in 0..100_000 {
                    black_box((left.clone(), right.clone()));
                }
                clone_samples.push(started.elapsed().as_nanos() as f64 / 100_000.0);
            }
            clone_samples.sort_by(f64::total_cmp);
            println!(
                "compare rows={row_count} pairs={columns} blob={blob_size} blob_key={blob_key} changed={changed} input_text_blob_payload={payload_bytes}B: median={:.3}ms max={:.3}ms pair_arc_clone_drop_median={:.1}ns retained_index_bitmap_capacity={buffers}B (excludes snapshots and temporary maps)",
                samples[3], samples[6], clone_samples[3]
            );
        }
    }
}
