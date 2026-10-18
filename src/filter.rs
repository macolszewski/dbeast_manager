//! Exact typed keys for local value filters, independent of display formatting.
//!
//! Decimal spellings retain their precision and scale; they are never parsed as
//! floats. Floating-point positive/negative zero share a key, as do all NaNs.
//! Integers, floats, decimals, text, NULL, and DEFAULT remain distinct types.

use crate::{model::Value, sqlgen::CellEdit};
use std::{
    borrow::{Borrow, Cow},
    collections::HashSet,
    sync::Arc,
    fmt::Write,
    hash::{Hash, Hasher},
};

/// A borrowed key during a row scan, or an owned key retained by a filter.
#[derive(Clone, Debug)]
pub enum FilterValue<'a> {
    Null,
    Default,
    Bool(bool),
    Int(i64),
    Float(u64),
    Decimal(Cow<'a, str>),
    Text(Cow<'a, str>),
    Bytes(Cow<'a, [u8]>),
}

impl<'a> FilterValue<'a> {
    pub fn from_value(value: &'a Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Bool(value) => Self::Bool(*value),
            Value::Int(value) => Self::Int(*value),
            Value::Float(value) => Self::Float(canonical_float_bits(value.to_bits())),
            Value::Decimal(value) => Self::Decimal(Cow::Borrowed(value)),
            Value::Text(value) => Self::Text(Cow::Borrowed(value)),
            Value::Bytes(value) => Self::Bytes(Cow::Borrowed(value)),
        }
    }

    /// Pending text remains text: database-side coercion has not happened yet.
    pub fn from_edit(edit: &'a CellEdit) -> Self {
        match edit {
            CellEdit::Null => Self::Null,
            CellEdit::Default => Self::Default,
            CellEdit::Text(value) => Self::Text(Cow::Borrowed(value)),
        }
    }

    /// Preserve a fetched scalar's type when its pending text parses as that
    /// type. Invalid input stays text until the database validates it. This
    /// never guesses a type for originally textual, NULL, or binary values.
    pub fn from_edited_value(original: &Value, edit: &'a CellEdit) -> Self {
        let CellEdit::Text(text) = edit else {
            return Self::from_edit(edit);
        };
        let trimmed = text.trim();
        match original {
            Value::Int(_) => trimmed
                .parse()
                .map(Self::Int)
                .unwrap_or_else(|_| Self::from_edit(edit)),
            Value::Float(_) => trimmed
                .parse::<f64>()
                .map(|value| Self::Float(canonical_float_bits(value.to_bits())))
                .unwrap_or_else(|_| Self::from_edit(edit)),
            Value::Bool(_) if trimmed.eq_ignore_ascii_case("true") => Self::Bool(true),
            Value::Bool(_) if trimmed.eq_ignore_ascii_case("false") => Self::Bool(false),
            Value::Decimal(_) if is_decimal(trimmed) => Self::Decimal(Cow::Borrowed(trimmed)),
            _ => Self::from_edit(edit),
        }
    }

    pub fn into_owned(self) -> FilterValue<'static> {
        match self {
            Self::Null => FilterValue::Null,
            Self::Default => FilterValue::Default,
            Self::Bool(value) => FilterValue::Bool(value),
            Self::Int(value) => FilterValue::Int(value),
            Self::Float(bits) => FilterValue::Float(canonical_float_bits(bits)),
            Self::Decimal(value) => FilterValue::Decimal(Cow::Owned(value.into_owned())),
            Self::Text(value) => FilterValue::Text(Cow::Owned(value.into_owned())),
            Self::Bytes(value) => FilterValue::Bytes(Cow::Owned(value.into_owned())),
        }
    }

    /// A bounded popup label, not an identity or a SQL literal.
    ///
    /// Text is quoted and escaped to distinguish it from NULL, DEFAULT, and
    /// scalar values. Long text/decimals show at most 160 Unicode scalars; binary
    /// labels show a 32-byte prefix and length. Keys still compare full values.
    pub fn display_label(&self) -> String {
        match self {
            Self::Null => "<null>".into(),
            Self::Default => "<default>".into(),
            Self::Bool(value) => value.to_string(),
            Self::Int(value) => value.to_string(),
            Self::Float(bits) => {
                format!("{} (float)", f64::from_bits(canonical_float_bits(*bits)))
            }
            Self::Decimal(value) => format!("{} (decimal)", bounded_text(value, false)),
            Self::Text(value) => bounded_text(value, true),
            Self::Bytes(value) => {
                let mut label = String::with_capacity(90);
                label.push_str("0x");
                for byte in value.iter().take(32) {
                    write!(label, "{byte:02X}").expect("writing to a String cannot fail");
                }
                if value.len() > 32 {
                    label.push('…');
                }
                write!(label, " ({} bytes)", value.len()).expect("writing to a String cannot fail");
                label
            }
        }
    }

    pub fn as_borrowed(&self) -> FilterValue<'_> {
        match self {
            Self::Null => FilterValue::Null,
            Self::Default => FilterValue::Default,
            Self::Bool(value) => FilterValue::Bool(*value),
            Self::Int(value) => FilterValue::Int(*value),
            Self::Float(bits) => FilterValue::Float(*bits),
            Self::Decimal(value) => FilterValue::Decimal(Cow::Borrowed(value)),
            Self::Text(value) => FilterValue::Text(Cow::Borrowed(value)),
            Self::Bytes(value) => FilterValue::Bytes(Cow::Borrowed(value)),
        }
    }
}

