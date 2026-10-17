//! PostgreSQL's own text output for values received in the binary format.
//!
//! Generic binary decoders lose information: NUMERIC beyond 28 digits, array
//! quoting, dimensions and lower bounds, interval microseconds. These functions
//! follow the server's `*_out` functions, so the text matches psql and the
//! type's input function accepts it back unchanged.

use std::fmt::Write;
use tokio_postgres::types::{Field, Kind, Type};

/// Text output of one non-NULL value, or `None` when the type is not covered
/// or the bytes are malformed; callers must then treat the value as inexact.
pub(super) fn output(ty: &Type, raw: &[u8]) -> Option<String> {
    let mut out = String::new();
    write_value(ty, raw, &mut out)?;
    Some(out)
}

/// True for types whose binary format is their UTF-8 text.
pub(super) fn is_text_type(ty: &Type) -> bool {
    matches!(ty.kind(), Kind::Enum(_))
        || matches!(
            *ty,
            Type::TEXT
                | Type::VARCHAR
                | Type::BPCHAR
                | Type::NAME
                | Type::XML
                | Type::UNKNOWN
                | Type::JSON
                | Type::CSTRING
                | Type::REFCURSOR
                | Type::PG_NODE_TREE
                // `void` (a function without a result) is sent as no bytes.
                | Type::VOID
        )
        || ty.name() == "citext"
}

