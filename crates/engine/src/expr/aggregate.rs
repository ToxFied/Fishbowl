//! Aggregate functions such as `count()` and `sum()`: they see every row of a
//! group and return one value for the whole group. Each group gets its own
//! [`Accumulator`], which takes one row at a time and is finished at the end.
//! See <https://www.sqlite.org/lang_aggfunc.html>.

use std::collections::BTreeSet;
use std::fmt;

use crate::error::{Error, Result};
use crate::value::{Value, parse_numeric};

use super::{Expr, RowKey, comma_list};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateFunc {
    Count,
    Sum,
    Total,
    Avg,
    Min,
    Max,
    GroupConcat,
}

impl AggregateFunc {
    /// Looks up an aggregate by its lowercase name.
    pub fn from_name(name: &str) -> Option<AggregateFunc> {
        use AggregateFunc::*;
        Some(match name {
            "count" => Count,
            "sum" => Sum,
            "total" => Total,
            "avg" => Avg,
            "min" => Min,
            "max" => Max,
            "group_concat" => GroupConcat,
            _ => return None,
        })
    }

    /// Whether the aggregate takes `n` arguments. `count()` with none counts
    /// rows, like `count(*)`.
    pub fn accepts(self, n: usize) -> bool {
        match self {
            AggregateFunc::Count => n <= 1,
            AggregateFunc::GroupConcat => n == 1 || n == 2,
            _ => n == 1,
        }
    }

    pub fn is_min_max(self) -> bool {
        matches!(self, AggregateFunc::Min | AggregateFunc::Max)
    }

    pub fn name(self) -> &'static str {
        use AggregateFunc::*;
        match self {
            Count => "count",
            Sum => "sum",
            Total => "total",
            Avg => "avg",
            Min => "min",
            Max => "max",
            GroupConcat => "group_concat",
        }
    }
}

/// One aggregate in a query, such as `sum(DISTINCT price)`.
#[derive(Debug, Clone, PartialEq)]
pub struct AggregateCall {
    pub func: AggregateFunc,
    /// `count(DISTINCT x)`: each distinct value counts once.
    pub distinct: bool,
    /// Empty for `count(*)`.
    pub args: Vec<Expr>,
}

impl fmt::Display for AggregateCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.args.is_empty() {
            return write!(f, "{}(*)", self.func.name());
        }
        write!(f, "{}(", self.func.name())?;
        if self.distinct {
            f.write_str("DISTINCT ")?;
        }
        comma_list(f, &self.args)?;
        f.write_str(")")
    }
}

/// The running state of one aggregate for one group.
#[derive(Debug, Clone)]
pub struct Accumulator {
    func: AggregateFunc,
    /// Values already seen, for DISTINCT aggregates.
    seen: Option<BTreeSet<RowKey>>,
    state: State,
}

#[derive(Debug, Clone)]
enum State {
    Count(i64),
    Sum(RunningSum),
    /// The smallest (min) or largest (max) value so far.
    Best(Option<Value>),
    Concat(Option<String>),
}

impl Accumulator {
    pub fn new(call: &AggregateCall) -> Accumulator {
        use AggregateFunc::*;
        let state = match call.func {
            Count => State::Count(0),
            Sum | Total | Avg => State::Sum(RunningSum::default()),
            Min | Max => State::Best(None),
            GroupConcat => State::Concat(None),
        };
        Accumulator {
            func: call.func,
            seen: call.distinct.then(BTreeSet::new),
            state,
        }
    }

    /// Feeds one row's argument values. For `min()` and `max()` the result
    /// says whether this row became the group's chosen row, which is where
    /// SQLite takes plain columns from (`SELECT name, max(weight)`).
    pub fn step(&mut self, args: &[Value]) -> bool {
        let Some(value) = args.first() else {
            // count(*): every row counts.
            if let State::Count(n) = &mut self.state {
                *n += 1;
            }
            return false;
        };
        if let Some(seen) = &mut self.seen
            && !value.is_null()
            && !seen.insert(RowKey(vec![value.clone()]))
        {
            return false;
        }
        match &mut self.state {
            State::Count(n) => {
                if !value.is_null() {
                    *n += 1;
                }
                false
            }
            State::Sum(sum) => {
                sum.add(value);
                false
            }
            State::Best(best) => match (best.as_ref(), value.is_null()) {
                // Until a value arrives, every row is the chosen row.
                (None, true) => true,
                (Some(_), true) => false,
                (None, false) => {
                    *best = Some(value.clone());
                    true
                }
                (Some(current), false) => {
                    let ord = current.compare(value);
                    let better = match self.func {
                        AggregateFunc::Max => ord.is_lt(),
                        _ => ord.is_gt(),
                    };
                    if better {
                        *best = Some(value.clone());
                    }
                    better
                }
            },
            State::Concat(text) => {
                if value.is_null() {
                    return false;
                }
                let separator = match args.get(1) {
                    None => ",".to_string(),
                    Some(Value::Null) => String::new(),
                    Some(sep) => sep.to_string(),
                };
                match text {
                    Some(text) => {
                        text.push_str(&separator);
                        text.push_str(&value.to_string());
                    }
                    None => *text = Some(value.to_string()),
                }
                false
            }
        }
    }

    pub fn finish(&self) -> Result<Value> {
        Ok(match &self.state {
            State::Count(n) => Value::Integer(*n),
            State::Sum(sum) => match self.func {
                AggregateFunc::Total => real(sum.total()),
                _ if sum.count == 0 => Value::Null,
                AggregateFunc::Avg => real(sum.total() / sum.count as f64),
                _ if sum.overflow => return Err(Error::Runtime("integer overflow".into())),
                _ if sum.approx => real(sum.total()),
                _ => Value::Integer(sum.int_sum),
            },
            State::Best(best) => best.clone().unwrap_or(Value::Null),
            State::Concat(text) => text.clone().map_or(Value::Null, Value::Text),
        })
    }
}

