//! Aggregates for a snapshot of selected grid cells, independent of the UI.
//!
//! COUNT includes every supplied cell; COUNT_NUMS includes every typed integer,
//! decimal and float, including invalid/non-finite numeric values. NULL, DEFAULT,
//! text, bytes and booleans do not participate in numeric aggregates. Numeric
//! text is never guessed: callers can use `FilterValue::from_edited_value` for
//! pending edits whose original database type is known.
//!
//! Integer/decimal selections use checked decimal arithmetic. SUM never silently
//! discards precision; AVG and an even-sized MEDIAN may round within Decimal's
//! 96-bit mantissa and maximum scale of 28. A float makes all numeric aggregates
//! explicitly approximate. Any invalid/out-of-range decimal or non-finite input
//! makes every numeric aggregate unavailable, rather than dropping that cell.
//!
//! The coefficient of variation is the population standard deviation divided by
//! the absolute mean (a ratio, not a percentage). It is always approximate and
//! undefined for zero mean. Decimal conversion to f64 is used only for this metric
//! or when the selection already contains a float. Welford's variance is computed
//! on scaled values so squaring very large/small inputs does not overflow/underflow.
//!
//! Compute only when the selection or its data changes. Input is consumed once;
//! only numeric values are retained for linear-time median selection. No text or
//! binary cell contents are copied. Subsequent UI frames should reuse the result.

use crate::filter::FilterValue;
use rust_decimal::{Decimal, prelude::ToPrimitive};
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AggregateNumber {
    /// Decimal representation; AVG and even MEDIAN may round during division.
    Decimal(Decimal),
    Approximate(f64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggregateError {
    InvalidDecimal,
    DecimalOutOfRange,
    PrecisionLoss,
    Overflow,
    NonFinite,
    Undefined,
}

impl fmt::Display for AggregateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidDecimal => "Invalid decimal value",
            Self::DecimalOutOfRange => "Decimal exceeds supported precision or range",
            Self::PrecisionLoss => "Result cannot be represented without losing precision",
            Self::Overflow => "Numeric overflow",
            Self::NonFinite => "Selection contains NaN or infinity",
            Self::Undefined => "Undefined for a zero mean",
        })
    }
}

impl std::error::Error for AggregateError {}

/// `Ok(None)` means there are no numeric cells; an error means numeric cells
/// exist but that particular result must not be displayed as a valid number.
pub type AggregateResult = Result<Option<AggregateNumber>, AggregateError>;

#[derive(Clone, Debug, PartialEq)]
pub struct AggregateSummary {
    pub rows: usize,
    pub cols: usize,
    pub count: usize,
    pub count_nums: usize,
    pub sum: AggregateResult,
    pub avg: AggregateResult,
    pub min: AggregateResult,
    pub max: AggregateResult,
    pub median: AggregateResult,
    pub coefficient_of_variation: AggregateResult,
}

impl Default for AggregateSummary {
    fn default() -> Self {
        Self {
            rows: 0,
            cols: 0,
            count: 0,
            count_nums: 0,
            sum: Ok(None),
            avg: Ok(None),
            min: Ok(None),
            max: Ok(None),
            median: Ok(None),
            coefficient_of_variation: Ok(None),
        }
    }
}

impl AggregateSummary {
    fn set_error(&mut self, error: AggregateError) {
        self.sum = Err(error);
        self.avg = Err(error);
        self.min = Err(error);
        self.max = Err(error);
        self.median = Err(error);
        self.coefficient_of_variation = Err(error);
    }
}