fn write_value(ty: &Type, raw: &[u8], out: &mut String) -> Option<()> {
    if matches!(*ty, Type::INT2_VECTOR | Type::OID_VECTOR) {
        return vector(raw, out);
    }
    match ty.kind() {
        Kind::Array(element) => return array(element, raw, out),
        Kind::Range(element) => return range(element, raw, out),
        Kind::Multirange(element) => return multirange(element, raw, out),
        Kind::Domain(base) => return write_value(base, raw, out),
        Kind::Composite(fields) => return composite(Some(fields), raw, out),
        _ => {}
    }
    if is_text_type(ty) {
        out.push_str(std::str::from_utf8(raw).ok()?);
        return Some(());
    }
    let mut b = Buf(raw);
    match *ty {
        Type::BOOL => out.push(if b.u8()? != 0 { 't' } else { 'f' }),
        Type::INT2 => write!(out, "{}", b.i16()?).ok()?,
        Type::INT4 => write!(out, "{}", b.i32()?).ok()?,
        Type::INT8 => write!(out, "{}", b.i64()?).ok()?,
        Type::OID | Type::XID | Type::CID | Type::REGPROC | Type::REGPROCEDURE | Type::REGOPER
        | Type::REGOPERATOR | Type::REGCLASS | Type::REGTYPE | Type::REGCONFIG
        | Type::REGDICTIONARY | Type::REGNAMESPACE | Type::REGROLE | Type::REGCOLLATION => {
            write!(out, "{}", b.u32()?).ok()?
        }
        Type::XID8 => write!(out, "{}", b.u64()?).ok()?,
        Type::PG_LSN => {
            let lsn = b.u64()?;
            write!(out, "{:X}/{:X}", lsn >> 32, lsn & 0xFFFF_FFFF).ok()?
        }
        Type::FLOAT4 => out.push_str(&float_out(f32::from_bits(b.u32()?) as f64, true)),
        Type::FLOAT8 => out.push_str(&float_out(f64::from_bits(b.u64()?), false)),
        Type::NUMERIC => {
            out.push_str(&numeric(raw)?);
            return Some(());
        }
        Type::CHAR => {
            let byte = b.u8()?;
            match byte {
                0 => {}
                1..=0x7F => out.push(byte as char),
                _ => write!(out, "\\{byte:03o}").ok()?,
            }
        }
        Type::JSONB | Type::JSONPATH => {
            let (1, text) = raw.split_first()? else {
                return None;
            };
            out.push_str(std::str::from_utf8(text).ok()?);
            return Some(());
        }
        Type::BYTEA => {
            out.push_str("\\x");
            for byte in raw {
                write!(out, "{byte:02x}").ok()?;
            }
            return Some(());
        }
        Type::UUID => {
            let bytes: [u8; 16] = raw.try_into().ok()?;
            out.push_str(&uuid::Uuid::from_bytes(bytes).hyphenated().to_string());
            return Some(());
        }
        Type::DATE => out.push_str(&date(b.i32()?)?),
        Type::TIMESTAMP => out.push_str(&timestamp(b.i64()?, false)?),
        Type::TIMESTAMPTZ => out.push_str(&timestamp(b.i64()?, true)?),
        Type::TIME => out.push_str(&time(b.i64()?)),
        Type::TIMETZ => {
            out.push_str(&time(b.i64()?));
            zone(-b.i32()?, out);
        }
        Type::INTERVAL => {
            out.push_str(&interval(raw)?);
            return Some(());
        }
        Type::INET | Type::CIDR => {
            out.push_str(&inet(raw)?);
            return Some(());
        }
        Type::MACADDR | Type::MACADDR8 => {
            let expected = if *ty == Type::MACADDR { 6 } else { 8 };
            if raw.len() != expected {
                return None;
            }
            let parts: Vec<String> = raw.iter().map(|byte| format!("{byte:02x}")).collect();
            out.push_str(&parts.join(":"));
            return Some(());
        }
        Type::BIT | Type::VARBIT => {
            let bits = usize::try_from(b.i32()?).ok()?;
            let bytes = b.take(bits.div_ceil(8))?;
            for i in 0..bits {
                out.push(if bytes[i / 8] & (0x80 >> (i % 8)) != 0 { '1' } else { '0' });
            }
        }
        Type::POINT => point(&mut b, out)?,
        Type::LINE => {
            let (a, bb, c) = (b.f64()?, b.f64()?, b.f64()?);
            write!(out, "{{{},{},{}}}", float_out(a, false), float_out(bb, false), float_out(c, false)).ok()?;
        }
        Type::LSEG => {
            out.push('[');
            point(&mut b, out)?;
            out.push(',');
            point(&mut b, out)?;
            out.push(']');
        }
        Type::BOX => {
            point(&mut b, out)?;
            out.push(',');
            point(&mut b, out)?;
        }
        Type::PATH | Type::POLYGON => {
            let closed = if *ty == Type::PATH { b.u8()? != 0 } else { true };
            let points = usize::try_from(b.i32()?).ok()?;
            out.push(if closed { '(' } else { '[' });
            for i in 0..points {
                if i > 0 {
                    out.push(',');
                }
                point(&mut b, out)?;
            }
            out.push(if closed { ')' } else { ']' });
        }
        Type::CIRCLE => {
            out.push('<');
            point(&mut b, out)?;
            write!(out, ",{}>", float_out(b.f64()?, false)).ok()?;
        }
        Type::RECORD => return composite(None, raw, out),
        _ => return None,
    }
    b.is_empty().then_some(())
}

struct Buf<'a>(&'a [u8]);

impl<'a> Buf<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Some(head)
    }
    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn i16(&mut self) -> Option<i16> {
        Some(i16::from_be_bytes(self.array()?))
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_be_bytes(self.array()?))
    }
    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_be_bytes(self.array()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.array()?))
    }
    fn i64(&mut self) -> Option<i64> {
        Some(i64::from_be_bytes(self.array()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.array()?))
    }
    fn f64(&mut self) -> Option<f64> {
        Some(f64::from_bits(self.u64()?))
    }
    /// A length-prefixed value; `None` inside for SQL NULL.
    fn value(&mut self) -> Option<Option<&'a [u8]>> {
        match self.i32()? {
            -1 => Some(None),
            len => Some(Some(self.take(usize::try_from(len).ok()?)?)),
        }
    }
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

