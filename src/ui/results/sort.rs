//! Sort keys for the fetched page, built once per sort instead of in every
//! comparison.

use crate::model::Value;
use std::borrow::Cow;
use std::cmp::Ordering;

/// What a value sorts by. NULL sorts first. Two numbers compare by value:
/// integers and decimals exactly, anything with a float as `f64`; NaN sorts
/// after every other number, like in PostgreSQL. Other pairs compare by their
/// displayed text, so text, booleans and mixed columns keep their order.
pub(super) struct SortKey<'a> {
    null: bool,
    text: Cow<'a, str>,
    exact: Option<Exact<'a>>,
    float: Option<f64>,
}

impl<'a> SortKey<'a> {
    pub fn of(value: &'a Value) -> Self {
        match value {
            Value::Null => Self {
                null: true,
                text: Cow::Borrowed(""),
                exact: None,
                float: None,
            },
            Value::Text(text) => Self {
                null: false,
                text: Cow::Borrowed(text),
                exact: None,
                float: None,
            },
            Value::Decimal(text) => Self {
                null: false,
                text: Cow::Borrowed(text),
                exact: Exact::parse(text),
                float: Some(text.parse().unwrap_or(f64::NAN)),
            },
            Value::Int(i) => Self {
                null: false,
                text: Cow::Owned(i.to_string()),
                exact: None,
                float: Some(*i as f64),
            },
            Value::Float(x) => Self {
                null: false,
                text: Cow::Owned(x.to_string()),
                exact: None,
                float: Some(*x),
            },
            other => Self {
                null: false,
                text: Cow::Owned(other.to_string()),
                exact: None,
                float: None,
            },
        }
    }

    fn int(value: &Value) -> Option<i64> {
        match value {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }
}

/// Compares two values of one column. `a_value`/`b_value` give integers their
/// exact comparison without storing a second key.
pub(super) fn compare(a: &SortKey<'_>, a_value: &Value, b: &SortKey<'_>, b_value: &Value) -> Ordering {
    match (a.null, b.null) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        _ => {}
    }
    if let (Some(x), Some(y)) = (SortKey::int(a_value), SortKey::int(b_value)) {
        return x.cmp(&y);
    }
    let (Some(fa), Some(fb)) = (a.float, b.float) else {
        return a.text.as_ref().cmp(b.text.as_ref());
    };
    // An integer's exact form is parsed from its short text, without allocating.
    let exact_a = match a_value {
        Value::Int(_) => Exact::parse(&a.text),
        _ => a.exact,
    };
    let exact_b = match b_value {
        Value::Int(_) => Exact::parse(&b.text),
        _ => b.exact,
    };
    if let (Some(x), Some(y)) = (exact_a, exact_b) {
        return x.cmp(&y);
    }
    match (fa.is_nan(), fb.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        _ => fa.partial_cmp(&fb).unwrap_or(Ordering::Equal),
    }
}

/// An exact decimal, borrowed from its text: `-0012.3400` is stored as a sign,
/// "12" and "34".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Exact<'a> {
    NegInfinity,
    Finite {
        negative: bool,
        int: &'a str,
        frac: &'a str,
    },
    Infinity,
    NaN,
}

impl<'a> Exact<'a> {
    fn parse(text: &'a str) -> Option<Self> {
        let text = text.trim();
        let (negative, digits) = match text.as_bytes().first()? {
            b'-' => (true, &text[1..]),
            b'+' => (false, &text[1..]),
            _ => (false, text),
        };
        if digits.eq_ignore_ascii_case("nan") {
            return (!negative).then_some(Exact::NaN);
        }
        if digits.eq_ignore_ascii_case("infinity") || digits.eq_ignore_ascii_case("inf") {
            return Some(if negative { Exact::NegInfinity } else { Exact::Infinity });
        }
        let (int, frac) = digits.split_once('.').unwrap_or((digits, ""));
        if int.is_empty() && frac.is_empty()
            || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit())
        {
            return None;
        }
        let int = int.trim_start_matches('0');
        let frac = frac.trim_end_matches('0');
        Some(Exact::Finite {
            negative: negative && !(int.is_empty() && frac.is_empty()),
            int,
            frac,
        })
    }

    fn rank(&self) -> u8 {
        match self {
            Exact::NegInfinity => 0,
            Exact::Finite { .. } => 1,
            Exact::Infinity => 2,
            Exact::NaN => 3,
        }
    }
}

impl Ord for Exact<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        let (
            Exact::Finite {
                negative: na,
                int: ia,
                frac: fa,
            },
            Exact::Finite {
                negative: nb,
                int: ib,
                frac: fb,
            },
        ) = (self, other)
        else {
            return self.rank().cmp(&other.rank());
        };
        if na != nb {
            return if *na { Ordering::Less } else { Ordering::Greater };
        }
        let magnitude = ia
            .len()
            .cmp(&ib.len())
            .then_with(|| ia.cmp(ib))
            .then_with(|| fa.cmp(fb));
        if *na { magnitude.reverse() } else { magnitude }
    }
}

