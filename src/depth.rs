//! Time model of Depth View: interprets result columns as points on one time
//! axis, without the UI. Everything here runs once per result version or
//! configuration change, never per frame.
//!
//! Instants (with a time zone) and wall-clock values (without one) are never
//! mixed on one axis, and no time zone is assumed for wall-clock values.

use crate::model::{QueryResult, Value};
use chrono::{DateTime, NaiveDate, NaiveDateTime};

/// At most three projections of a row, as in the research specification.
pub const MAX_SERIES: usize = 3;

/// What a position on the time axis means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Domain {
    /// An absolute moment (`timestamptz`, RFC 3339 with an offset), in UTC.
    Instant,
    /// Wall-clock date and time without a time zone (`timestamp`, `DATETIME`, `date`).
    Wall,
}

impl Domain {
    pub fn label(self) -> &'static str {
        match self {
            Self::Instant => "instants (UTC)",
            Self::Wall => "wall-clock time without a time zone",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Parsed {
    /// Microseconds since 1970-01-01 00:00 (UTC for instants, naive for wall time).
    pub micros: i64,
    pub domain: Domain,
    pub date_only: bool,
}

/// Exact parse of a textual date/time value. Numbers are never guessed as epochs.
pub fn parse_time(value: &Value) -> Option<Parsed> {
    let Value::Text(text) = value else {
        return None;
    };
    let text = text.trim();
    if text.len() < 10 || !text.as_bytes()[0].is_ascii_digit() {
        return None;
    }
    if let Ok(moment) = DateTime::parse_from_rfc3339(text) {
        return Some(Parsed {
            micros: moment.timestamp_micros(),
            domain: Domain::Instant,
            date_only: false,
        });
    }
    // PostgreSQL text output: "2026-10-15 10:00:00.5+02" (offset without minutes).
    for format in ["%Y-%m-%d %H:%M:%S%.f%#z", "%Y-%m-%dT%H:%M:%S%.f%#z"] {
        if let Ok(moment) = DateTime::parse_from_str(text, format) {
            return Some(Parsed {
                micros: moment.timestamp_micros(),
                domain: Domain::Instant,
                date_only: false,
            });
        }
    }
    for format in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M"] {
        if let Ok(wall) = NaiveDateTime::parse_from_str(text, format) {
            return Some(Parsed {
                micros: wall.and_utc().timestamp_micros(),
                domain: Domain::Wall,
                date_only: false,
            });
        }
    }
    let date = NaiveDate::parse_from_str(text, "%Y-%m-%d").ok()?;
    Some(Parsed {
        micros: date.and_hms_opt(0, 0, 0)?.and_utc().timestamp_micros(),
        domain: Domain::Wall,
        date_only: true,
    })
}

/// A result column that can position rows in time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimeColumn {
    pub index: usize,
    pub domain: Domain,
    pub date_only: bool,
    /// Values that are present but cannot be read as a time.
    pub invalid: usize,
}

fn temporal_type(type_name: &str) -> bool {
    let name = type_name.to_ascii_lowercase();
    name.contains("timestamp") || name.contains("date") || name == "time"
}

/// Cheap test for showing Depth View: a declared date type, or an undeclared
/// column whose first non-null values parse. [`time_columns`] decides exactly.
/// Each column looks at a bounded number of rows, so columns that are mostly
/// NULL do not make the check scan the whole page.
pub fn may_have_time_columns(data: &QueryResult) -> bool {
    const SAMPLE: usize = 32;
    const SAMPLE_ROWS: usize = 256;
    data.columns.iter().enumerate().any(|(index, column)| {
        if column.type_name.eq_ignore_ascii_case("time") {
            return false;
        }
        if temporal_type(&column.type_name) {
            return true;
        }
        let mut sample = data
            .rows
            .iter()
            .take(SAMPLE_ROWS)
            .filter_map(|row| row.get(index))
            .filter(|value| !matches!(value, Value::Null))
            .take(SAMPLE)
            .peekable();
        sample.peek().is_some() && sample.all(|value| parse_time(value).is_some())
    })
}