fn point(b: &mut Buf, out: &mut String) -> Option<()> {
    let (x, y) = (b.f64()?, b.f64()?);
    write!(out, "({},{})", float_out(x, false), float_out(y, false)).ok()
}

/// `array_out`: nested braces, `[lb:ub]=` dimensions when a lower bound is
/// not 1, and quotes for elements the array parser would misread.
fn array(element: &Type, raw: &[u8], out: &mut String) -> Option<()> {
    let mut b = Buf(raw);
    let dims = usize::try_from(b.i32()?).ok()?;
    let _has_nulls = b.i32()?;
    let _element_oid = b.u32()?;
    if dims == 0 {
        out.push_str("{}");
        return b.is_empty().then_some(());
    }
    if dims > 6 {
        return None;
    }
    let mut shape = Vec::with_capacity(dims);
    for _ in 0..dims {
        let len = usize::try_from(b.i32()?).ok()?;
        shape.push((len, b.i32()?));
    }
    if shape.iter().any(|&(_, lower)| lower != 1) {
        for &(len, lower) in &shape {
            write!(out, "[{lower}:{}]", i64::from(lower) + len as i64 - 1).ok()?;
        }
        out.push('=');
    }
    // `box` is the only built-in type whose array delimiter is not a comma.
    let delimiter = if *element == Type::BOX { ';' } else { ',' };
    array_level(element, &shape, &mut b, delimiter, out)?;
    b.is_empty().then_some(())
}

/// `int2vector` and `oidvector` travel as arrays but print as a plain list.
fn vector(raw: &[u8], out: &mut String) -> Option<()> {
    let mut b = Buf(raw);
    let dims = b.i32()?;
    let _has_nulls = b.i32()?;
    let element_oid = b.u32()?;
    let count = match dims {
        0 => 0,
        1 => {
            let count = usize::try_from(b.i32()?).ok()?;
            let _lower = b.i32()?;
            count
        }
        _ => return None,
    };
    for i in 0..count {
        if i > 0 {
            out.push(' ');
        }
        let raw = b.value()??;
        match element_oid {
            21 => write!(out, "{}", i16::from_be_bytes(raw.try_into().ok()?)).ok()?,
            26 => write!(out, "{}", u32::from_be_bytes(raw.try_into().ok()?)).ok()?,
            _ => return None,
        }
    }
    b.is_empty().then_some(())
}

fn array_level(
    element: &Type,
    shape: &[(usize, i32)],
    b: &mut Buf,
    delimiter: char,
    out: &mut String,
) -> Option<()> {
    out.push('{');
    for i in 0..shape[0].0 {
        if i > 0 {
            out.push(delimiter);
        }
        if shape.len() > 1 {
            array_level(element, &shape[1..], b, delimiter, out)?;
        } else {
            match b.value()? {
                None => out.push_str("NULL"),
                Some(raw) => {
                    let text = output(element, raw)?;
                    let quote = text.is_empty()
                        || text.eq_ignore_ascii_case("NULL")
                        || text.chars().any(|c| {
                            matches!(c, '"' | '\\' | '{' | '}' | ' ' | '\t' | '\n' | '\r' | '\x0B' | '\x0C')
                                || c == delimiter
                        });
                    push_quoted(&text, quote, '\\', out);
                }
            }
        }
    }
    out.push('}');
    Some(())
}

/// Writes `text`, in double quotes when `quote`; `"` and `\` are escaped with
/// `escape` (a backslash for arrays, doubling for ranges and records).
fn push_quoted(text: &str, quote: bool, escape: char, out: &mut String) {
    if !quote {
        out.push_str(text);
        return;
    }
    out.push('"');
    for c in text.chars() {
        if c == '"' || c == '\\' {
            out.push(if escape == '\\' { '\\' } else { c });
        }
        out.push(c);
    }
    out.push('"');
}