/// Aggregate the supplied selection. `rows` and `cols` are the caller's distinct
/// selected row/column counts, not the dimensions of the complete result set.
pub fn aggregate<'a>(
    values: impl IntoIterator<Item = FilterValue<'a>>,
    rows: usize,
    cols: usize,
) -> AggregateSummary {
    let mut result = AggregateSummary {
        rows,
        cols,
        ..Default::default()
    };
    let mut numbers = Numbers::Decimal(Vec::new());
    let mut error = None;
    for value in values {
        result.count += 1;
        let numeric = matches!(
            value,
            FilterValue::Int(_) | FilterValue::Decimal(_) | FilterValue::Float(_)
        );
        if !numeric {
            continue;
        }
        result.count_nums += 1;
        if error.is_some() {
            continue;
        }
        let added = match value {
            FilterValue::Int(value) => numbers.push_decimal(Decimal::from(value)),
            FilterValue::Decimal(value) => {
                parse_decimal(&value).and_then(|value| numbers.push_decimal(value))
            }
            FilterValue::Float(bits) => numbers.push_float(f64::from_bits(bits)),
            _ => unreachable!(),
        };
        error = added.err();
    }
    if let Some(error) = error {
        result.set_error(error);
    } else if result.count_nums != 0 {
        match numbers {
            Numbers::Decimal(values) => decimal_results(values, &mut result),
            Numbers::Float(values) => float_results(values, &mut result),
        }
    }
    result
}

enum Numbers {
    Decimal(Vec<Decimal>),
    Float(Vec<f64>),
}

impl Numbers {
    fn push_decimal(&mut self, value: Decimal) -> Result<(), AggregateError> {
        match self {
            Self::Decimal(values) => values.push(value),
            Self::Float(values) => values.push(to_float(value)?),
        }
        Ok(())
    }

    fn push_float(&mut self, value: f64) -> Result<(), AggregateError> {
        if !value.is_finite() {
            return Err(AggregateError::NonFinite);
        }
        match self {
            Self::Decimal(values) => {
                let mut floats = Vec::with_capacity(values.len() + 1);
                for value in values.iter().copied() {
                    floats.push(to_float(value)?);
                }
                floats.push(value);
                *self = Self::Float(floats);
            }
            Self::Float(values) => values.push(value),
        }
        Ok(())
    }
}

fn to_float(value: Decimal) -> Result<f64, AggregateError> {
    value
        .to_f64()
        .filter(|value| value.is_finite())
        .ok_or(AggregateError::Overflow)
}

/// Parse scientific notation without the rounding performed by Decimal's
/// ordinary FromStr/from_scientific implementations on an oversized mantissa.
fn parse_decimal(text: &str) -> Result<Decimal, AggregateError> {
    let text = text.trim();
    let (base, exponent) = match text.split_once(['e', 'E']) {
        Some((base, exponent)) => {
            let digits = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
            if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(AggregateError::InvalidDecimal);
            }
            let exponent: i32 = exponent
                .parse()
                .map_err(|_| AggregateError::DecimalOutOfRange)?;
            (base, exponent)
        }
        None => (text, 0),
    };
    let unsigned = base.strip_prefix(['+', '-']).unwrap_or(base);
    let mut digits = 0;
    let mut dots = 0;
    for byte in unsigned.bytes() {
        match byte {
            b'0'..=b'9' => digits += 1,
            b'.' => dots += 1,
            _ => return Err(AggregateError::InvalidDecimal),
        }
    }
    if digits == 0 || dots > 1 {
        return Err(AggregateError::InvalidDecimal);
    }
    let value = Decimal::from_str_exact(base)
        .map_err(|_| AggregateError::DecimalOutOfRange)?
        .normalize();
    if value.is_zero() {
        return Ok(Decimal::ZERO);
    }
    let scale = i64::from(value.scale()) - i64::from(exponent);
    if scale > i64::from(Decimal::MAX_SCALE) || scale < -28 {
        return Err(AggregateError::DecimalOutOfRange);
    }
    let (mantissa, scale) = if scale < 0 {
        (
            value
                .mantissa()
                .checked_mul(10_i128.pow((-scale) as u32))
                .ok_or(AggregateError::DecimalOutOfRange)?,
            0,
        )
    } else {
        (value.mantissa(), scale as u32)
    };
    Decimal::try_from_i128_with_scale(mantissa, scale)
        .map_err(|_| AggregateError::DecimalOutOfRange)
}