/// Columns whose non-null values all share one domain. Declared temporal types
/// may contain unreadable values (reported as invalid); undeclared columns
/// qualify only when every non-null value parses. One pass over the page.
pub fn time_columns(data: &QueryResult) -> Vec<TimeColumn> {
    time_columns_until(data, &|| false).unwrap_or_default()
}

/// Rows parsed between checks of a cancellation flag.
const CHECK_ROWS: usize = 1024;

/// [`time_columns`] that gives up with `None` once `cancelled` returns true.
pub fn time_columns_until(data: &QueryResult, cancelled: &dyn Fn() -> bool) -> Option<Vec<TimeColumn>> {
    let mut out = Vec::new();
    for (index, column) in data.columns.iter().enumerate() {
        if column.type_name.eq_ignore_ascii_case("time") {
            continue; // a time of day is not a point on a date axis
        }
        let declared = temporal_type(&column.type_name);
        let mut domain = None;
        let mut date_only = true;
        let mut invalid = 0;
        let mut ok = true;
        for (number, row) in data.rows.iter().enumerate() {
            if number % CHECK_ROWS == 0 && cancelled() {
                return None;
            }
            match row.get(index) {
                None | Some(Value::Null) => {}
                Some(value) => match parse_time(value) {
                    Some(parsed) if domain.is_none_or(|d| d == parsed.domain) => {
                        domain = Some(parsed.domain);
                        date_only &= parsed.date_only;
                    }
                    _ if declared => invalid += 1,
                    _ => {
                        ok = false;
                        break;
                    }
                },
            }
        }
        if ok && let Some(domain) = domain {
            out.push(TimeColumn {
                index,
                domain,
                date_only,
                invalid,
            });
        }
    }
    Some(out)
}

/// No value: below any representable instant, which starts at year -262143.
const MISSING: i64 = i64::MIN;

/// Parsed times of the chosen series for every row of one result version.
#[derive(Clone, Debug)]
pub struct DepthModel {
    pub series: Vec<usize>,
    pub domain: Domain,
    /// Some series are dates without a time of day while others are precise.
    pub mixed_precision: bool,
    /// Row-major times; [`MISSING`] marks no value, half the size of `Option<i64>`.
    times: Vec<i64>,
    pub min: i64,
    pub max: i64,
    /// Per series: rows without a usable value (NULL or unreadable).
    pub missing: Vec<usize>,
}

impl DepthModel {
    /// `candidates` are the result's [`time_columns`], computed once per result.
    pub fn build(data: &QueryResult, series: &[usize], candidates: &[TimeColumn]) -> Result<Self, String> {
        Self::build_until(data, series, candidates, &|| false).unwrap_or_else(|| Err("Cancelled.".into()))
    }

    /// [`DepthModel::build`] that gives up with `None` once `cancelled` returns true.
    pub fn build_until(
        data: &QueryResult,
        series: &[usize],
        candidates: &[TimeColumn],
        cancelled: &dyn Fn() -> bool,
    ) -> Option<Result<Self, String>> {
        if series.is_empty() || series.len() > MAX_SERIES {
            return Some(Err(format!("Choose one to {MAX_SERIES} time columns.")));
        }
        let mut domain = None;
        let mut precisions = Vec::new();
        for &column in series {
            let Some(candidate) = candidates.iter().find(|candidate| candidate.index == column) else {
                return Some(Err("A chosen column is not a date or time column.".into()));
            };
            if domain.is_some_and(|d| d != candidate.domain) {
                return Some(Err("Columns with and without a time zone cannot share one time axis. Choose columns of the same kind.".into()));
            }
            domain = Some(candidate.domain);
            precisions.push(candidate.date_only);
        }
        let width = series.len();
        let mut times = Vec::with_capacity(data.rows.len() * width);
        let mut missing = vec![0; width];
        let (mut min, mut max) = (i64::MAX, i64::MIN);
        for (number, row) in data.rows.iter().enumerate() {
            if number % CHECK_ROWS == 0 && cancelled() {
                return None;
            }
            for (slot, &column) in series.iter().enumerate() {
                let time = row.get(column).and_then(parse_time).map(|p| p.micros);
                match time {
                    Some(micros) => {
                        min = min.min(micros);
                        max = max.max(micros);
                    }
                    None => missing[slot] += 1,
                }
                times.push(time.unwrap_or(MISSING));
            }
        }
        if min > max {
            return Some(Err("The chosen columns have no readable dates in this result.".into()));
        }
        Some(Ok(Self {
            series: series.to_vec(),
            domain: domain.unwrap_or(Domain::Wall),
            mixed_precision: precisions.iter().any(|d| *d) && precisions.iter().any(|d| !d),
            times,
            min,
            max,
            missing,
        }))
    }