/// Quoting shared by `range_out` bounds and `record_out` fields.
fn push_bound(text: &str, out: &mut String) {
    let quote = text.is_empty()
        || text
            .chars()
            .any(|c| matches!(c, '"' | '\\' | '(' | ')' | '[' | ']' | ',') || c.is_ascii_whitespace() || c == '\x0B');
    push_quoted(text, quote, '"', out);
}

fn range(element: &Type, raw: &[u8], out: &mut String) -> Option<()> {
    const EMPTY: u8 = 0x01;
    const LOWER_INCLUSIVE: u8 = 0x02;
    const UPPER_INCLUSIVE: u8 = 0x04;
    const LOWER_INFINITE: u8 = 0x08;
    const UPPER_INFINITE: u8 = 0x10;
    let mut b = Buf(raw);
    let flags = b.u8()?;
    if flags & EMPTY != 0 {
        out.push_str("empty");
        return b.is_empty().then_some(());
    }
    out.push(if flags & LOWER_INCLUSIVE != 0 { '[' } else { '(' });
    if flags & LOWER_INFINITE == 0 {
        push_bound(&output(element, b.value()??)?, out);
    }
    out.push(',');
    if flags & UPPER_INFINITE == 0 {
        push_bound(&output(element, b.value()??)?, out);
    }
    out.push(if flags & UPPER_INCLUSIVE != 0 { ']' } else { ')' });
    b.is_empty().then_some(())
}

fn multirange(element: &Type, raw: &[u8], out: &mut String) -> Option<()> {
    let mut b = Buf(raw);
    let count = usize::try_from(b.i32()?).ok()?;
    out.push('{');
    for i in 0..count {
        if i > 0 {
            out.push(',');
        }
        range(element, b.value()??, out)?;
    }
    out.push('}');
    b.is_empty().then_some(())
}

/// `record_out`; an anonymous record names each field's type by OID.
fn composite(fields: Option<&[Field]>, raw: &[u8], out: &mut String) -> Option<()> {
    let mut b = Buf(raw);
    let count = usize::try_from(b.i32()?).ok()?;
    if fields.is_some_and(|fields| fields.len() != count) {
        return None;
    }
    out.push('(');
    for i in 0..count {
        if i > 0 {
            out.push(',');
        }
        let oid = b.u32()?;
        let Some(raw) = b.value()? else {
            continue;
        };
        let ty = match fields {
            Some(fields) => fields[i].type_().clone(),
            None => Type::from_oid(oid)?,
        };
        push_bound(&output(&ty, raw)?, out);
    }
    out.push(')');
    b.is_empty().then_some(())
}

/// `float8out`/`float4out` with the default `extra_float_digits` (shortest
/// exact form, exponent notation outside 1e-4 .. 1e15 or 1e6 for float4).
pub(super) fn float_out(value: f64, single: bool) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    let sign = if value.is_sign_negative() { "-" } else { "" };
    if value == 0.0 {
        return format!("{sign}0");
    }
    let scientific = if single {
        format!("{:e}", (value as f32).abs())
    } else {
        format!("{:e}", value.abs())
    };
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let fixed_below = if single { 6 } else { 15 };
    if (-4..fixed_below).contains(&exponent) {
        if exponent < 0 {
            return format!("{sign}0.{}{digits}", "0".repeat((-exponent - 1) as usize));
        }
        let integer = exponent as usize + 1;
        if digits.len() <= integer {
            return format!("{sign}{digits}{}", "0".repeat(integer - digits.len()));
        }
        return format!("{sign}{}.{}", &digits[..integer], &digits[integer..]);
    }
    let mut out = format!("{sign}{}", &digits[..1]);
    if digits.len() > 1 {
        out.push('.');
        out.push_str(&digits[1..]);
    }
    let _ = write!(out, "e{}{:02}", if exponent < 0 { '-' } else { '+' }, exponent.unsigned_abs());
    out
}

