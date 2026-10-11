//! SQL values and the SQLite-style rules for comparing, converting and doing
//! arithmetic on them.
//!
//! Fishbowl follows SQLite's dynamic typing: a value carries its own type, and a
//! column's declared type only nudges values toward a preferred type (its
//! "affinity") when they are stored.

use std::cmp::Ordering;
use std::fmt;

use serde::Serialize;

/// One SQL value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    /// Bytes, as written `X'CAFE'`. A blob sorts after every text value and
    /// reads as text where a number is wanted, as in SQLite.
    Blob(Vec<u8>),
}

/// The type a column prefers to store, derived from its declared type name.
/// See <https://www.sqlite.org/datatype3.html#determination_of_column_affinity>.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Affinity {
    Integer,
    Text,
    Blob,
    Real,
    Numeric,
}

impl Affinity {
    /// Applies SQLite's five rules, in order, to a declared type name.
    pub fn from_declared_type(declared: &str) -> Self {
        let upper = declared.to_ascii_uppercase();
        if upper.contains("INT") {
            Affinity::Integer
        } else if ["CHAR", "CLOB", "TEXT"].iter().any(|s| upper.contains(s)) {
            Affinity::Text
        } else if upper.is_empty() || upper.contains("BLOB") {
            Affinity::Blob
        } else if ["REAL", "FLOA", "DOUB"].iter().any(|s| upper.contains(s)) {
            Affinity::Real
        } else {
            Affinity::Numeric
        }
    }
}

impl Value {
    /// Converts a value on its way into a column with the given affinity.
    pub fn apply_affinity(self, affinity: Affinity) -> Value {
        match (affinity, self) {
            (Affinity::Integer | Affinity::Numeric, Value::Text(s)) => match parse_numeric(&s) {
                Some(n) => n.into_integer_if_exact(),
                None => Value::Text(s),
            },
            (Affinity::Integer | Affinity::Numeric, Value::Real(f)) => {
                Value::Real(f).into_integer_if_exact()
            }
            (Affinity::Real, Value::Text(s)) => match parse_numeric(&s) {
                Some(n) => Value::Real(n.as_f64()),
                None => Value::Text(s),
            },
            (Affinity::Real, Value::Integer(i)) => Value::Real(i as f64),
            (Affinity::Text, Value::Integer(i)) => Value::Text(i.to_string()),
            (Affinity::Text, Value::Real(f)) => Value::Text(format_real(f)),
            (_, v) => v,
        }
    }