impl PartialOrd for Exact<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(values: Vec<Value>) -> Vec<Value> {
        let keys: Vec<SortKey<'_>> = values.iter().map(SortKey::of).collect();
        let mut order: Vec<usize> = (0..values.len()).collect();
        order.sort_by(|&a, &b| compare(&keys[a], &values[a], &keys[b], &values[b]));
        order.into_iter().map(|i| values[i].clone()).collect()
    }

    fn dec(text: &str) -> Value {
        Value::Decimal(text.into())
    }

    #[test]
    fn decimals_compare_exactly_beyond_f64_precision() {
        let values = vec![
            dec("12345678901234567890.000000000000000002"),
            dec("12345678901234567890.000000000000000001"),
            dec("-0.10"),
            dec("-0.1000000000000000000001"),
            dec("0"),
            dec("-0"),
            dec("NaN"),
            dec("Infinity"),
            dec("-Infinity"),
            Value::Null,
            Value::Int(9_007_199_254_740_993),
            dec("9007199254740992.5"),
        ];
        assert_eq!(
            sorted(values),
            vec![
                Value::Null,
                dec("-Infinity"),
                dec("-0.1000000000000000000001"),
                dec("-0.10"),
                dec("0"),
                dec("-0"),
                dec("9007199254740992.5"),
                Value::Int(9_007_199_254_740_993),
                dec("12345678901234567890.000000000000000001"),
                dec("12345678901234567890.000000000000000002"),
                dec("Infinity"),
                dec("NaN"),
            ]
        );
    }

    #[test]
    fn floats_order_nan_last_and_keep_duplicates_stable() {
        let values = vec![
            Value::Float(f64::NAN),
            Value::Float(2.0),
            Value::Int(2),
            Value::Float(f64::NEG_INFINITY),
            Value::Float(f64::INFINITY),
            Value::Float(-1.5),
        ];
        let out = sorted(values);
        assert_eq!(out[0], Value::Float(f64::NEG_INFINITY));
        assert_eq!(out[1], Value::Float(-1.5));
        // Equal values keep their fetched order.
        assert_eq!(out[2], Value::Float(2.0));
        assert_eq!(out[3], Value::Int(2));
        assert_eq!(out[4], Value::Float(f64::INFINITY));
        assert!(matches!(out[5], Value::Float(x) if x.is_nan()));
    }

    #[test]
    fn text_and_mixed_columns_sort_by_displayed_text() {
        let values = vec![
            Value::Text("b".into()),
            Value::Int(10),
            Value::Text("Ä".into()),
            Value::Bool(false),
            Value::Text("a".into()),
            Value::Bytes(vec![1, 2]),
        ];
        assert_eq!(
            sorted(values),
            vec![
                Value::Int(10),
                Value::Bytes(vec![1, 2]),
                Value::Text("a".into()),
                Value::Text("b".into()),
                Value::Bool(false),
                Value::Text("Ä".into()),
            ]
        );
    }

    /// The comparator before sort keys, kept to measure the change.
    fn old_compare(a: &Value, b: &Value) -> Ordering {
        match (a, b) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Null, _) => Ordering::Less,
            (_, Value::Null) => Ordering::Greater,
            (Value::Int(x), Value::Int(y)) => x.cmp(y),
            _ if a.is_numeric() && b.is_numeric() => {
                let num = |v: &Value| v.to_string().parse::<f64>().unwrap_or(f64::NAN);
                num(a).partial_cmp(&num(b)).unwrap_or(Ordering::Equal)
            }
            _ => a.to_string().cmp(&b.to_string()),
        }
    }

    #[test]
    #[ignore = "benchmark: cargo test --release --lib sort_benchmark -- --ignored --nocapture"]
    fn sort_benchmark() {
        use std::time::Instant;
        let text: Vec<Value> = (0..5000u64)
            .map(|i| Value::Text(format!("{:x}{}", i.wrapping_mul(2654435761) % 100_000, "x".repeat(200))))
            .collect();
        let decimals: Vec<Value> = (0..5000u64)
            .map(|i| Value::Decimal(format!("{}.{:04}", i.wrapping_mul(2654435761) % 1_000_000, i % 10_000)))
            .collect();
        for (name, values) in [("text 200 B", &text), ("decimal", &decimals)] {
            let start = Instant::now();
            let mut order: Vec<usize> = (0..values.len()).collect();
            order.sort_by(|&a, &b| old_compare(&values[a], &values[b]));
            let old = start.elapsed();
            let start = Instant::now();
            let keys: Vec<SortKey<'_>> = values.iter().map(SortKey::of).collect();
            let mut new_order: Vec<usize> = (0..values.len()).collect();
            new_order.sort_by(|&a, &b| compare(&keys[a], &values[a], &keys[b], &values[b]));
            let new = start.elapsed();
            assert_eq!(order, new_order);
            println!("{name}, 5000 rows: comparator {old:?}, keys {new:?}");
        }
    }
}