/// Binary NUMERIC: digit count, weight of the first base-10000 digit, sign
/// (with NaN and infinities) and display scale, then the digits.
pub(super) fn numeric(raw: &[u8]) -> Option<String> {
    let mut b = Buf(raw);
    let count = usize::from(b.u16()?);
    let weight = i32::from(b.i16()?);
    let sign = b.u16()?;
    let scale = usize::from(b.u16()? & 0x3FFF);
    match sign {
        0xC000 => return b.is_empty().then(|| "NaN".into()),
        0xD000 => return b.is_empty().then(|| "Infinity".into()),
        0xF000 => return b.is_empty().then(|| "-Infinity".into()),
        0x0000 | 0x4000 => {}
        _ => return None,
    }
    let mut digits = Vec::with_capacity(count);
    for _ in 0..count {
        let digit = b.u16()?;
        if digit >= 10_000 {
            return None;
        }
        digits.push(digit);
    }
    if !b.is_empty() {
        return None;
    }
    let digit = |d: i32| usize::try_from(d).ok().and_then(|d| digits.get(d)).copied().unwrap_or(0);
    let mut out = String::new();
    if sign == 0x4000 {
        out.push('-');
    }
    let mut d;
    if weight < 0 {
        d = weight + 1;
        out.push('0');
    } else {
        d = 0;
        while d <= weight {
            if d == 0 {
                let _ = write!(out, "{}", digit(d));
            } else {
                let _ = write!(out, "{:04}", digit(d));
            }
            d += 1;
        }
    }
    if scale > 0 {
        out.push('.');
        let start = out.len();
        while out.len() - start < scale {
            let _ = write!(out, "{:04}", digit(d));
            d += 1;
        }
        out.truncate(start + scale);
    }
    Some(out)
}

/// Binary INTERVAL in the default `IntervalStyle` (postgres): microseconds,
/// days and months, each part with its own sign.
pub(super) fn interval(raw: &[u8]) -> Option<String> {
    const HOUR: i64 = 3_600_000_000;
    const MINUTE: i64 = 60_000_000;
    const SECOND: i64 = 1_000_000;
    let mut b = Buf(raw);
    let (time, days, months) = (b.i64()?, b.i32()?, b.i32()?);
    if !b.is_empty() {
        return None;
    }
    if (time, days, months) == (i64::MAX, i32::MAX, i32::MAX) {
        return Some("infinity".into());
    }
    if (time, days, months) == (i64::MIN, i32::MIN, i32::MIN) {
        return Some("-infinity".into());
    }
    let mut out = String::new();
    let (mut is_zero, mut is_before) = (true, false);
    for (value, unit) in [
        (i64::from(months / 12), "year"),
        (i64::from(months % 12), "mon"),
        (i64::from(days), "day"),
    ] {
        if value == 0 {
            continue;
        }
        let _ = write!(
            out,
            "{}{}{value} {unit}{}",
            if is_zero { "" } else { " " },
            if is_before && value > 0 { "+" } else { "" },
            if value != 1 { "s" } else { "" }
        );
        is_before = value < 0;
        is_zero = false;
    }
    let (hour, minute, second, fraction) =
        (time / HOUR, time % HOUR / MINUTE, time % MINUTE / SECOND, time % SECOND);
    if is_zero || time != 0 {
        let minus = hour < 0 || minute < 0 || second < 0 || fraction < 0;
        let _ = write!(
            out,
            "{}{}{:02}:{:02}:{:02}",
            if is_zero { "" } else { " " },
            if minus { "-" } else if is_before { "+" } else { "" },
            hour.unsigned_abs(),
            minute.unsigned_abs(),
            second.unsigned_abs()
        );
        push_fraction(fraction.unsigned_abs(), &mut out);
    }
    Some(out)
}