/// checked_add detects overflow, but can still round to fit a 96-bit mantissa.
/// Compare aligned integer mantissas as well so SUM is exact or unavailable.
fn exact_add(left: Decimal, right: Decimal) -> Result<Decimal, AggregateError> {
    let sum = left.checked_add(right).ok_or(AggregateError::Overflow)?;
    let left = left.normalize();
    let right = right.normalize();
    let scale = left.scale().max(right.scale());
    let expected = aligned_mantissa(left, scale)
        .and_then(|left| aligned_mantissa(right, scale).and_then(|right| left.checked_add(right)))
        .ok_or(AggregateError::PrecisionLoss)?;
    let sum = sum.normalize();
    if sum.scale() > scale || aligned_mantissa(sum, scale) != Some(expected) {
        return Err(AggregateError::PrecisionLoss);
    }
    Ok(sum)
}

fn aligned_mantissa(value: Decimal, scale: u32) -> Option<i128> {
    value
        .mantissa()
        .checked_mul(10_i128.pow(scale - value.scale()))
}

fn decimal_result(value: Result<Decimal, AggregateError>) -> AggregateResult {
    value.map(|value| Some(AggregateNumber::Decimal(value.normalize())))
}

fn float_result(value: f64) -> AggregateResult {
    if value.is_finite() {
        Ok(Some(AggregateNumber::Approximate(if value == 0.0 {
            0.0
        } else {
            value
        })))
    } else {
        Err(AggregateError::Overflow)
    }
}

fn decimal_results(mut values: Vec<Decimal>, result: &mut AggregateSummary) {
    let mut sum = Ok(Decimal::ZERO);
    let mut min = values[0];
    let mut max = min;
    let mut variance = Variance::default();
    for &value in &values {
        sum = sum.and_then(|sum| exact_add(sum, value));
        min = min.min(value);
        max = max.max(value);
        // Decimal's range is contained in f64's finite range. This conversion
        // affects only the explicitly approximate coefficient of variation.
        variance.push(to_float(value).expect("Decimal fits finite f64"));
    }
    result.sum = decimal_result(sum);
    result.avg = decimal_result(sum.and_then(|sum| {
        sum.checked_div(Decimal::from(values.len() as u64))
            .ok_or(AggregateError::Overflow)
    }));
    result.min = decimal_result(Ok(min));
    result.max = decimal_result(Ok(max));
    let normalized_mean = sum
        .ok()
        .and_then(|sum| to_float(sum).ok())
        .map_or(variance.mean, |sum| variance.mean_from_sum(sum));
    result.coefficient_of_variation = variance.coefficient(normalized_mean);

    let middle = values.len() / 2;
    let even = values.len().is_multiple_of(2);
    let (lower, upper, _) = values.select_nth_unstable(middle);
    let median = if even {
        let lower = *lower.iter().max().expect("even nonempty sample");
        decimal_midpoint(lower, *upper)
    } else {
        Ok(*upper)
    };
    result.median = decimal_result(median);
}

fn decimal_midpoint(lower: Decimal, upper: Decimal) -> Result<Decimal, AggregateError> {
    if lower == upper {
        return Ok(lower);
    }
    if let Ok(sum) = exact_add(lower, upper) {
        return sum
            .checked_div(Decimal::from(2))
            .ok_or(AggregateError::Overflow);
    }

    // The sum can exceed Decimal even though its midpoint fits. Use a wider
    // integer numerator and round only once, avoiding lower + (upper-lower)/2
    // which double-rounds half units at the limit of Decimal's scale/range.
    let lower = lower.normalize();
    let upper = upper.normalize();
    let mut scale = lower.scale().max(upper.scale());
    let numerator = aligned_mantissa(lower, scale)
        .and_then(|lower| aligned_mantissa(upper, scale).and_then(|upper| lower.checked_add(upper)))
        .ok_or(AggregateError::PrecisionLoss)?;
    let negative = numerator < 0;
    let magnitude = numerator.unsigned_abs();
    let mut divisor = 2_u128;
    loop {
        let quotient = magnitude / divisor;
        let remainder = magnitude % divisor;
        let round_up = remainder > divisor / 2 || (remainder == divisor / 2 && quotient % 2 == 1);
        let rounded = quotient + u128::from(round_up);
        if rounded <= Decimal::MAX.mantissa() as u128 {
            let mantissa = rounded as i128 * if negative { -1 } else { 1 };
            return Decimal::try_from_i128_with_scale(mantissa, scale)
                .map_err(|_| AggregateError::Overflow);
        }
        if scale == 0 {
            return Err(AggregateError::Overflow);
        }
        scale -= 1;
        divisor *= 10;
    }
}