    pub fn rows(&self) -> usize {
        self.times.len() / self.series.len()
    }

    pub fn time(&self, row: usize, series: usize) -> Option<i64> {
        self.times
            .get(row * self.series.len() + series)
            .copied()
            .filter(|t| *t != MISSING)
    }

    /// Default filter: at least one projection lies in `[start, end)`.
    pub fn visible(&self, row: usize, start: i64, end: i64) -> bool {
        (0..self.series.len()).any(|s| self.time(row, s).is_some_and(|t| t >= start && t < end))
    }

    /// Signed difference `series + 1` minus `series`, from exact values.
    pub fn delta(&self, row: usize, series: usize) -> Option<i64> {
        Some(self.time(row, series + 1)? - self.time(row, series)?)
    }
}

pub fn format_time(micros: i64, domain: Domain, date_only: bool) -> String {
    let Some(moment) = DateTime::from_timestamp_micros(micros) else {
        return "out of range".into();
    };
    let naive = moment.naive_utc();
    if date_only {
        return naive.format("%Y-%m-%d").to_string();
    }
    let text = if micros % 1_000_000 == 0 {
        naive.format("%Y-%m-%d %H:%M:%S").to_string()
    } else {
        naive.format("%Y-%m-%d %H:%M:%S%.6f").to_string()
    };
    match domain {
        Domain::Instant => format!("{text} UTC"),
        Domain::Wall => text,
    }
}

/// Signed, human scale: "+18 s", "−1 h 02 min", "+250 ms".
pub fn format_delta(micros: i64) -> String {
    let sign = if micros < 0 { "−" } else { "+" };
    let abs = micros.unsigned_abs();
    let (ms, s, m, h, d) = (
        abs / 1_000,
        abs / 1_000_000,
        abs / 60_000_000,
        abs / 3_600_000_000,
        abs / 86_400_000_000,
    );
    let body = if abs < 1_000 {
        format!("{abs} µs")
    } else if abs < 1_000_000 {
        format!("{ms} ms")
    } else if s < 60 {
        let tenths = (abs % 1_000_000) / 100_000;
        if tenths == 0 { format!("{s} s") } else { format!("{s}.{tenths} s") }
    } else if m < 60 {
        format!("{m} min {:02} s", s % 60)
    } else if h < 24 {
        format!("{h} h {:02} min", m % 60)
    } else {
        format!("{d} d {:02} h", h % 24)
    };
    format!("{sign}{body}")
}

/// Window shift per arrow press; the axis itself stays continuous.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Second,
    Minute,
    Hour,
    Day,
}