/// Microseconds as `.ffffff` without trailing zeros, nothing for zero.
fn push_fraction(micros: u64, out: &mut String) {
    if micros != 0 {
        let digits = format!("{micros:06}");
        out.push('.');
        out.push_str(digits.trim_end_matches('0'));
    }
}

/// Day number of 2000-01-01, the PostgreSQL epoch, counted with 0001-01-01 as day 1.
const EPOCH_CE: i64 = 730_120;

fn civil(days: i64) -> Option<(String, bool)> {
    let date = chrono::NaiveDate::from_num_days_from_ce_opt(i32::try_from(days + EPOCH_CE).ok()?)?;
    use chrono::Datelike;
    let (year, bc) = if date.year() > 0 { (date.year(), false) } else { (1 - date.year(), true) };
    Some((format!("{year:04}-{:02}-{:02}", date.month(), date.day()), bc))
}

fn date(days: i32) -> Option<String> {
    match days {
        i32::MAX => Some("infinity".into()),
        i32::MIN => Some("-infinity".into()),
        _ => {
            let (date, bc) = civil(i64::from(days))?;
            Some(if bc { format!("{date} BC") } else { date })
        }
    }
}

fn time(micros: i64) -> String {
    let micros = micros.unsigned_abs();
    let seconds = micros / 1_000_000;
    let mut out = format!("{:02}:{:02}:{:02}", seconds / 3600, seconds / 60 % 60, seconds % 60);
    push_fraction(micros % 1_000_000, &mut out);
    out
}

/// TIMESTAMPTZ is shown in UTC, as the session's other values are.
fn timestamp(micros: i64, utc: bool) -> Option<String> {
    match micros {
        i64::MAX => return Some("infinity".into()),
        i64::MIN => return Some("-infinity".into()),
        _ => {}
    }
    const DAY: i64 = 86_400_000_000;
    let (date, bc) = civil(micros.div_euclid(DAY))?;
    let mut out = format!("{date} {}", time(micros.rem_euclid(DAY)));
    if utc {
        out.push_str("+00");
    }
    if bc {
        out.push_str(" BC");
    }
    Some(out)
}

/// `+HH[:MM[:SS]]` for an offset east of UTC in seconds.
fn zone(offset: i32, out: &mut String) {
    let abs = offset.unsigned_abs();
    let (hours, minutes, seconds) = (abs / 3600, abs / 60 % 60, abs % 60);
    let _ = write!(out, "{}{hours:02}", if offset < 0 { '-' } else { '+' });
    if minutes != 0 || seconds != 0 {
        let _ = write!(out, ":{minutes:02}");
    }
    if seconds != 0 {
        let _ = write!(out, ":{seconds:02}");
    }
}

