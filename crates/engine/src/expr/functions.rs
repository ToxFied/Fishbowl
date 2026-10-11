//! Scalar functions such as `abs()` and `coalesce()`: they take values from one
//! row and return one value. Each follows SQLite's definition, including what
//! it does with NULL and with arguments of the "wrong" type.
//! See <https://www.sqlite.org/lang_corefunc.html>.

use crate::error::{Error, Result};
use crate::value::Value;

use super::{Env, Expr, to_integer};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarFunc {
    Abs,
    Coalesce,
    IfNull,
    Iif,
    NullIf,
    Length,
    Lower,
    Upper,
    Substr,
    Trim,
    LTrim,
    RTrim,
    Replace,
    Round,
    TypeOf,
    Instr,
    /// `min(a, b, ...)` with two or more arguments. With one it is an aggregate.
    Min,
    Max,
}

impl ScalarFunc {
    /// Looks up a function by its lowercase name.
    pub fn from_name(name: &str) -> Option<ScalarFunc> {
        use ScalarFunc::*;
        Some(match name {
            "abs" => Abs,
            "coalesce" => Coalesce,
            "ifnull" => IfNull,
            "iif" => Iif,
            "nullif" => NullIf,
            "length" => Length,
            "lower" => Lower,
            "upper" => Upper,
            "substr" | "substring" => Substr,
            "trim" => Trim,
            "ltrim" => LTrim,
            "rtrim" => RTrim,
            "replace" => Replace,
            "round" => Round,
            "typeof" => TypeOf,
            "instr" => Instr,
            "min" => Min,
            "max" => Max,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        use ScalarFunc::*;
        match self {
            Abs => "abs",
            Coalesce => "coalesce",
            IfNull => "ifnull",
            Iif => "iif",
            NullIf => "nullif",
            Length => "length",
            Lower => "lower",
            Upper => "upper",
            Substr => "substr",
            Trim => "trim",
            LTrim => "ltrim",
            RTrim => "rtrim",
            Replace => "replace",
            Round => "round",
            TypeOf => "typeof",
            Instr => "instr",
            Min => "min",
            Max => "max",
        }
    }

    /// Whether the function takes `n` arguments.
    pub fn accepts(self, n: usize) -> bool {
        use ScalarFunc::*;
        match self {
            Abs | Length | Lower | Upper | TypeOf => n == 1,
            IfNull | NullIf | Instr => n == 2,
            Replace => n == 3,
            Iif | Substr => n == 2 || n == 3,
            Trim | LTrim | RTrim | Round => n == 1 || n == 2,
            Coalesce | Min | Max => n >= 2,
        }
    }

    pub(super) fn eval(self, args: &[Expr], row: &[Value], env: &mut dyn Env) -> Result<Value> {
        use ScalarFunc::*;
        // These evaluate only the arguments they need, as SQLite does, so an
        // error in an unused argument never surfaces.
        match self {
            Coalesce | IfNull => {
                for arg in args {
                    let v = arg.eval_with(row, env)?;
                    if !v.is_null() {
                        return Ok(v);
                    }
                }
                return Ok(Value::Null);
            }
            Iif => {
                return if args[0].eval_with(row, env)?.truthiness() == Some(true) {
                    args[1].eval_with(row, env)
                } else {
                    args.get(2)
                        .map_or(Ok(Value::Null), |e| e.eval_with(row, env))
                };
            }
            _ => {}
        }
        let values = args
            .iter()
            .map(|a| a.eval_with(row, env))
            .collect::<Result<Vec<_>>>()?;
        self.call(&values)
    }

    fn call(self, args: &[Value]) -> Result<Value> {
        use ScalarFunc::*;
        let null = Value::Null;
        Ok(match self {
            Abs => match &args[0] {
                Value::Null => null,
                Value::Integer(i) => Value::Integer(
                    i.checked_abs()
                        .ok_or_else(|| Error::Runtime("integer overflow".into()))?,
                ),
                // Text goes through its numeric prefix and always gives a real.
                v => Value::Real(v.to_f64_lossy().abs()),
            },
            NullIf => {
                if args[0].compare(&args[1]).is_ne() {
                    args[0].clone()
                } else {
                    null
                }
            }
            // A blob's length is in bytes, text's in characters.
            Length => match &args[0] {
                Value::Blob(b) => Value::Integer(b.len() as i64),
                v => text(v).map_or(null, |s| Value::Integer(s.chars().count() as i64)),
            },
            Lower => text(&args[0]).map_or(null, |s| Value::Text(s.to_ascii_lowercase())),
            Upper => text(&args[0]).map_or(null, |s| Value::Text(s.to_ascii_uppercase())),
            Substr => {
                if args.iter().any(Value::is_null) {
                    return Ok(null);
                }
                let s = args[0].to_string();
                let length = args.get(2).map(to_integer);
                Value::Text(substr(&s, to_integer(&args[1]), length))
            }
            Trim | LTrim | RTrim => {
                let (Some(s), Some(set)) = (
                    text(&args[0]),
                    args.get(1).map_or(Some(" ".to_string()), text),
                ) else {
                    return Ok(null);
                };
                let strip = |c: char| set.contains(c);
                Value::Text(
                    match self {
                        Trim => s.trim_matches(strip),
                        LTrim => s.trim_start_matches(strip),
                        _ => s.trim_end_matches(strip),
                    }
                    .to_string(),
                )
            }
            Replace => {
                let (Some(s), Some(pattern)) = (text(&args[0]), text(&args[1])) else {
                    return Ok(null);
                };
                if pattern.is_empty() {
                    return Ok(Value::Text(s));
                }
                text(&args[2]).map_or(null, |with| Value::Text(s.replace(&pattern, &with)))
            }
            Round => {
                let digits = match args.get(1) {
                    None => 0,
                    Some(Value::Null) => return Ok(null),
                    Some(v) => to_integer(v).clamp(0, 30) as usize,
                };
                if args[0].is_null() {
                    return Ok(null);
                }
                Value::Real(round(args[0].to_f64_lossy(), digits))
            }
            TypeOf => Value::Text(args[0].type_name().to_string()),
            Instr => {
                let (Some(haystack), Some(needle)) = (text(&args[0]), text(&args[1])) else {
                    return Ok(null);
                };
                let position = haystack
                    .find(&needle)
                    .map_or(0, |at| haystack[..at].chars().count() + 1);
                Value::Integer(position as i64)
            }
            Min | Max => {
                if args.iter().any(Value::is_null) {
                    return Ok(null);
                }
                // On ties min() keeps the last argument and max() the first,
                // which only shows when 1 and 1.0 tie.
                let mut best = 0;
                for i in 1..args.len() {
                    let ord = args[best].compare(&args[i]);
                    if (self == Max && ord.is_lt()) || (self == Min && ord.is_ge()) {
                        best = i;
                    }
                }
                args[best].clone()
            }
            Coalesce | IfNull | Iif => unreachable!("evaluated lazily in eval"),
        })
    }
}

/// A value as text, or `None` for NULL.
fn text(v: &Value) -> Option<String> {
    (!v.is_null()).then(|| v.to_string())
}

/// `substr(s, start, length)` over characters. Positions count from 1; a
/// negative start counts from the end, and a negative length takes the
/// characters before `start` instead of after it.
fn substr(s: &str, start: i64, length: Option<i64>) -> String {
    let (mut p1, mut p2) = (start, length.map_or(i64::MAX, i64::saturating_abs));
    let negative_length = length.is_some_and(|l| l < 0);
    if p1 < 0 {
        p1 = p1.saturating_add(s.chars().count() as i64);
        if p1 < 0 {
            p2 = p2.saturating_add(p1).max(0);
            p1 = 0;
        }
    } else if p1 > 0 {
        p1 -= 1;
    } else if p2 > 0 {
        p2 -= 1;
    }
    if negative_length {
        p1 -= p2;
        if p1 < 0 {
            p2 += p1;
            p1 = 0;
        }
    }
    s.chars().skip(p1 as usize).take(p2 as usize).collect()
}

/// `round(r, digits)`: halves round away from zero, judged on the exact
/// decimal expansion of the float, as SQLite's printf does.
fn round(r: f64, digits: usize) -> f64 {
    // Beyond 2^52 a double has no fractional part to round.
    if !(-4_503_599_627_370_496.0..=4_503_599_627_370_496.0).contains(&r) {
        return r;
    }
    if digits == 0 {
        return ((r + if r < 0.0 { -0.5 } else { 0.5 }) as i64) as f64;
    }
    // Far more digits than needed, so Rust's own rounding at the last one
    // cannot reach the digit that decides.
    let exact = format!("{:.*}", digits + 40, r.abs());
    let (whole, fraction) = exact.split_once('.').unwrap_or((&exact, ""));
    let mut kept: Vec<u8> = whole.bytes().chain(fraction.bytes().take(digits)).collect();
    let mut whole_len = whole.len();
    if fraction.as_bytes().get(digits).is_some_and(|d| *d >= b'5') {
        let mut i = kept.len();
        loop {
            if i == 0 {
                kept.insert(0, b'1');
                whole_len += 1;
                break;
            }
            i -= 1;
            if kept[i] == b'9' {
                kept[i] = b'0';
            } else {
                kept[i] += 1;
                break;
            }
        }
    }
    let rounded = format!(
        "{}.{}",
        String::from_utf8_lossy(&kept[..whole_len]),
        String::from_utf8_lossy(&kept[whole_len..])
    );
    let magnitude: f64 = rounded.parse().unwrap_or(r.abs());
    if r < 0.0 { -magnitude } else { magnitude }
}

/// Functions SQLite has that Fishbowl does not implement yet. Calling one is
/// "not supported yet"; calling a name not on this list is "no such function".
pub fn is_sqlite_function(name: &str) -> bool {
    const NAMES: &[&str] = &[
        "changes",
        "char",
        "concat",
        "concat_ws",
        "format",
        "glob",
        "hex",
        "if",
        "last_insert_rowid",
        "like",
        "likelihood",
        "likely",
        "load_extension",
        "octet_length",
        "printf",
        "quote",
        "random",
        "randomblob",
        "sign",
        "soundex",
        "total_changes",
        "unhex",
        "unicode",
        "unistr",
        "unlikely",
        "zeroblob",
        "date",
        "time",
        "datetime",
        "julianday",
        "unixepoch",
        "strftime",
        "timediff",
        "acos",
        "acosh",
        "asin",
        "asinh",
        "atan",
        "atan2",
        "atanh",
        "ceil",
        "ceiling",
        "cos",
        "cosh",
        "degrees",
        "exp",
        "floor",
        "ln",
        "log",
        "log10",
        "log2",
        "mod",
        "pi",
        "pow",
        "power",
        "radians",
        "sin",
        "sinh",
        "sqrt",
        "tan",
        "tanh",
        "trunc",
        "string_agg",
        "median",
        "percentile",
        "percentile_cont",
        "percentile_disc",
        "row_number",
        "rank",
        "dense_rank",
        "percent_rank",
        "cume_dist",
        "ntile",
        "lag",
        "lead",
        "first_value",
        "last_value",
        "nth_value",
    ];
    NAMES.contains(&name) || name.starts_with("json") || name.starts_with("sqlite_")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(func: ScalarFunc, args: &[Value]) -> Value {
        func.call(args).unwrap()
    }

    fn text(s: &str) -> Value {
        Value::Text(s.into())
    }

    #[test]
    fn substr_handles_negative_and_zero_positions() {
        assert_eq!(substr("hello", 2, Some(3)), "ell");
        assert_eq!(substr("hello", -3, None), "llo");
        assert_eq!(substr("hello", 0, Some(2)), "h");
        assert_eq!(substr("hello", 3, Some(-2)), "he");
        assert_eq!(substr("hello", -2, Some(-2)), "el");
        assert_eq!(substr("hello", -10, Some(3)), "");
    }

    #[test]
    fn round_goes_half_away_from_zero() {
        assert_eq!(round(2.5, 0), 3.0);
        assert_eq!(round(-2.5, 0), -3.0);
        assert_eq!(round(0.125, 2), 0.13);
        // 2.675 is stored as 2.67499999..., so it rounds down.
        assert_eq!(round(2.675, 2), 2.67);
        assert_eq!(round(9.995, 2), 9.99);
        assert_eq!(round(9.9951, 2), 10.0);
    }

    #[test]
    fn abs_and_nullif_follow_sqlite() {
        assert_eq!(call(ScalarFunc::Abs, &[text("-5")]), Value::Real(5.0));
        assert!(matches!(
            ScalarFunc::Abs.call(&[Value::Integer(i64::MIN)]),
            Err(Error::Runtime(_))
        ));
        // nullif compares without affinity: 1 and '1' differ.
        assert_eq!(
            call(ScalarFunc::NullIf, &[Value::Integer(1), text("1")]),
            Value::Integer(1)
        );
        assert_eq!(
            call(ScalarFunc::NullIf, &[Value::Integer(1), Value::Real(1.0)]),
            Value::Null
        );
    }

    #[test]
    fn scalar_min_max_return_null_if_any_argument_is_null() {
        let args = [Value::Integer(1), text("a"), Value::Integer(2)];
        assert_eq!(call(ScalarFunc::Max, &args), text("a"));
        assert_eq!(call(ScalarFunc::Min, &args), Value::Integer(1));
        assert_eq!(
            call(ScalarFunc::Min, &[Value::Integer(1), Value::Null]),
            Value::Null
        );
    }
}
