//! Database-column contracts shared by UI editors and mutation generation.
//! Parsed once from catalog metadata; values remain lossless until driver binding.
use crate::model::{DriverKind, Value};
use crate::sqlgen::CellEdit;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditorKind {
    Boolean,
    Enum(Vec<String>),
    Integer {
        min: i128,
        max: i128,
    },
    Decimal {
        precision: Option<u32>,
        scale: Option<i32>,
        unsigned: bool,
    },
    Float {
        single: bool,
        unsigned: bool,
    },
    Text {
        limit: Option<usize>,
    },
    Date,
    Time,
    Timestamp,
    Binary,
    Json,
    Uuid,
    /// User-defined, array and spatial syntax stays with the server.
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnSpec {
    pub kind: EditorKind,
    pub nullable: bool,
    pub type_name: String,
    driver: DriverKind,
}

impl ColumnSpec {
    pub fn new(driver: DriverKind, type_name: &str, nullable: bool) -> Self {
        let name = type_name.trim().to_ascii_lowercase();
        let base = name.split(['(', ' ']).next().unwrap_or("");
        let parameters: Vec<&str> = name
            .split_once('(')
            .and_then(|(_, tail)| tail.split_once(')'))
            .map(|(params, _)| params.split(',').map(str::trim).collect())
            .unwrap_or_default();
        let unsigned = name.contains("unsigned");
        let bits = match base {
            "tinyint" => Some(8),
            "smallint" | "int2" | "smallserial" => Some(16),
            "mediumint" => Some(24),
            "int" | "integer" | "int4" | "serial" => Some(32),
            "bigint" | "int8" | "bigserial" => Some(64),
            _ => None,
        };
        let kind = if name.ends_with("[]") || name.starts_with('_') {
            EditorKind::Other
        } else if let Some(bits) = bits {
            let bits = if driver == DriverKind::Sqlite {
                64
            } else {
                bits
            };
            EditorKind::Integer {
                min: if unsigned { 0 } else { -(1_i128 << (bits - 1)) },
                max: if unsigned {
                    (1_i128 << bits) - 1
                } else {
                    (1_i128 << (bits - 1)) - 1
                },
            }
        } else {
            match base {
                "bool" | "boolean" => EditorKind::Boolean,
                "enum" => enum_values(type_name)
                    .map(EditorKind::Enum)
                    .unwrap_or(EditorKind::Other),
                "numeric" | "decimal" | "dec" => EditorKind::Decimal {
                    precision: parameters.first().and_then(|v| v.parse().ok()),
                    scale: parameters
                        .get(1)
                        .and_then(|v| v.parse().ok())
                        .or_else(|| (!parameters.is_empty()).then_some(0)),
                    unsigned,
                },
                "float4" | "real" | "float" | "float8" | "double" => EditorKind::Float {
                    single: matches!(base, "float4" | "real") && driver == DriverKind::Postgres
                        || base == "float" && driver == DriverKind::MySql,
                    unsigned,
                },
                "text" | "varchar" | "char" | "character" | "bpchar" | "nvarchar" | "nchar"
                | "tinytext" | "mediumtext" | "longtext" => EditorKind::Text {
                    limit: if driver == DriverKind::Sqlite {
                        None
                    } else {
                        parameters.first().and_then(|v| v.parse().ok())
                    },
                },
                "date" => EditorKind::Date,
                "time" | "timetz" => EditorKind::Time,
                "timestamp" | "timestamptz" | "datetime" => EditorKind::Timestamp,
                "json" | "jsonb" => EditorKind::Json,
                "uuid" => EditorKind::Uuid,
                "bytea" | "blob" | "binary" | "varbinary" | "tinyblob" | "mediumblob"
                | "longblob" => EditorKind::Binary,
                _ => EditorKind::Other,
            }
        };
        Self {
            kind,
            nullable,
            type_name: type_name.into(),
            driver,
        }
    }

    pub fn driver(&self) -> DriverKind { self.driver }

    pub fn validate(&self, edit: &CellEdit) -> Result<(), String> {
        match edit {
            CellEdit::Default => Ok(()),
            CellEdit::Null if self.nullable => Ok(()),
            CellEdit::Null => Err("This column does not allow NULL.".into()),
            CellEdit::Text(text) => self.validate_text(text),
        }
    }

    pub fn validate_text(&self, text: &str) -> Result<(), String> {
        let value = text.trim();
        let pg_special = self.driver == DriverKind::Postgres
            && ["nan", "infinity", "-infinity", "+infinity"]
                .iter()
                .any(|s| value.eq_ignore_ascii_case(s));
        match &self.kind {
            EditorKind::Enum(values) => {
                if !values.iter().any(|v| v == text) {
                    return Err("Choose one of the column's declared enum values.".into());
                }
            }
            EditorKind::Boolean => {
                if boolean(value).is_none() {
                    return Err("Choose True, False or NULL (when allowed).".into());
                }
            }
            EditorKind::Integer { min, max } => {
                let parsed = value
                    .parse::<i128>()
                    .map_err(|_| "Enter a whole number without a decimal point.".to_string())?;
                if parsed < *min || parsed > *max {
                    return Err(format!("Enter a whole number from {min} to {max}."));
                }
            }
            EditorKind::Decimal {
                precision,
                scale,
                unsigned,
            } => {
                if pg_special && precision.is_some() && !value.eq_ignore_ascii_case("nan") {
                    return Err(
                        "Infinity cannot fit a numeric column with constrained precision.".into(),
                    );
                }
                if !pg_special {
                    let (integer_digits, fractional_digits, negative) = decimal_shape(value)?;
                    if *unsigned && negative {
                        return Err("This column requires a nonnegative number.".into());
                    }
                    if let (Some(precision), Some(scale)) = (precision, scale)
                        && *scale >= 0
                    {
                        if integer_digits > i64::from(*precision) - i64::from(*scale)
                            || fractional_digits > i64::from(*scale)
                        {
                            return Err(format!(
                                "Value does not fit numeric({precision},{scale}) without rounding."
                            ));
                        }
                    }
                }
            }
            EditorKind::Float { single, unsigned } => {
                let parsed = value
                    .parse::<f64>()
                    .map_err(|_| "Enter a number, optionally with an exponent.".to_string())?;
                if (!parsed.is_finite() || (*single && !(parsed as f32).is_finite())) && !pg_special
                {
                    return Err("The number is outside this floating-point type's range.".into());
                }
                if *unsigned && parsed < 0.0 {
                    return Err("This column requires a nonnegative number.".into());
                }
            }
            EditorKind::Text { limit: Some(limit) } => {
                if text.chars().take(limit + 1).count() > *limit {
                    return Err(format!("Use at most {limit} characters."));
                }
            }
            EditorKind::Date => {
                if !(self.driver == DriverKind::Postgres
                    && matches!(value, "infinity" | "-infinity"))
                    && chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").is_err()
                {
                    return Err("Enter a valid date as YYYY-MM-DD.".into());
                }
            }
            EditorKind::Timestamp => {
                let normalized = value.replacen(' ', "T", 1);
                if !(self.driver == DriverKind::Postgres
                    && matches!(value, "infinity" | "-infinity"))
                    && chrono::DateTime::parse_from_rfc3339(&normalized).is_err()
                    && chrono::NaiveDateTime::parse_from_str(&normalized, "%Y-%m-%dT%H:%M:%S%.f")
                        .is_err()
                {
                    return Err("Enter a valid YYYY-MM-DD HH:MM:SS timestamp, optionally with fractional seconds and an offset.".into());
                }
            }
            EditorKind::Time => {
                if !valid_time(value, self.driver) {
                    return Err("Enter a valid HH:MM:SS time, optionally with fractional seconds and an offset.".into());
                }
            }
            EditorKind::Binary => {
                return Err(
                    "Binary values require a binary editor; text input is unavailable.".into(),
                );
            }
            EditorKind::Json => {
                serde_json::from_str::<Box<serde_json::value::RawValue>>(text)
                    .map_err(|e| format!("Invalid JSON: {e}"))?;
            }
            EditorKind::Uuid => {
                uuid::Uuid::parse_str(value).map_err(|_| "Enter a valid UUID.".to_string())?;
            }
            _ => {}
        }
        Ok(())
    }

    /// SQL parameters, not SQL literals. Decimal text is never converted to f64.
    pub fn bind_value(&self, edit: &CellEdit) -> Result<Value, String> {
        self.validate(edit)?;
        match edit {
            CellEdit::Null => Ok(Value::Null),
            CellEdit::Default => Err("DEFAULT is SQL syntax, not a bound value.".into()),
            CellEdit::Text(text) if self.kind == EditorKind::Boolean => {
                Ok(Value::Bool(boolean(text.trim()).unwrap()))
            }
            CellEdit::Text(text) => Ok(Value::Text(text.clone())),
        }
    }
}

// MySQL catalog enum syntax; preserve case and escaped quotes in labels.
fn enum_values(type_name: &str) -> Option<Vec<String>> {
    let (_, body) = type_name.split_once('(')?;
    let body = body.strip_suffix(')')?;
    let mut chars = body.chars().peekable();
    let mut values = Vec::new();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.next()? != '\'' {
            return None;
        }
        let mut value = String::new();
        loop {
            match chars.next()? {
                '\\' => value.push(match chars.next()? {
                    '0' => '\0',
                    'b' => '\u{0008}',
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    'Z' => '\u{001a}',
                    c => c,
                }),
                '\'' if chars.peek() == Some(&'\'') => {
                    chars.next();
                    value.push('\'');
                }
                '\'' => break,
                c => value.push(c),
            }
        }
        values.push(value);
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        match chars.next() {
            None => return Some(values),
            Some(',') => {}
            _ => return None,
        }
    }
}