/// A floating-point result. SQLite stores NaN as NULL.
fn real(f: f64) -> Value {
    if f.is_nan() {
        Value::Null
    } else {
        Value::Real(f)
    }
}

/// SQLite's sum: exact in 64-bit integers while every value is an integer,
/// then floating point with Kahan-Babuska-Neumaier error compensation.
#[derive(Debug, Clone, Default)]
struct RunningSum {
    /// Non-NULL values added.
    count: i64,
    int_sum: i64,
    /// Whether the sum has moved to floating point.
    approx: bool,
    /// The integer sum overflowed and no real has arrived since.
    overflow: bool,
    sum: f64,
    /// Rounding error collected so far, added back at the end.
    err: f64,
}

/// Integers this large lose precision as doubles, so they are added in two parts.
const BIG: i64 = 4_503_599_627_370_496;

impl RunningSum {
    fn add(&mut self, value: &Value) {
        // Text that looks like a number counts as that number; other text
        // counts as a real through its numeric prefix.
        let value = match value {
            Value::Null => return,
            Value::Text(s) => parse_numeric(s).unwrap_or_else(|| value.clone()),
            other => other.clone(),
        };
        self.count += 1;
        match value {
            Value::Integer(i) if !self.approx => match self.int_sum.checked_add(i) {
                Some(sum) => self.int_sum = sum,
                None => {
                    self.overflow = true;
                    self.start_approx();
                    self.add_integer(i);
                }
            },
            Value::Integer(i) => self.add_integer(i),
            other => {
                if self.approx {
                    self.overflow = false;
                } else {
                    self.start_approx();
                }
                self.add_real(other.to_f64_lossy());
            }
        }
    }

    fn start_approx(&mut self) {
        self.approx = true;
        let i = self.int_sum;
        if i <= -BIG || i >= BIG {
            let small = i % 16384;
            self.sum = (i - small) as f64;
            self.err = small as f64;
        } else {
            self.sum = i as f64;
            self.err = 0.0;
        }
    }

    fn add_integer(&mut self, i: i64) {
        if i <= -BIG || i >= BIG {
            let small = i % 16384;
            self.add_real((i - small) as f64);
            self.add_real(small as f64);
        } else {
            self.add_real(i as f64);
        }
    }

    fn add_real(&mut self, r: f64) {
        let s = self.sum;
        let t = s + r;
        if s.abs() > r.abs() {
            self.err += (s - t) + r;
        } else {
            self.err += (r - t) + s;
        }
        self.sum = t;
    }

    /// The sum as a double, which is what total() and avg() report.
    fn total(&self) -> f64 {
        match (self.approx, self.err.is_finite()) {
            (false, _) => self.int_sum as f64,
            (true, true) => self.sum + self.err,
            (true, false) => self.sum,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(func: AggregateFunc, values: &[Value]) -> Result<Value> {
        let call = AggregateCall {
            func,
            distinct: false,
            args: vec![Expr::Literal(Value::Null)],
        };
        let mut acc = Accumulator::new(&call);
        for v in values {
            acc.step(std::slice::from_ref(v));
        }
        acc.finish()
    }

    #[test]
    fn sum_stays_integer_until_a_real_arrives() {
        let ints = [Value::Integer(1), Value::Integer(2), Value::Null];
        assert_eq!(run(AggregateFunc::Sum, &ints), Ok(Value::Integer(3)));
        assert_eq!(run(AggregateFunc::Avg, &ints), Ok(Value::Real(1.5)));
        let mixed = [
            Value::Integer(1),
            Value::Real(2.5),
            Value::Text("3".into()),
            Value::Text("abc".into()),
        ];
        assert_eq!(run(AggregateFunc::Sum, &mixed), Ok(Value::Real(6.5)));
        assert_eq!(run(AggregateFunc::Avg, &mixed), Ok(Value::Real(1.625)));
    }

    #[test]
    fn empty_input_gives_null_except_count_and_total() {
        assert_eq!(run(AggregateFunc::Sum, &[Value::Null]), Ok(Value::Null));
        assert_eq!(run(AggregateFunc::Avg, &[]), Ok(Value::Null));
        assert_eq!(run(AggregateFunc::Max, &[]), Ok(Value::Null));
        assert_eq!(run(AggregateFunc::Total, &[]), Ok(Value::Real(0.0)));
        assert_eq!(
            run(AggregateFunc::Count, &[Value::Null]),
            Ok(Value::Integer(0))
        );
    }

    #[test]
    fn integer_overflow_in_sum_is_an_error() {
        let big = [Value::Integer(i64::MAX), Value::Integer(1)];
        assert!(matches!(
            run(AggregateFunc::Sum, &big),
            Err(Error::Runtime(_))
        ));
        // total() and avg() switch to floating point instead.
        assert!(run(AggregateFunc::Total, &big).is_ok());
    }

    #[test]
    fn min_and_max_skip_nulls_and_order_numbers_before_text() {
        let values = [
            Value::Null,
            Value::Text("a".into()),
            Value::Integer(2),
            Value::Real(1.5),
        ];
        assert_eq!(run(AggregateFunc::Min, &values), Ok(Value::Real(1.5)));
        assert_eq!(
            run(AggregateFunc::Max, &values),
            Ok(Value::Text("a".into()))
        );
    }
}