fn float_results(mut values: Vec<f64>, result: &mut AggregateSummary) {
    let mut sum: f64 = 0.0;
    let mut correction: f64 = 0.0;
    let mut min = values[0];
    let mut max = min;
    let mut variance = Variance::default();
    for &value in &values {
        let next = sum + value;
        correction += if sum.abs() >= value.abs() {
            (sum - next) + value
        } else {
            (value - next) + sum
        };
        sum = next;
        min = min.min(value);
        max = max.max(value);
        variance.push(value);
    }
    let sum = sum + correction;
    let normalized_mean = if sum.is_finite() {
        variance.mean_from_sum(sum)
    } else {
        variance.mean
    };
    result.sum = float_result(sum);
    result.avg = float_result(normalized_mean * variance.scale);
    result.min = float_result(min);
    result.max = float_result(max);
    result.coefficient_of_variation = variance.coefficient(normalized_mean);

    let middle = values.len() / 2;
    let even = values.len().is_multiple_of(2);
    let (lower, upper, _) = values.select_nth_unstable_by(middle, f64::total_cmp);
    let median = if even {
        let lower = *lower.iter().max_by(|a, b| a.total_cmp(b)).unwrap();
        let sum = lower + *upper;
        if sum.is_finite() {
            sum / 2.0
        } else {
            // Only large same-sign values can overflow their sum; halving
            // them first is safe. Always halving first loses subnormal bits.
            lower / 2.0 + *upper / 2.0
        }
    } else {
        *upper
    };
    result.median = float_result(median);
}

#[derive(Default)]
struct Variance {
    count: usize,
    /// Mean and squared deviations use values divided by this maximum magnitude.
    scale: f64,
    mean: f64,
    squared_deviations: f64,
}

impl Variance {
    fn push(&mut self, value: f64) {
        let magnitude = value.abs();
        if magnitude > self.scale {
            let rescale = self.scale / magnitude;
            self.mean *= rescale;
            self.squared_deviations *= rescale * rescale;
            self.scale = magnitude;
        }
        self.count += 1;
        if self.scale == 0.0 {
            return;
        }
        let value = value / self.scale;
        let delta = value - self.mean;
        let weight = 1.0 / self.count as f64;
        self.mean += delta * weight;
        self.squared_deviations += delta * (value - self.mean);
    }

    fn mean_from_sum(&self, sum: f64) -> f64 {
        if self.scale == 0.0 {
            0.0
        } else {
            // Divide by the scale before the count. A mean smaller than the
            // minimum subnormal float can still have a well-defined finite CV.
            sum / self.scale / self.count as f64
        }
    }