pub fn boolean(value: &str) -> Option<bool> {
    if ["true", "t", "1"]
        .iter()
        .any(|s| value.eq_ignore_ascii_case(s))
    {
        Some(true)
    } else if ["false", "f", "0"]
        .iter()
        .any(|s| value.eq_ignore_ascii_case(s))
    {
        Some(false)
    } else {
        None
    }
}

/// Count decimal positions without losing significant digits or expanding exponents.
fn decimal_shape(text: &str) -> Result<(i64, i64, bool), String> {
    let error = || "Enter a decimal number, optionally with an exponent.".to_string();
    let negative = text.starts_with('-');
    let unsigned = text.strip_prefix(['-', '+']).unwrap_or(text);
    let mut parts = unsigned.split(['e', 'E']);
    let mantissa = parts.next().unwrap_or("");
    let exponent = parts
        .next()
        .map(|e| e.parse::<i32>().map(i64::from))
        .transpose()
        .map_err(|_| error())?
        .unwrap_or(0);
    if parts.next().is_some() {
        return Err(error());
    }
    let mut digits = String::with_capacity(mantissa.len());
    let mut point = None;
    for c in mantissa.chars() {
        if c == '.' && point.is_none() {
            point = Some(digits.len() as i64);
        } else if c.is_ascii_digit() {
            digits.push(c);
        } else {
            return Err(error());
        }
    }
    if digits.is_empty() {
        return Err(error());
    }
    let point = point.unwrap_or(digits.len() as i64) + exponent;
    let Some(first) = digits.bytes().position(|b| b != b'0') else {
        return Ok((i64::MIN / 2, 0, false));
    };
    let last = digits.trim_end_matches('0').len() as i64;
    Ok((point - first as i64, (last - point).max(0), negative))
}