impl Step {
    pub const ALL: [Step; 4] = [Self::Second, Self::Minute, Self::Hour, Self::Day];
    pub fn micros(self) -> i64 {
        match self {
            Self::Second => 1_000_000,
            Self::Minute => 60_000_000,
            Self::Hour => 3_600_000_000,
            Self::Day => 86_400_000_000,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Second => "1 s",
            Self::Minute => "1 min",
            Self::Hour => "1 h",
            Self::Day => "1 day",
        }
    }
    /// A step that moves a window of this width in a few presses.
    pub fn for_span(span: i64) -> Self {
        *Self::ALL
            .iter()
            .rev()
            .find(|step| step.micros() * 4 <= span)
            .unwrap_or(&Self::Second)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Column;

    fn text(value: &str) -> Value {
        Value::Text(value.into())
    }

    fn result(columns: &[(&str, &str)], rows: Vec<Vec<Value>>) -> QueryResult {
        QueryResult {
            columns: columns
                .iter()
                .map(|(name, ty)| Column {
                    name: (*name).into(),
                    type_name: (*ty).into(),
                })
                .collect(),
            rows,
            ..Default::default()
        }
    }

    #[test]
    fn parses_driver_formats_exactly_without_guessing_zones_or_epochs() {
        let utc = parse_time(&text("2026-10-15T08:00:00+00:00")).unwrap();
        let offset = parse_time(&text("2026-10-15 10:00:00+02")).unwrap();
        assert_eq!(utc.micros, offset.micros);
        assert_eq!(utc.domain, Domain::Instant);
        let wall = parse_time(&text("2026-10-15 10:00:00.000001")).unwrap();
        assert_eq!(wall.domain, Domain::Wall);
        assert_eq!(wall.micros % 1_000_000, 1);
        assert!(parse_time(&text("2026-10-15")).unwrap().date_only);
        assert!(parse_time(&Value::Int(1_760_000_000)).is_none());
        assert!(parse_time(&text("not a date")).is_none());
        assert!(parse_time(&text("2026-02-30")).is_none());
    }

    #[test]
    fn time_columns_respect_types_domains_and_invalid_values() {
        let data = result(
            &[
                ("id", "int4"),
                ("at", "timestamptz"),
                ("local", "timestamp"),
                ("note", "text"),
                ("text_time", "text"),
                ("mixed", "text"),
            ],
            vec![
                vec![
                    Value::Int(1),
                    text("2026-10-15T08:00:00+00:00"),
                    text("2026-10-15 10:00:00"),
                    text("hello"),
                    text("2026-10-15 10:00:01"),
                    text("2026-10-15T08:00:00+00:00"),
                ],
                vec![
                    Value::Int(2),
                    text("garbage"),
                    Value::Null,
                    text("world"),
                    text("2026-10-15 10:00:02"),
                    text("2026-10-15 10:00:00"),
                ],
            ],
        );
        let columns = time_columns(&data);
        let indices: Vec<_> = columns.iter().map(|c| c.index).collect();
        assert_eq!(indices, [1, 2, 4], "mixed domains and plain text are excluded");
        assert_eq!(columns[0].invalid, 1);
        assert_eq!(columns[0].domain, Domain::Instant);
        assert_eq!(columns[1].domain, Domain::Wall);
    }

    #[test]
    fn model_keeps_one_axis_signed_deltas_and_any_projection_window() {
        let data = result(
            &[("occurred", "timestamp"), ("received", "timestamp"), ("processed", "timestamp")],
            vec![
                vec![text("2026-10-15 10:00:00"), text("2026-10-15 10:00:18"), text("2026-10-15 10:00:19")],
                vec![text("2026-10-15 10:05:00"), text("2026-10-15 10:04:50"), Value::Null],
            ],
        );
        let model = DepthModel::build(&data, &[0, 1, 2], &time_columns(&data)).unwrap();
        assert_eq!(model.delta(0, 0), Some(18_000_000));
        assert_eq!(model.delta(0, 1), Some(1_000_000));
        assert_eq!(model.delta(1, 0), Some(-10_000_000), "reversed times stay negative");
        assert_eq!(model.delta(1, 1), None, "a missing stage is not zero");
        assert_eq!(model.missing, [0, 0, 1]);
        let ten = model.time(0, 0).unwrap();
        assert!(model.visible(0, ten, ten + 1));
        assert!(model.visible(0, ten + 18_000_000, ten + 18_000_001), "a later projection qualifies");
        assert!(!model.visible(0, ten + 1, ten + 18_000_000), "window end is exclusive");
        assert_eq!(model.max - model.min, 300_000_000);
        let mixed = result(
            &[("a", "timestamptz"), ("b", "timestamp")],
            vec![vec![text("2026-10-15T08:00:00Z"), text("2026-10-15 10:00:00")]],
        );
        assert!(DepthModel::build(&mixed, &[0, 1], &time_columns(&mixed)).is_err());
    }

    #[test]
    fn deltas_and_steps_are_formatted_for_people() {
        assert_eq!(format_delta(18_000_000), "+18 s");
        assert_eq!(format_delta(-3_720_000_000), "−1 h 02 min");
        assert_eq!(format_delta(250_000), "+250 ms");
        assert_eq!(format_delta(1_500_000), "+1.5 s");
        assert_eq!(Step::for_span(10 * 60_000_000), Step::Minute);
        assert_eq!(format_time(0, Domain::Instant, false), "1970-01-01 00:00:00 UTC");
    }
}