/// INET keeps the prefix length unless it is the full address; CIDR always shows it.
fn inet(raw: &[u8]) -> Option<String> {
    let mut b = Buf(raw);
    let (family, bits, is_cidr, len) = (b.u8()?, b.u8()?, b.u8()?, b.u8()?);
    let bytes = b.take(usize::from(len))?;
    if !b.is_empty() {
        return None;
    }
    let (address, max) = match family {
        2 => (std::net::IpAddr::from(<[u8; 4]>::try_from(bytes).ok()?), 32),
        3 => (std::net::IpAddr::from(<[u8; 16]>::try_from(bytes).ok()?), 128),
        _ => return None,
    };
    Some(if is_cidr != 0 || bits != max { format!("{address}/{bits}") } else { address.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn array_raw(element_oid: u32, dims: &[(i32, i32)], items: &[Option<&[u8]>]) -> Vec<u8> {
        let mut raw = Vec::new();
        raw.extend_from_slice(&(dims.len() as i32).to_be_bytes());
        raw.extend_from_slice(&i32::from(items.iter().any(Option::is_none)).to_be_bytes());
        raw.extend_from_slice(&element_oid.to_be_bytes());
        for (len, lower) in dims {
            raw.extend_from_slice(&len.to_be_bytes());
            raw.extend_from_slice(&lower.to_be_bytes());
        }
        for item in items {
            match item {
                None => raw.extend_from_slice(&(-1i32).to_be_bytes()),
                Some(bytes) => {
                    raw.extend_from_slice(&(bytes.len() as i32).to_be_bytes());
                    raw.extend_from_slice(bytes);
                }
            }
        }
        raw
    }

    #[test]
    fn text_arrays_are_quoted_like_array_out() {
        let items: [Option<&[u8]>; 10] = [
            Some(b"a,b"),
            Some(b"NULL"),
            None,
            Some(b""),
            Some(b"say \"hi\""),
            Some(b"back\\slash"),
            Some(b"{braces}"),
            Some(b" padded "),
            Some(b"plain"),
            Some(b"null"),
        ];
        let raw = array_raw(25, &[(10, 1)], &items);
        assert_eq!(
            output(&Type::TEXT_ARRAY, &raw).unwrap(),
            r#"{"a,b","NULL",NULL,"","say \"hi\"","back\\slash","{braces}"," padded ",plain,"null"}"#
        );
    }

    #[test]
    fn arrays_keep_dimensions_and_lower_bounds() {
        let one = 1i32.to_be_bytes();
        let two = 2i32.to_be_bytes();
        let three = 3i32.to_be_bytes();
        let four = 4i32.to_be_bytes();
        let raw = array_raw(23, &[(2, 1), (2, 1)], &[Some(&one), Some(&two), Some(&three), Some(&four)]);
        assert_eq!(output(&Type::INT4_ARRAY, &raw).unwrap(), "{{1,2},{3,4}}");
        let raw = array_raw(25, &[(2, 0)], &[Some(b"x"), Some(b"y")]);
        assert_eq!(output(&Type::TEXT_ARRAY, &raw).unwrap(), "[0:1]={x,y}");
        assert_eq!(output(&Type::TEXT_ARRAY, &array_raw(25, &[], &[])).unwrap(), "{}");
        // A truncated body is never shown as a shorter array.
        let mut raw = array_raw(25, &[(2, 1)], &[Some(b"x"), Some(b"y")]);
        raw.pop();
        assert_eq!(output(&Type::TEXT_ARRAY, &raw), None);
    }

    fn numeric_raw(weight: i16, sign: u16, scale: u16, digits: &[u16]) -> Vec<u8> {
        let mut raw = Vec::new();
        raw.extend_from_slice(&(digits.len() as u16).to_be_bytes());
        raw.extend_from_slice(&weight.to_be_bytes());
        raw.extend_from_slice(&sign.to_be_bytes());
        raw.extend_from_slice(&scale.to_be_bytes());
        for digit in digits {
            raw.extend_from_slice(&digit.to_be_bytes());
        }
        raw
    }

    #[test]
    fn numeric_is_decoded_digit_for_digit() {
        assert_eq!(numeric(&numeric_raw(10, 0, 0, &[1])).unwrap(), format!("1{}", "0".repeat(40)));
        assert_eq!(
            numeric(&numeric_raw(8, 0, 3, &[123, 4567, 8901, 2345, 6789, 123, 4567, 8901, 2345, 1230])).unwrap(),
            "12345678901234567890123456789012345.123"
        );
        assert_eq!(
            numeric(&numeric_raw(-1, 0, 31, &[1234, 5678, 9012, 3456, 7890, 1234, 5678, 9010])).unwrap(),
            "0.1234567890123456789012345678901"
        );
        assert_eq!(numeric(&numeric_raw(-2, 0x4000, 6, &[5000])).unwrap(), "-0.000050");
        assert_eq!(numeric(&numeric_raw(0, 0, 2, &[])).unwrap(), "0.00");
        assert_eq!(numeric(&numeric_raw(0, 0xC000, 0, &[])).unwrap(), "NaN");
        assert_eq!(numeric(&numeric_raw(0, 0xD000, 0, &[])).unwrap(), "Infinity");
        assert_eq!(numeric(&numeric_raw(0, 0xF000, 0, &[])).unwrap(), "-Infinity");
        assert_eq!(numeric(&numeric_raw(0, 0, 0, &[10_000])), None);
    }

    fn interval_raw(micros: i64, days: i32, months: i32) -> Vec<u8> {
        let mut raw = micros.to_be_bytes().to_vec();
        raw.extend_from_slice(&days.to_be_bytes());
        raw.extend_from_slice(&months.to_be_bytes());
        raw
    }

    #[test]
    fn interval_matches_the_postgres_style() {
        let cases = [
            (interval_raw(0, 0, 0), "00:00:00"),
            (interval_raw(3_723_000_001, 0, 0), "01:02:03.000001"),
            (interval_raw(-5_400_000_000, 0, 0), "-01:30:00"),
            (interval_raw(-5_400_000_000, 1, 0), "1 day -01:30:00"),
            (interval_raw(7_200_000_000, -1, 0), "-1 days +02:00:00"),
            (interval_raw(500_000, 3, 14), "1 year 2 mons 3 days 00:00:00.5"),
            (interval_raw(0, 0, -13), "-1 years -1 mons"),
            (interval_raw(-1, 0, 0), "-00:00:00.000001"),
            (interval_raw(i64::MAX, i32::MAX, i32::MAX), "infinity"),
        ];
        for (raw, text) in cases {
            assert_eq!(interval(&raw).unwrap(), text);
        }
    }

    #[test]
    fn floats_use_the_shortest_exact_form() {
        assert_eq!(float_out(0.1, false), "0.1");
        assert_eq!(float_out(1e20, false), "1e+20");
        assert_eq!(float_out(1.5e-5, false), "1.5e-05");
        assert_eq!(float_out(123456789012345.0, false), "123456789012345");
        assert_eq!(float_out(-0.0, false), "-0");
        assert_eq!(float_out(f64::NEG_INFINITY, false), "-Infinity");
        assert_eq!(float_out(1234567.0, true), "1.234567e+06");
        assert_eq!(float_out(0.1f32 as f64, true), "0.1");
    }

    #[test]
    fn dates_times_ranges_and_addresses() {
        assert_eq!(date(0).unwrap(), "2000-01-01");
        assert_eq!(date(-730_120 - 365).unwrap(), "0001-01-01 BC");
        assert_eq!(date(i32::MAX).unwrap(), "infinity");
        assert_eq!(timestamp(86_400_000_000 + 1_500_000, false).unwrap(), "2000-01-02 00:00:01.5");
        assert_eq!(timestamp(-1, true).unwrap(), "1999-12-31 23:59:59.999999+00");
        let mut range = vec![0x02];
        for bound in [b"a b".as_slice(), b"z"] {
            range.extend_from_slice(&(bound.len() as i32).to_be_bytes());
            range.extend_from_slice(bound);
        }
        assert_eq!(output(&Type::new("r".into(), 1, Kind::Range(Type::TEXT), "s".into()), &range).unwrap(), "[\"a b\",z)");
        assert_eq!(inet(&[2, 8, 0, 4, 10, 0, 0, 1]).unwrap(), "10.0.0.1/8");
        assert_eq!(inet(&[2, 32, 0, 4, 10, 0, 0, 1]).unwrap(), "10.0.0.1");
        assert_eq!(inet(&[2, 32, 1, 4, 10, 0, 0, 1]).unwrap(), "10.0.0.1/32");
        let one = 1i16.to_be_bytes();
        let two = 2i16.to_be_bytes();
        let raw = array_raw(21, &[(2, 0)], &[Some(&one), Some(&two)]);
        assert_eq!(output(&Type::INT2_VECTOR, &raw).unwrap(), "1 2");
        assert_eq!(output(&Type::VOID, &[]).unwrap(), "");
    }
}