impl PartialEq for FilterValue<'_> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Null, Self::Null) | (Self::Default, Self::Default) => true,
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (Self::Int(a), Self::Int(b)) => a == b,
            (Self::Float(a), Self::Float(b)) => {
                canonical_float_bits(*a) == canonical_float_bits(*b)
            }
            (Self::Decimal(a), Self::Decimal(b)) | (Self::Text(a), Self::Text(b)) => a == b,
            (Self::Bytes(a), Self::Bytes(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for FilterValue<'_> {}

impl Hash for FilterValue<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            Self::Null | Self::Default => {}
            Self::Bool(value) => value.hash(state),
            Self::Int(value) => value.hash(state),
            Self::Float(bits) => canonical_float_bits(*bits).hash(state),
            Self::Decimal(value) | Self::Text(value) => value.hash(state),
            Self::Bytes(value) => value.hash(state),
        }
    }
}

fn canonical_float_bits(bits: u64) -> u64 {
    let magnitude = bits & 0x7fff_ffff_ffff_ffff;
    if magnitude == 0 {
        0
    } else if magnitude > 0x7ff0_0000_0000_0000 {
        // One quiet NaN represents every sign and signaling/payload encoding.
        0x7ff8_0000_0000_0000
    } else {
        bits
    }
}

/// Check arbitrary-precision decimal syntax without rounding or allocating.
fn is_decimal(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut position = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let mut digits = 0;
    while bytes.get(position).is_some_and(u8::is_ascii_digit) {
        position += 1;
        digits += 1;
    }
    if bytes.get(position) == Some(&b'.') {
        position += 1;
        while bytes.get(position).is_some_and(u8::is_ascii_digit) {
            position += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return false;
    }
    if matches!(bytes.get(position), Some(b'e' | b'E')) {
        position += 1;
        if matches!(bytes.get(position), Some(b'+' | b'-')) {
            position += 1;
        }
        let exponent_start = position;
        while bytes.get(position).is_some_and(u8::is_ascii_digit) {
            position += 1;
        }
        if position == exponent_start {
            return false;
        }
    }
    position == bytes.len()
}

fn bounded_text(text: &str, quoted: bool) -> String {
    let mut label = String::with_capacity(text.len().min(160) + 2);
    if quoted {
        label.push('"');
    }
    let mut chars = text.chars();
    for value in chars.by_ref().take(160) {
        match value {
            '\n' => label.push_str("\\n"),
            '\r' => label.push_str("\\r"),
            '\t' => label.push_str("\\t"),
            '\\' if quoted => label.push_str("\\\\"),
            '"' if quoted => label.push_str("\\\""),
            value if value.is_control() => label.extend(value.escape_unicode()),
            value => label.push(value),
        }
    }
    if chars.next().is_some() {
        label.push('…');
    }
    if quoted {
        label.push('"');
    }
    label
}

/// A set of explicitly selected values. An empty set matches no values; callers
/// represent an absent column filter separately from an empty selection.
/// Values are shared, so a filter's value list can show retained values
/// without copying them again.
#[derive(Clone, Debug, Default)]
pub struct SelectedValues {
    values: HashSet<Shared>,
}

#[derive(Clone, Debug)]
struct Shared(Arc<FilterValue<'static>>);

impl PartialEq for Shared {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for Shared {}

impl Hash for Shared {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl SelectedValues {
    /// Own a selected value once. Reinserting a borrowed duplicate does not copy.
    pub fn insert(&mut self, value: FilterValue<'_>) -> bool {
        if self.contains(&value) {
            return false;
        }
        self.values.insert(Shared(Arc::new(value.into_owned())))
    }

    /// Adds a value another selection already owns, without copying it.
    pub fn insert_shared(&mut self, value: Arc<FilterValue<'static>>) -> bool {
        self.values.insert(Shared(value))
    }

    /// Borrowed lookup: no text/binary copying or numeric-to-text conversion.
    pub fn contains(&self, value: &FilterValue<'_>) -> bool {
        self.values.contains(value as &dyn BorrowedIdentity)
    }

    /// Remove by borrowed identity without copying even a large text/binary key.
    pub fn remove(&mut self, value: &FilterValue<'_>) -> bool {
        self.values.remove(value as &dyn BorrowedIdentity)
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn clear(&mut self) {
        self.values.clear();
    }

    pub fn iter(&self) -> impl Iterator<Item = &FilterValue<'static>> {
        self.values.iter().map(|value| value.0.as_ref())
    }

    pub fn iter_shared(&self) -> impl Iterator<Item = &Arc<FilterValue<'static>>> {
        self.values.iter().map(|value| &value.0)
    }
}

// Immutable HashSet lookup can shorten the Cow lifetime by covariance, but
// mutable lookup cannot. A borrowed identity trait supplies HashSet::remove's
// Borrow key without allocating a second owned copy or scanning the whole set.
trait BorrowedIdentity {
    fn key(&self) -> FilterValue<'_>;
}

impl BorrowedIdentity for FilterValue<'_> {
    fn key(&self) -> FilterValue<'_> {
        self.as_borrowed()
    }
}

impl PartialEq for dyn BorrowedIdentity + '_ {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl Eq for dyn BorrowedIdentity + '_ {}

impl Hash for dyn BorrowedIdentity + '_ {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.key().hash(state);
    }
}