fn valid_time(value: &str, driver: DriverKind) -> bool {
    if chrono::NaiveTime::parse_from_str(value, "%H:%M:%S%.f").is_ok() {
        return true;
    }
    if driver == DriverKind::Postgres
        && chrono::DateTime::parse_from_rfc3339(&format!("2000-01-01T{value}")).is_ok()
    {
        return true;
    }
    if driver == DriverKind::MySql {
        let mut parts = value.trim_start_matches(['-', '+']).split(':');
        let hour = parts.next().and_then(|p| p.parse::<u32>().ok());
        let minute = parts.next().and_then(|p| p.parse::<u32>().ok());
        let second = parts.next().and_then(|p| p.parse::<f64>().ok());
        return matches!((hour, minute, second), (Some(h), Some(m), Some(s)) if h <= 838 && m < 60 && (0.0..60.0).contains(&s))
            && parts.next().is_none();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn typed_values_validate_without_lossy_numeric_conversion() {
        for (ty, valid, invalid) in [
            ("boolean", "true", "dupakonia"),
            ("smallint", "32767", "32768"),
            ("integer", "-2147483648", "2.5"),
            ("numeric(6,2)", "1234.50", "12345.50"),
            ("numeric(6,2)", "1.23e2", "1.234"),
            ("numeric(2,4)", "0", "0.1"),
            ("numeric(2,4)", "0.0012", "0.00123"),
            ("varchar(3)", "żół", "żółw"),
            ("date", "2028-02-29", "2026-02-29"),
            (
                "timestamptz",
                "2026-10-20T03:21:07.971+00:00",
                "2026-13-20 03:21:07",
            ),
            ("jsonb", "{\"n\":1e400}", "{broken}"),
            ("uuid", "550e8400-e29b-41d4-a716-446655440000", "nope"),
        ] {
            let spec = ColumnSpec::new(DriverKind::Postgres, ty, false);
            assert!(spec.validate_text(valid).is_ok(), "{ty}: {valid}");
            assert!(spec.validate_text(invalid).is_err(), "{ty}: {invalid}");
            assert!(spec.validate(&CellEdit::Null).is_err());
        }
        let enumeration =
            ColumnSpec::new(DriverKind::MySql, "enum('Pending','It''s ready','')", true);
        assert_eq!(
            enumeration.kind,
            EditorKind::Enum(vec!["Pending".into(), "It's ready".into(), "".into()])
        );
        assert!(enumeration.validate_text("Pending").is_ok());
        assert!(enumeration.validate_text("pending").is_err());
        assert!(enumeration.validate_text("other").is_err());
        let unsigned = ColumnSpec::new(DriverKind::MySql, "bigint unsigned", true);
        assert!(unsigned.validate_text("18446744073709551615").is_ok());
        assert!(unsigned.validate_text("18446744073709551616").is_err());
        assert!(unsigned.validate_text("-1").is_err());
        assert!(unsigned.validate(&CellEdit::Null).is_ok());
        assert!(
            ColumnSpec::new(DriverKind::Postgres, "numeric", true)
                .validate_text(&"9".repeat(1000))
                .is_ok()
        );
        assert_eq!(
            ColumnSpec::new(DriverKind::Postgres, "bool", true)
                .bind_value(&CellEdit::Text("false".into()))
                .unwrap(),
            Value::Bool(false)
        );
        assert!(
            ColumnSpec::new(DriverKind::MySql, "time", true)
                .validate_text("-838:59:59.123456")
                .is_ok()
        );
    }
}