    fn coefficient(&self, mean: f64) -> AggregateResult {
        if mean == 0.0 {
            return Err(AggregateError::Undefined);
        }
        let variance = self.squared_deviations / self.count as f64;
        // A negative variance indicates numerical failure, never a real result.
        if !variance.is_finite() || variance < 0.0 {
            return Err(AggregateError::Overflow);
        }
        float_result(variance.sqrt() / mean.abs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{model::Value, sqlgen::CellEdit};
    use std::{borrow::Cow, hint::black_box, time::Instant};

    fn decimal(text: &str) -> FilterValue<'_> {
        FilterValue::Decimal(Cow::Borrowed(text))
    }

    fn number(text: &str) -> AggregateResult {
        Ok(Some(AggregateNumber::Decimal(parse_decimal(text).unwrap())))
    }

    fn approximate(result: AggregateResult) -> f64 {
        match result.unwrap().unwrap() {
            AggregateNumber::Approximate(value) => value,
            other => panic!("expected approximate, got {other:?}"),
        }
    }

    #[test]
    fn mixed_cells_count_every_cell_but_only_typed_numbers_contribute() {
        let result = aggregate(
            [
                FilterValue::Null,
                FilterValue::Default,
                FilterValue::Bool(true),
                FilterValue::Text(Cow::Borrowed("100")),
                FilterValue::Text(Cow::Borrowed("")),
                FilterValue::Bytes(Cow::Borrowed(&[1, 2])),
                FilterValue::Int(2),
                decimal("4.50"),
            ],
            4,
            2,
        );
        assert_eq!(
            (result.rows, result.cols, result.count, result.count_nums),
            (4, 2, 8, 2)
        );
        assert_eq!(result.sum, number("6.5"));
        assert_eq!(result.avg, number("3.25"));
        assert_eq!(result.median, number("3.25"));
        assert_eq!(result.min, number("2"));
        assert_eq!(result.max, number("4.5"));
    }

    #[test]
    fn integers_above_f64_precision_and_exact_decimals_keep_their_digits() {
        let result = aggregate(
            [FilterValue::Int(9_007_199_254_740_993), decimal("0.125")],
            2,
            1,
        );
        assert_eq!(result.sum, number("9007199254740993.125"));
        assert_eq!(result.max, number("9007199254740993"));
        let result = aggregate(
            [FilterValue::Int(i64::MAX), FilterValue::Int(i64::MAX)],
            2,
            1,
        );
        assert_eq!(result.sum, number("18446744073709551614"));
        assert_eq!(result.avg, number("9223372036854775807"));
        assert_eq!(result.median, result.avg);
    }

    #[test]
    fn average_division_is_decimal_and_may_round() {
        let result = aggregate(
            [
                FilterValue::Int(1),
                FilterValue::Int(0),
                FilterValue::Int(0),
            ],
            3,
            1,
        );
        assert_eq!(result.avg, number("0.3333333333333333333333333333"));
        assert_eq!(result.median, number("0"));
    }

    #[test]
    fn decimal_median_rounds_half_units_once_and_avoids_sum_overflow() {
        for (lower, upper, expected) in [
            ("1e-28", "2e-28", "2e-28"),
            ("2e-28", "3e-28", "2e-28"),
            ("-2e-28", "-1e-28", "-2e-28"),
            (
                "0.9",
                "79228162514264337593543950335",
                "39614081257132168796771975168",
            ),
            (
                "79228162514264337593543950333",
                "79228162514264337593543950334",
                "79228162514264337593543950334",
            ),
        ] {
            let result = aggregate([decimal(lower), decimal(upper)], 2, 1);
            assert_eq!(result.median, number(expected), "{lower}, {upper}");
        }
    }

    #[test]
    fn decimal_input_overflow_precision_loss_and_sum_overflow_are_visible() {
        for text in [
            "1e29",
            "1e-29",
            "0.12345678901234567890123456789",
            "999999999999999999999999999999999",
        ] {
            let result = aggregate([decimal(text), FilterValue::Int(1)], 2, 1);
            assert_eq!(result.count_nums, 2);
            assert_eq!(result.sum, Err(AggregateError::DecimalOutOfRange), "{text}");
            assert_eq!(result.min, result.sum);
        }
        let result = aggregate(
            [
                decimal("79228162514264337593543950335"),
                FilterValue::Int(1),
            ],
            2,
            1,
        );
        assert_eq!(result.sum, Err(AggregateError::Overflow));
        assert_eq!(result.min, number("1"));
        let result = aggregate(
            [decimal("79228162514264337593543950335"), decimal("0.1")],
            2,
            1,
        );
        assert_eq!(result.sum, Err(AggregateError::PrecisionLoss));
        let result = aggregate(
            [
                decimal("100000000000000000000"),
                decimal("0.00000000000000000001"),
            ],
            2,
            1,
        );
        assert_eq!(result.sum, Err(AggregateError::PrecisionLoss));
        assert_eq!(
            aggregate([decimal("oops")], 1, 1).sum,
            Err(AggregateError::InvalidDecimal)
        );
    }

    #[test]
    fn scientific_notation_is_exact_or_rejected() {
        for (input, expected) in [
            ("1.25e2", "125"),
            ("-12.5E-2", "-0.125"),
            ("1e+28", "10000000000000000000000000000"),
        ] {
            assert_eq!(aggregate([decimal(input)], 1, 1).sum, number(expected));
        }
        assert_eq!(
            aggregate([decimal("0.12345678901234567890123456789e2")], 1, 1).sum,
            Err(AggregateError::DecimalOutOfRange)
        );
    }

    #[test]
    fn floats_make_mixed_results_explicitly_approximate() {
        let result = aggregate(
            [
                decimal("0.1"),
                FilterValue::Float(0.2_f64.to_bits()),
                FilterValue::Int(1),
            ],
            3,
            1,
        );
        assert!((approximate(result.sum) - 1.3).abs() < 1e-14);
        assert!((approximate(result.avg) - 1.3 / 3.0).abs() < 1e-14);
        assert_eq!(approximate(result.min), 0.1);
        assert_eq!(approximate(result.max), 1.0);
        assert_eq!(approximate(result.median), 0.2);
        let result = aggregate(
            [1e16, 1.0, -1e16].map(|value: f64| FilterValue::Float(value.to_bits())),
            3,
            1,
        );
        assert_eq!(approximate(result.sum), 1.0);
    }

    #[test]
    fn nonfinite_numbers_are_counted_and_invalidate_numeric_results() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let result = aggregate(
                [
                    FilterValue::Float(value.to_bits()),
                    FilterValue::Int(2),
                    FilterValue::Null,
                ],
                3,
                1,
            );
            assert_eq!((result.count, result.count_nums), (3, 2));
            for metric in [
                result.sum,
                result.avg,
                result.min,
                result.max,
                result.median,
                result.coefficient_of_variation,
            ] {
                assert_eq!(metric, Err(AggregateError::NonFinite));
            }
        }
    }

    #[test]
    fn empty_numeric_selection_has_no_fabricated_zero() {
        for result in [
            aggregate([], 0, 0),
            aggregate([FilterValue::Null, FilterValue::Default], 2, 1),
        ] {
            assert_eq!(result.count_nums, 0);
            assert_eq!(result.sum, Ok(None));
            assert_eq!(result.avg, Ok(None));
            assert_eq!(result.min, Ok(None));
            assert_eq!(result.max, Ok(None));
            assert_eq!(result.median, Ok(None));
            assert_eq!(result.coefficient_of_variation, Ok(None));
        }
    }

    #[test]
    fn variance_is_population_ratio_and_handles_zero_mean_and_large_values() {
        let result = aggregate([FilterValue::Int(2), FilterValue::Int(4)], 2, 1);
        assert!((approximate(result.coefficient_of_variation) - 1.0 / 3.0).abs() < 1e-14);
        assert_eq!(
            aggregate([FilterValue::Int(-2), FilterValue::Int(2)], 2, 1).coefficient_of_variation,
            Err(AggregateError::Undefined)
        );
        let result = aggregate(
            [1e308, 1e308].map(|value: f64| FilterValue::Float(value.to_bits())),
            2,
            1,
        );
        assert_eq!(result.sum, Err(AggregateError::Overflow));
        assert_eq!(approximate(result.avg), 1e308);
        assert_eq!(approximate(result.median), 1e308);
    }

    #[test]
    fn scaled_variance_preserves_cv_at_extreme_and_subnormal_magnitudes() {
        for values in [
            [2e-308, 4e-308],
            [5e307, 1e308],
            [-2e-308, -4e-308],
            [-5e307, -1e308],
        ] {
            let result = aggregate(
                values.map(|value: f64| FilterValue::Float(value.to_bits())),
                2,
                1,
            );
            assert!(
                (approximate(result.coefficient_of_variation) - 1.0 / 3.0).abs() < 1e-14,
                "{values:?}"
            );
        }
        // The actual mean rounds to zero in f64, but CV remains exactly one.
        let result = aggregate(
            [0.0, f64::from_bits(1)].map(|value: f64| FilterValue::Float(value.to_bits())),
            2,
            1,
        );
        assert_eq!(approximate(result.avg), 0.0);
        assert_eq!(approximate(result.coefficient_of_variation), 1.0);
        let result = aggregate(
            [1e308, -1e308].map(|value: f64| FilterValue::Float(value.to_bits())),
            2,
            1,
        );
        assert_eq!(approximate(result.avg), 0.0);
        assert_eq!(
            result.coefficient_of_variation,
            Err(AggregateError::Undefined)
        );
    }

    #[test]
    fn float_median_rounds_subnormal_half_units_once() {
        let unit = f64::from_bits(1);
        for (values, expected) in [
            ([unit, unit * 2.0], unit * 2.0),
            ([-unit, unit * 2.0], 0.0),
            ([-unit * 2.0, -unit], -unit * 2.0),
            ([f64::MAX, f64::MAX], f64::MAX),
        ] {
            let result = aggregate(
                values.map(|value: f64| FilterValue::Float(value.to_bits())),
                2,
                1,
            );
            assert_eq!(approximate(result.median), expected, "{values:?}");
        }
    }

    #[test]
    fn pending_edits_only_acquire_numeric_type_from_the_original_value() {
        let edit = CellEdit::Text("42.25".to_owned());
        let original = Value::Decimal("1".to_owned());
        let result = aggregate([FilterValue::from_edited_value(&original, &edit)], 1, 1);
        assert_eq!(result.sum, number("42.25"));
        let result = aggregate([FilterValue::from_edit(&edit)], 1, 1);
        assert_eq!(result.count_nums, 0);
    }

    #[test]
    #[ignore = "manual release benchmark: cargo test --release aggregate::tests::benchmark -- --ignored --nocapture"]
    fn benchmark() {
        for rows in [500, 5_000, 100_000] {
            for (case, cols) in [
                ("integer", 1),
                ("decimal", 1),
                ("float", 1),
                ("text", 1),
                ("mixed", 6),
            ] {
                // Build the fetched-data fixture outside the measured section.
                // The timed path borrows cells just like a selected grid range.
                let values: Vec<Value> = (0..rows * cols)
                    .map(|index| {
                        let row = index / cols;
                        let value = (row * 17) % 1000;
                        let kind = match case {
                            "mixed" => index % cols,
                            "integer" => 0,
                            "decimal" => 1,
                            "float" => 2,
                            "text" => 3,
                            _ => unreachable!(),
                        };
                        match kind {
                            0 => Value::Int(value as i64),
                            1 => Value::Decimal(format!("{value}.125")),
                            2 => Value::Float(value as f64 + 0.125),
                            3 => Value::Text(format!("row {row}: żółć 😀")),
                            4 => Value::Null,
                            5 => Value::Bool(row % 2 == 0),
                            _ => unreachable!(),
                        }
                    })
                    .collect();
                let started = Instant::now();
                for _ in 0..20 {
                    let summary = aggregate(
                        black_box(&values).iter().map(FilterValue::from_value),
                        rows,
                        cols,
                    );
                    black_box(summary);
                }
                eprintln!(
                    "aggregate {case}: {rows} rows × {cols} columns, all 10 metrics: {:?}",
                    started.elapsed() / 20
                );
            }
        }
    }
}