impl<'a> Borrow<dyn BorrowedIdentity + 'a> for Shared {
    fn borrow(&self) -> &(dyn BorrowedIdentity + 'a) {
        self.0.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_default_empty_text_and_scalar_types_are_distinct() {
        let values = [
            FilterValue::Null,
            FilterValue::Default,
            FilterValue::Text(Cow::Borrowed("")),
            FilterValue::Text(Cow::Borrowed("<null>")),
            FilterValue::Text(Cow::Borrowed("<default>")),
            FilterValue::Bool(true),
            FilterValue::Text(Cow::Borrowed("true")),
            FilterValue::Int(1),
            FilterValue::Float(1.0_f64.to_bits()),
            FilterValue::Decimal(Cow::Borrowed("1")),
            FilterValue::Text(Cow::Borrowed("1")),
            FilterValue::Bytes(Cow::Borrowed(b"1")),
        ];
        let mut selected = SelectedValues::default();
        for value in &values {
            assert!(selected.insert(value.clone()));
            assert!(!selected.insert(value.clone()));
        }
        assert_eq!(selected.len(), values.len());
        for value in &values {
            assert!(selected.contains(value));
            assert!(selected.remove(value));
            assert!(!selected.contains(value));
        }
        assert!(selected.is_empty());
        assert!(!selected.remove(&FilterValue::Null));
    }

    #[test]
    fn integers_and_decimals_never_lose_precision() {
        let values = [
            Value::Int(9_007_199_254_740_992),
            Value::Int(9_007_199_254_740_993),
            Value::Int(i64::MAX),
            Value::Int(i64::MIN),
            Value::Decimal("99999999999999999999999999999.00000000000000000001".into()),
            Value::Decimal("99999999999999999999999999999.00000000000000000002".into()),
            Value::Decimal("1.0".into()),
            Value::Decimal("1.00".into()),
        ];
        let mut selected = SelectedValues::default();
        for value in &values {
            assert!(selected.insert(FilterValue::from_value(value)));
        }
        assert_eq!(selected.len(), values.len());
        assert!(!selected.contains(&FilterValue::Float((i64::MAX as f64).to_bits())));
    }

    #[test]
    fn float_zero_and_nan_identity_matches_hashing_for_all_encodings() {
        let mut selected = SelectedValues::default();
        assert!(selected.insert(FilterValue::Float(0.0_f64.to_bits())));
        assert!(!selected.insert(FilterValue::Float((-0.0_f64).to_bits())));
        assert!(selected.contains(&FilterValue::Float((-0.0_f64).to_bits())));
        assert!(selected.insert(FilterValue::Float(f64::NAN.to_bits())));
        for bits in [
            0x7ff0_0000_0000_0001,
            0x7ff8_0000_0000_1234,
            0xfff8_0000_0000_4321,
        ] {
            assert!(!selected.insert(FilterValue::Float(bits)));
            assert!(selected.contains(&FilterValue::Float(bits)));
        }
        assert!(selected.insert(FilterValue::from_value(&Value::Float(f64::INFINITY))));
        assert!(selected.insert(FilterValue::from_value(&Value::Float(f64::NEG_INFINITY))));
        assert_eq!(selected.len(), 4);
        assert!(selected.remove(&FilterValue::Float(0xfff0_0000_0000_0001)));
        assert!(!selected.contains(&FilterValue::Float(f64::NAN.to_bits())));
    }

    #[test]
    fn binary_keys_compare_the_full_payload_and_borrow_storage() {
        let mut first = vec![0xAB; 100_000];
        let mut second = first.clone();
        second[99_999] = 0xAC;
        let value = Value::Bytes(first.clone());
        let borrowed = FilterValue::from_value(&value);
        assert!(matches!(borrowed, FilterValue::Bytes(Cow::Borrowed(_))));
        let mut selected = SelectedValues::default();
        assert!(selected.insert(borrowed));
        assert!(!selected.contains(&FilterValue::Bytes(Cow::Borrowed(&second))));
        assert!(selected.insert(FilterValue::Bytes(Cow::Borrowed(&second))));
        first[0] = 0;
        assert!(!selected.contains(&FilterValue::Bytes(Cow::Borrowed(&first))));
        assert!(selected.remove(&FilterValue::Bytes(Cow::Borrowed(&second))));
        assert!(selected.contains(&FilterValue::from_value(&value)));
    }

    #[test]
    fn pending_values_preserve_types_and_owned_keys_outlive_the_source() {
        let mut selected = SelectedValues::default();
        {
            let text = Value::Text("Zażółć 😀".into());
            let edit = CellEdit::Text("1".into());
            assert!(selected.insert(FilterValue::from_value(&text)));
            assert!(selected.insert(FilterValue::from_edit(&edit)));
            assert!(selected.insert(FilterValue::from_edit(&CellEdit::Null)));
            assert!(selected.insert(FilterValue::from_edit(&CellEdit::Default)));
        }
        assert!(selected.contains(&FilterValue::Text(Cow::Borrowed("Zażółć 😀"))));
        assert!(selected.contains(&FilterValue::Text(Cow::Borrowed("1"))));
        assert!(!selected.contains(&FilterValue::Int(1)));
        assert_eq!(selected.iter().count(), 4);
        selected.clear();
        assert!(selected.is_empty());
    }

    #[test]
    fn pending_scalar_edits_keep_known_types_without_precision_loss() {
        let edit = CellEdit::Text("9007199254740993".into());
        assert_eq!(
            FilterValue::from_edited_value(&Value::Int(0), &edit),
            FilterValue::Int(9_007_199_254_740_993)
        );
        assert_eq!(
            FilterValue::from_edited_value(&Value::Text(String::new()), &edit),
            FilterValue::from_edit(&edit)
        );
        let overflow = CellEdit::Text("9223372036854775808".into());
        assert_eq!(
            FilterValue::from_edited_value(&Value::Int(0), &overflow),
            FilterValue::from_edit(&overflow)
        );
        let decimal = CellEdit::Text("999999999999999999999.00000000000000000001".into());
        assert_eq!(
            FilterValue::from_edited_value(&Value::Decimal("0".into()), &decimal),
            FilterValue::Decimal(Cow::Borrowed("999999999999999999999.00000000000000000001"))
        );
        for text in ["1", "-1.00", ".123", "+5.", "2.31e-99999999"] {
            let edit = CellEdit::Text(text.into());
            assert!(matches!(
                FilterValue::from_edited_value(&Value::Decimal("0".into()), &edit),
                FilterValue::Decimal(_)
            ));
        }
        for text in ["", ".", "+", "1e", "e1", "1,23", "1e+", " 1 2 ", "invalid"] {
            let edit = CellEdit::Text(text.into());
            assert_eq!(
                FilterValue::from_edited_value(&Value::Decimal("0".into()), &edit),
                FilterValue::from_edit(&edit)
            );
        }
        assert_eq!(
            FilterValue::from_edited_value(&Value::Float(1.0), &CellEdit::Text(" -0 ".into())),
            FilterValue::Float(0)
        );
        assert_eq!(
            FilterValue::from_edited_value(&Value::Bool(false), &CellEdit::Text("TRUE".into())),
            FilterValue::Bool(true)
        );
        assert_eq!(
            FilterValue::from_edited_value(&Value::Int(1), &CellEdit::Null),
            FilterValue::Null
        );
        assert_eq!(
            FilterValue::from_edited_value(&Value::Int(1), &CellEdit::Default),
            FilterValue::Default
        );
    }

    #[test]
    fn labels_distinguish_types_escape_text_and_bound_large_values() {
        assert_eq!(FilterValue::Null.display_label(), "<null>");
        assert_eq!(FilterValue::Default.display_label(), "<default>");
        assert_eq!(FilterValue::Text(Cow::Borrowed("")).display_label(), "\"\"");
        assert_eq!(
            FilterValue::Text(Cow::Borrowed("<null>")).display_label(),
            "\"<null>\""
        );
        assert_eq!(
            FilterValue::Text(Cow::Borrowed("ż\n\"\\")).display_label(),
            "\"ż\\n\\\"\\\\\""
        );
        let long = "😀".repeat(100_000);
        let label = FilterValue::Text(Cow::Borrowed(&long)).display_label();
        assert_eq!(label.chars().count(), 163);
        assert!(label.ends_with("…\""));
        let bytes = vec![0xFF; 100_000];
        let label = FilterValue::Bytes(Cow::Borrowed(&bytes)).display_label();
        assert!(label.ends_with("… (100000 bytes)"));
        assert!(label.len() < 100);
    }

    #[test]
    fn selected_values_filter_a_large_page_using_borrowed_keys() {
        let rows: Vec<Value> = (0..100_000)
            .map(|index| Value::Text(format!("row {index}")))
            .collect();
        let mut selected = SelectedValues::default();
        for index in [0, 500, 50_000, 99_999] {
            selected.insert(FilterValue::from_value(&rows[index]));
        }
        let found: Vec<_> = rows
            .iter()
            .enumerate()
            .filter(|(_, value)| selected.contains(&FilterValue::from_value(value)))
            .map(|(index, _)| index)
            .collect();
        assert_eq!(found, vec![0, 500, 50_000, 99_999]);
    }

    #[test]
    #[ignore = "release-mode value filter benchmark; run explicitly with --ignored --nocapture"]
    fn selected_values_benchmark() {
        use std::{hint::black_box, time::Instant};

        let mut selected = SelectedValues::default();
        for index in [1, 3, 5, 7, 9] {
            selected.insert(FilterValue::Text(Cow::Owned(format!(
                "category {index} / Żółć"
            ))));
        }
        for count in [500, 5_000, 100_000] {
            let rows: Vec<Value> = (0..count)
                .map(|index| Value::Text(format!("category {} / Żółć", index % 10)))
                .collect();
            let start = Instant::now();
            let iterations = 100;
            for _ in 0..iterations {
                let matched = rows
                    .iter()
                    .filter(|value| selected.contains(&FilterValue::from_value(black_box(value))))
                    .count();
                assert_eq!(black_box(matched), count / 2);
            }
            eprintln!(
                "typed value filter: {count} rows, {:?}/iteration",
                start.elapsed() / iterations
            );
        }
    }
}