    fn into_integer_if_exact(self) -> Value {
        match self {
            Value::Real(f) if f.fract() == 0.0 && f.abs() < 9.007_199_254_740_992e15 => {
                Value::Integer(f as i64)
            }
            v => v,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// SQL truthiness: NULL is unknown (`None`), numbers are true when non-zero,
    /// and text is converted to a number first.
    pub fn truthiness(&self) -> Option<bool> {
        match self {
            Value::Null => None,
            Value::Integer(i) => Some(*i != 0),
            Value::Real(f) => Some(*f != 0.0),
            Value::Text(s) => Some(numeric_prefix(s).as_f64() != 0.0),
            Value::Blob(b) => Some(numeric_prefix(&String::from_utf8_lossy(b)).as_f64() != 0.0),
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Integer(_) => "integer",
            Value::Real(_) => "real",
            Value::Text(_) => "text",
            Value::Blob(_) => "blob",
        }
    }

    /// Converts to an integer the way `sqlite3_column_int64` does: reals
    /// truncate toward zero and text uses its longest numeric prefix.
    pub fn to_i64_lossy(&self) -> i64 {
        match self {
            Value::Null => 0,
            Value::Integer(i) => *i,
            Value::Real(f) => real_to_i64(*f),
            Value::Text(s) => match numeric_prefix(s) {
                Value::Integer(i) => i,
                Value::Real(f) => real_to_i64(f),
                _ => 0,
            },
            Value::Blob(b) => Value::Text(String::from_utf8_lossy(b).into()).to_i64_lossy(),
        }
    }

    /// Converts to a float the way `sqlite3_column_double` does.
    pub fn to_f64_lossy(&self) -> f64 {
        match self {
            Value::Null => 0.0,
            Value::Integer(i) => *i as f64,
            Value::Real(f) => *f,
            Value::Text(s) => numeric_prefix(s).as_f64(),
            Value::Blob(b) => numeric_prefix(&String::from_utf8_lossy(b)).as_f64(),
        }
    }

    fn as_f64(&self) -> f64 {
        match self {
            Value::Integer(i) => *i as f64,
            Value::Real(f) => *f,
            _ => 0.0,
        }
    }

    /// Orders two non-NULL values: numbers, then text, then blobs. Numbers
    /// compare by value, text and blobs byte by byte. Callers handle NULL first, because comparisons with NULL
    /// are unknown rather than less or greater.
    pub fn compare(&self, other: &Value) -> Ordering {
        match (self, other) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Null, _) => Ordering::Less,
            (_, Value::Null) => Ordering::Greater,
            (Value::Integer(a), Value::Integer(b)) => a.cmp(b),
            (Value::Integer(a), Value::Real(b)) => compare_int_real(*a, *b),
            (Value::Real(a), Value::Integer(b)) => compare_int_real(*b, *a).reverse(),
            (Value::Real(a), Value::Real(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
            (Value::Blob(a), Value::Blob(b)) => a.cmp(b),
            (Value::Blob(_), _) => Ordering::Greater,
            (_, Value::Blob(_)) => Ordering::Less,
            (Value::Text(a), Value::Text(b)) => a.as_bytes().cmp(b.as_bytes()),
            (Value::Text(_), _) => Ordering::Greater,
            (_, Value::Text(_)) => Ordering::Less,
        }
    }
}

fn real_to_i64(f: f64) -> i64 {
    // `as` saturates at the i64 bounds and maps NaN to 0, matching SQLite.
    f as i64
}

fn compare_int_real(i: i64, r: f64) -> Ordering {
    if r.is_nan() {
        return Ordering::Greater;
    }
    // Compare in float space first; break ties exactly in integer space so
    // large integers that round to the same float still order correctly.
    match (i as f64).partial_cmp(&r) {
        Some(Ordering::Equal) if r.abs() < 9.2e18 => i.cmp(&(r as i64)),
        Some(ord) => ord,
        None => Ordering::Less,
    }
}

/// Parses text that is entirely a number (surrounding spaces allowed).
pub fn parse_numeric(s: &str) -> Option<Value> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    if let Ok(i) = t.parse::<i64>() {
        return Some(Value::Integer(i));
    }
    let looks_numeric = t
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'));
    if looks_numeric {
        t.parse::<f64>()
            .ok()
            .filter(|f| f.is_finite())
            .map(Value::Real)
    } else {
        None
    }
}

/// The longest prefix of `s` that reads as a number, or 0. This is how SQLite
/// treats text in arithmetic: `'12abc' + 1` is 13.
pub fn numeric_prefix(s: &str) -> Value {
    let bytes = s.trim_start().as_bytes();
    let mut end = 0;
    if matches!(bytes.first(), Some(b'+' | b'-')) {
        end = 1;
    }
    let digits_start = end;
    while bytes.get(end).is_some_and(u8::is_ascii_digit) {
        end += 1;
    }
    let mut is_real = false;
    if bytes.get(end) == Some(&b'.') {
        is_real = true;
        end += 1;
        while bytes.get(end).is_some_and(u8::is_ascii_digit) {
            end += 1;
        }
    }
    if end == digits_start || (is_real && end == digits_start + 1) {
        return Value::Integer(0);
    }
    if matches!(bytes.get(end), Some(b'e' | b'E')) {
        let mut exp_end = end + 1;
        if matches!(bytes.get(exp_end), Some(b'+' | b'-')) {
            exp_end += 1;
        }
        if bytes.get(exp_end).is_some_and(u8::is_ascii_digit) {
            while bytes.get(exp_end).is_some_and(u8::is_ascii_digit) {
                exp_end += 1;
            }
            is_real = true;
            end = exp_end;
        }
    }
    let text = std::str::from_utf8(&bytes[..end]).unwrap_or("0");
    if !is_real && let Ok(i) = text.parse::<i64>() {
        return Value::Integer(i);
    }
    Value::Real(text.parse().unwrap_or(0.0))
}

/// Formats a float like SQLite's `%!.15g`: 15 significant digits, and always
/// a decimal point so the value still reads as a real.
pub fn format_real(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Inf".into() } else { "-Inf".into() };
    }
    if f == 0.0 {
        return "0.0".into();
    }
    let sci = format!("{f:.14e}");
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    if !(-4..15).contains(&exp) {
        let mantissa = trim_fraction(mantissa);
        let sign = if exp < 0 { '-' } else { '+' };
        return format!("{mantissa}e{sign}{:02}", exp.abs());
    }
    let decimals = (14 - exp).max(0) as usize;
    trim_fraction(&format!("{f:.decimals$}"))
}

fn trim_fraction(s: &str) -> String {
    if !s.contains('.') {
        return format!("{s}.0");
    }
    let trimmed = s.trim_end_matches('0');
    if trimmed.ends_with('.') {
        format!("{trimmed}0")
    } else {
        trimmed.to_string()
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("NULL"),
            Value::Integer(i) => write!(f, "{i}"),
            Value::Real(r) => f.write_str(&format_real(*r)),
            Value::Text(s) => f.write_str(s),
            // The bytes as they are, like `sqlite3_column_text`.
            Value::Blob(b) => f.write_str(&String::from_utf8_lossy(b)),
        }
    }
}

/// Values cross into JavaScript as `{ type, text }`. Text keeps 64-bit
/// integers exact, which JSON numbers cannot.
impl Serialize for Value {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("Value", 2)?;
        s.serialize_field("type", self.type_name())?;
        match self {
            Value::Null => s.serialize_field("text", &None::<String>)?,
            other => s.serialize_field("text", &Some(other.to_string()))?,
        }
        s.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn affinity_follows_sqlite_rules_in_order() {
        assert_eq!(Affinity::from_declared_type("INTEGER"), Affinity::Integer);
        assert_eq!(Affinity::from_declared_type("VARCHAR(10)"), Affinity::Text);
        // "INT" wins over "CHAR" because the INT rule is checked first.
        assert_eq!(Affinity::from_declared_type("CHARINT"), Affinity::Integer);
        assert_eq!(Affinity::from_declared_type(""), Affinity::Blob);
        assert_eq!(Affinity::from_declared_type("DOUBLE"), Affinity::Real);
        assert_eq!(
            Affinity::from_declared_type("DECIMAL(5,2)"),
            Affinity::Numeric
        );
    }

    #[test]
    fn integer_affinity_converts_numeric_text() {
        let v = Value::Text("42".into()).apply_affinity(Affinity::Integer);
        assert_eq!(v, Value::Integer(42));
        let v = Value::Text("4.0".into()).apply_affinity(Affinity::Integer);
        assert_eq!(v, Value::Integer(4));
        let v = Value::Text("abc".into()).apply_affinity(Affinity::Integer);
        assert_eq!(v, Value::Text("abc".into()));
    }

    #[test]
    fn numeric_prefix_reads_leading_number() {
        assert_eq!(numeric_prefix("12abc"), Value::Integer(12));
        assert_eq!(numeric_prefix(" -3.5x"), Value::Real(-3.5));
        assert_eq!(numeric_prefix("1e3"), Value::Real(1000.0));
        assert_eq!(numeric_prefix("abc"), Value::Integer(0));
        assert_eq!(numeric_prefix("."), Value::Integer(0));
    }

    #[test]
    fn reals_format_like_sqlite() {
        assert_eq!(format_real(1.0), "1.0");
        assert_eq!(format_real(0.1), "0.1");
        assert_eq!(format_real(-2.5), "-2.5");
        assert_eq!(format_real(1e20), "1.0e+20");
        assert_eq!(format_real(1.5e-7), "1.5e-07");
        assert_eq!(format_real(123456.789), "123456.789");
    }

    #[test]
    fn numbers_sort_before_text() {
        assert_eq!(
            Value::Integer(9).compare(&Value::Text("1".into())),
            Ordering::Less
        );
        assert_eq!(Value::Integer(2).compare(&Value::Real(2.5)), Ordering::Less);
        assert_eq!(
            Value::Real(3.0).compare(&Value::Integer(3)),
            Ordering::Equal
        );
    }
}
