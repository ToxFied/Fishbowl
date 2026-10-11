//! Bound expressions: SQL expressions whose column names have already been
//! resolved to positions in a row, so evaluating them is just arithmetic.

mod aggregate;
mod functions;

use std::cmp::Ordering;
use std::fmt;
use std::rc::Rc;

pub use aggregate::{Accumulator, AggregateCall, AggregateFunc};
pub use functions::{ScalarFunc, is_sqlite_function};

use crate::error::{Result, unsupported};
use crate::plan::Subquery;
use crate::value::{Affinity, Value, numeric_prefix};

/// What an expression can reach beyond its own row: the rows of the queries
/// it is nested in, and the executor, which runs its subqueries.
pub trait Env {
    /// Column `index` of the row being processed `depth` queries out.
    fn outer(&self, depth: usize, index: usize) -> Result<Value>;
    /// Runs a subquery for `row` and returns its first column: every row
    /// when `all_rows` is set, otherwise at most the first.
    fn subquery(&mut self, query: &Subquery, row: &[Value], all_rows: bool) -> Result<Rc<[Value]>>;
}

/// The environment outside a query, where no expression has a subquery or an
/// outer column. The planner makes sure of that.
struct NoEnv;

impl Env for NoEnv {
    fn outer(&self, _: usize, _: usize) -> Result<Value> {
        unsupported("a column of an enclosing query here")
    }

    fn subquery(&mut self, _: &Subquery, _: &[Value], _: bool) -> Result<Rc<[Value]>> {
        unsupported("a subquery here")
    }
}

/// How a subquery's rows become one value.
#[derive(Debug, Clone, PartialEq)]
pub enum SubqueryKind {
    /// `(SELECT ...)`: the first column of the first row, or NULL.
    Scalar,
    /// `[NOT] EXISTS (SELECT ...)`: whether there is any row.
    Exists { negated: bool },
    /// `expr [NOT] IN (SELECT ...)`.
    In { expr: Box<Expr>, negated: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
    Concat,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
}

impl BinaryOp {
    fn is_comparison(self) -> bool {
        use BinaryOp::*;
        matches!(self, Eq | NotEq | Lt | LtEq | Gt | GtEq)
    }

    /// Whether a comparison operator holds for two values in this order.
    fn holds(self, ord: Ordering) -> bool {
        use BinaryOp::*;
        match self {
            Eq => ord.is_eq(),
            NotEq => ord.is_ne(),
            Lt => ord.is_lt(),
            LtEq => ord.is_le(),
            Gt => ord.is_gt(),
            _ => ord.is_ge(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Negate,
    Plus,
    Not,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(Value),
    /// A column, by its position in the input row. The name is for display.
    /// The affinity is the column's, and steers comparisons.
    Column {
        index: usize,
        name: String,
        affinity: Affinity,
    },
    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    Binary {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    /// `left IS [NOT] DISTINCT FROM right`: like `=`, but NULL equals NULL and
    /// the answer is never NULL. `distinct` is true for `IS DISTINCT FROM`.
    Is {
        left: Box<Expr>,
        right: Box<Expr>,
        distinct: bool,
    },
    /// `expr IS [NOT] TRUE` or `expr IS [NOT] FALSE`: a truth test that is
    /// never NULL. `expr IS TRUE` holds when `expr` is a non-zero number.
    IsTruth {
        expr: Box<Expr>,
        truth: bool,
        negated: bool,
    },
    /// `expr [NOT] BETWEEN low AND high`, which means `expr >= low AND expr <= high`.
    Between {
        expr: Box<Expr>,
        low: Box<Expr>,
        high: Box<Expr>,
        negated: bool,
    },
    /// `expr [NOT] IN (a, b, ...)`.
    InList {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    /// `CASE [operand] WHEN .. THEN .. [ELSE ..] END`. With an operand, each
    /// WHEN is compared to it; without one, each WHEN is a condition.
    Case {
        operand: Option<Box<Expr>>,
        branches: Vec<(Expr, Expr)>,
        otherwise: Option<Box<Expr>>,
    },
    /// `CAST(expr AS type)`. The type name is kept for display; the affinity
    /// derived from it decides the conversion.
    Cast {
        expr: Box<Expr>,
        type_name: String,
        affinity: Affinity,
    },
    Function {
        func: ScalarFunc,
        args: Vec<Expr>,
    },
    /// A column of an enclosing query's row, read by a correlated subquery.
    /// `depth` counts queries outward: 1 is the query just outside.
    OuterColumn {
        depth: usize,
        index: usize,
        name: String,
        affinity: Affinity,
    },
    Subquery {
        kind: SubqueryKind,
        query: Box<Subquery>,
    },
}

impl Expr {
    /// Evaluates an expression that has no subqueries or outer columns.
    pub fn eval(&self, row: &[Value]) -> Result<Value> {
        self.eval_with(row, &mut NoEnv)
    }

    pub fn eval_with(&self, row: &[Value], env: &mut dyn Env) -> Result<Value> {
        Ok(match self {
            Expr::Literal(v) => v.clone(),
            Expr::Column { index, .. } => row.get(*index).cloned().unwrap_or(Value::Null),
            Expr::Unary { op, expr } => eval_unary(*op, expr.eval_with(row, env)?),
            Expr::Binary { op, left, right } => {
                let l = left.eval_with(row, env)?;
                // AND and OR can skip the right side when the left decides it.
                match (op, l.truthiness()) {
                    (BinaryOp::And, Some(false)) => return Ok(Value::Integer(0)),
                    (BinaryOp::Or, Some(true)) => return Ok(Value::Integer(1)),
                    _ => {}
                }
                let r = right.eval_with(row, env)?;
                if op.is_comparison() {
                    compare(l, left.affinity(), r, right.affinity())
                        .map_or(Value::Null, |ord| bool_value(op.holds(ord)))
                } else {
                    eval_binary(*op, l, r)
                }
            }
            Expr::IsNull { expr, negated } => {
                bool_value(expr.eval_with(row, env)?.is_null() != *negated)
            }
            Expr::Is {
                left,
                right,
                distinct,
            } => {
                let (l, r) = (left.eval_with(row, env)?, right.eval_with(row, env)?);
                let same = match (l.is_null(), r.is_null()) {
                    (true, true) => true,
                    (true, false) | (false, true) => false,
                    _ => compare(l, left.affinity(), r, right.affinity()) == Some(Ordering::Equal),
                };
                bool_value(same != *distinct)
            }
            Expr::IsTruth {
                expr,
                truth,
                negated,
            } => bool_value((expr.eval_with(row, env)?.truthiness() == Some(*truth)) != *negated),
            Expr::Between {
                expr,
                low,
                high,
                negated,
            } => {
                let v = expr.eval_with(row, env)?;
                let affinity = expr.affinity();
                let above = compare(
                    v.clone(),
                    affinity,
                    low.eval_with(row, env)?,
                    low.affinity(),
                )
                .map(Ordering::is_ge);
                let below = compare(v, affinity, high.eval_with(row, env)?, high.affinity())
                    .map(Ordering::is_le);
                let inside = match (above, below) {
                    (Some(false), _) | (_, Some(false)) => Some(false),
                    (Some(true), Some(true)) => Some(true),
                    _ => None,
                };
                inside.map_or(Value::Null, |b| bool_value(b != *negated))
            }
            Expr::InList {
                expr,
                list,
                negated,
            } => eval_in_list(expr, list, row, env)?
                .map_or(Value::Null, |b| bool_value(b != *negated)),
            Expr::Case {
                operand,
                branches,
                otherwise,
            } => {
                let base = match operand {
                    Some(operand) => Some((operand.eval_with(row, env)?, operand.affinity())),
                    None => None,
                };
                for (when, then) in branches {
                    let w = when.eval_with(row, env)?;
                    let matched = match &base {
                        Some((b, affinity)) => {
                            compare(b.clone(), *affinity, w, when.affinity())
                                == Some(Ordering::Equal)
                        }
                        None => w.truthiness() == Some(true),
                    };
                    if matched {
                        return then.eval_with(row, env);
                    }
                }
                match otherwise {
                    Some(e) => e.eval_with(row, env)?,
                    None => Value::Null,
                }
            }
            Expr::Cast { expr, affinity, .. } => cast(expr.eval_with(row, env)?, *affinity),
            Expr::Function { func, args } => func.eval(args, row, env)?,
            Expr::OuterColumn { depth, index, .. } => env.outer(*depth, *index)?,
            Expr::Subquery { kind, query } => match kind {
                SubqueryKind::Scalar => {
                    let values = env.subquery(query, row, false)?;
                    values.first().cloned().unwrap_or(Value::Null)
                }
                SubqueryKind::Exists { negated } => {
                    let values = env.subquery(query, row, false)?;
                    bool_value(values.is_empty() == *negated)
                }
                SubqueryKind::In { expr, negated } => {
                    let v = expr.eval_with(row, env)?;
                    let values = env.subquery(query, row, true)?;
                    let affinity = query.plan.affinities.first().copied().flatten();
                    in_values(v, expr.affinity(), &values, affinity)
                        .map_or(Value::Null, |b| bool_value(b != *negated))
                }
            },
        })
    }

    /// The affinity an expression brings to a comparison. Only a plain column
    /// and a CAST have one; everything else, even `+column`, has none.
    /// See <https://www.sqlite.org/datatype3.html#affinity_of_expressions>.
    pub fn affinity(&self) -> Option<Affinity> {
        match self {
            Expr::Column { affinity, .. }
            | Expr::OuterColumn { affinity, .. }
            | Expr::Cast { affinity, .. } => Some(*affinity),
            // A scalar subquery has the affinity of its result column.
            Expr::Subquery {
                kind: SubqueryKind::Scalar,
                query,
            } => query.plan.affinities.first().copied().flatten(),
            _ => None,
        }
    }

    /// Calls `f` on this expression and every expression inside it. It does
    /// not enter a subquery's plan.
    pub fn walk<'e>(&'e self, f: &mut impl FnMut(&'e Expr)) {
        f(self);
        match self {
            Expr::Literal(_) | Expr::Column { .. } | Expr::OuterColumn { .. } => {}
            Expr::Subquery { kind, .. } => {
                if let SubqueryKind::In { expr, .. } = kind {
                    expr.walk(f);
                }
            }
            Expr::Unary { expr, .. }
            | Expr::IsNull { expr, .. }
            | Expr::IsTruth { expr, .. }
            | Expr::Cast { expr, .. } => expr.walk(f),
            Expr::Binary { left, right, .. } | Expr::Is { left, right, .. } => {
                left.walk(f);
                right.walk(f);
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                expr.walk(f);
                low.walk(f);
                high.walk(f);
            }
            Expr::InList { expr, list, .. } => {
                expr.walk(f);
                list.iter().for_each(|e| e.walk(f));
            }
            Expr::Case {
                operand,
                branches,
                otherwise,
            } => {
                operand.iter().for_each(|e| e.walk(f));
                for (when, then) in branches {
                    when.walk(f);
                    then.walk(f);
                }
                otherwise.iter().for_each(|e| e.walk(f));
            }
            Expr::Function { args, .. } => args.iter().for_each(|e| e.walk(f)),
        }
    }
}

impl Expr {
    /// Moves every column reference `by` positions to the left, for an
    /// expression that moves to a step whose rows hold fewer columns.
    pub fn shift_columns(&mut self, by: usize) {
        match self {
            Expr::Column { index, .. } => *index -= by,
            Expr::Literal(_) | Expr::OuterColumn { .. } => {}
            Expr::Unary { expr, .. }
            | Expr::IsNull { expr, .. }
            | Expr::IsTruth { expr, .. }
            | Expr::Cast { expr, .. } => expr.shift_columns(by),
            Expr::Binary { left, right, .. } | Expr::Is { left, right, .. } => {
                left.shift_columns(by);
                right.shift_columns(by);
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                for e in [expr, low, high] {
                    e.shift_columns(by);
                }
            }
            Expr::InList { expr, list, .. } => {
                expr.shift_columns(by);
                list.iter_mut().for_each(|e| e.shift_columns(by));
            }
            Expr::Case {
                operand,
                branches,
                otherwise,
            } => {
                for e in operand.iter_mut().chain(otherwise.iter_mut()) {
                    e.shift_columns(by);
                }
                for (when, then) in branches {
                    when.shift_columns(by);
                    then.shift_columns(by);
                }
            }
            Expr::Function { args, .. } => args.iter_mut().for_each(|e| e.shift_columns(by)),
            // A subquery reads this query's columns as outer columns, which
            // point into the whole row; the planner never moves one.
            Expr::Subquery { .. } => unreachable!("expressions with subqueries stay in place"),
        }
    }
}

/// Compares two values the way `=`, `<` and friends do. First both sides may
/// be converted, following SQLite's comparison affinity rules:
///
/// - Two columns: if either is numeric (INTEGER, REAL, NUMERIC), text that
///   looks like a number becomes a number. Otherwise nothing changes, so a
///   TEXT column and an untyped (BLOB) column compare values as stored.
/// - A column and an expression: the column's affinity applies to both.
/// - Two expressions: nothing changes.
///
/// Then NULL makes the answer unknown (`None`).
pub fn compare(
    l: Value,
    l_affinity: Option<Affinity>,
    r: Value,
    r_affinity: Option<Affinity>,
) -> Option<Ordering> {
    if l.is_null() || r.is_null() {
        return None;
    }
    let numeric = |a| matches!(a, Affinity::Integer | Affinity::Real | Affinity::Numeric);
    let affinity = match (l_affinity, r_affinity) {
        (Some(a), Some(b)) => (numeric(a) || numeric(b)).then_some(Affinity::Numeric),
        (Some(a), None) | (None, Some(a)) => Some(a),
        (None, None) => None,
    };
    let (l, r) = match affinity {
        Some(a) if numeric(a) => (
            l.apply_affinity(Affinity::Numeric),
            r.apply_affinity(Affinity::Numeric),
        ),
        // Text affinity turns numbers into text, but only when the other side
        // is text already.
        Some(Affinity::Text) if matches!(l, Value::Text(_)) || matches!(r, Value::Text(_)) => (
            l.apply_affinity(Affinity::Text),
            r.apply_affinity(Affinity::Text),
        ),
        _ => (l, r),
    };
    Some(l.compare(&r))
}

/// `x IN (list)` as three-valued logic. The list values have no affinity of
/// their own, so `x`'s affinity decides: SQLite treats `x IN (a, b)` as
/// `x = +a OR x = +b`.
fn eval_in_list(
    expr: &Expr,
    list: &[Expr],
    row: &[Value],
    env: &mut dyn Env,
) -> Result<Option<bool>> {
    // An empty list contains nothing, not even NULL.
    if list.is_empty() {
        return Ok(Some(false));
    }
    let v = expr.eval_with(row, env)?;
    if v.is_null() {
        return Ok(None);
    }
    let affinity = expr.affinity();
    let mut saw_null = false;
    for item in list {
        match compare(v.clone(), affinity, item.eval_with(row, env)?, None) {
            Some(Ordering::Equal) => return Ok(Some(true)),
            None => saw_null = true,
            Some(_) => {}
        }
    }
    // Not found, but a NULL in the list might have been equal: unknown.
    Ok(if saw_null { None } else { Some(false) })
}

/// `x IN (SELECT y ...)` as three-valued logic. Unlike a list, the subquery's
/// column has an affinity, and `x = y` decides as it would in a comparison.
fn in_values(
    v: Value,
    affinity: Option<Affinity>,
    values: &[Value],
    values_affinity: Option<Affinity>,
) -> Option<bool> {
    // No rows contain nothing, not even NULL.
    if values.is_empty() {
        return Some(false);
    }
    if v.is_null() {
        return None;
    }
    let mut saw_null = false;
    for value in values {
        match compare(v.clone(), affinity, value.clone(), values_affinity) {
            Some(Ordering::Equal) => return Some(true),
            None => saw_null = true,
            Some(_) => {}
        }
    }
    if saw_null { None } else { Some(false) }
}

/// `CAST(v AS type)`, where the type has already been reduced to an affinity.
pub fn cast(v: Value, affinity: Affinity) -> Value {
    match (affinity, v) {
        (_, Value::Null) => Value::Null,
        (Affinity::Text, v) => Value::Text(v.to_string()),
        (Affinity::Integer, v) => Value::Integer(to_integer(&v)),
        (Affinity::Real, v) => Value::Real(v.to_f64_lossy()),
        (Affinity::Numeric, Value::Text(s)) => match numeric_prefix(&s) {
            Value::Real(f) => small_integer(f).map_or(Value::Real(f), Value::Integer),
            other => other,
        },
        (Affinity::Numeric | Affinity::Blob, v) => v,
    }
}

/// A real with no fractional part, below 2^51 in size, as an integer. This is
/// the range where SQLite's CAST to NUMERIC prefers an integer.
fn small_integer(f: f64) -> Option<i64> {
    const LIMIT: f64 = 2_251_799_813_685_248.0;
    (f.fract() == 0.0 && (-LIMIT..LIMIT).contains(&f)).then_some(f as i64)
}

/// A value as an integer, the way SQLite reads an integer argument: reals
/// truncate toward zero, and text uses its leading digits, so `'12abc'` is
/// 12 and `'1e3'` is 1. Too many digits saturate.
pub fn to_integer(v: &Value) -> i64 {
    match v {
        Value::Text(s) => integer_prefix(s),
        other => other.to_i64_lossy(),
    }
}

fn integer_prefix(s: &str) -> i64 {
    let t = s.trim_start();
    let (negative, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let mut n: i64 = 0;
    for d in digits.bytes().take_while(u8::is_ascii_digit) {
        let d = i64::from(d - b'0');
        let next = n.checked_mul(10).and_then(|n| {
            if negative {
                n.checked_sub(d)
            } else {
                n.checked_add(d)
            }
        });
        match next {
            Some(next) => n = next,
            None => return if negative { i64::MIN } else { i64::MAX },
        }
    }
    n
}

fn bool_value(b: bool) -> Value {
    Value::Integer(i64::from(b))
}

fn eval_unary(op: UnaryOp, v: Value) -> Value {
    match (op, v) {
        (_, Value::Null) => Value::Null,
        (UnaryOp::Plus, v) => v,
        (UnaryOp::Not, v) => v.truthiness().map_or(Value::Null, |b| bool_value(!b)),
        (UnaryOp::Negate, Value::Integer(i)) => i
            .checked_neg()
            .map_or(Value::Real(-(i as f64)), Value::Integer),
        (UnaryOp::Negate, Value::Real(f)) => Value::Real(-f),
        (UnaryOp::Negate, Value::Text(s)) => eval_unary(UnaryOp::Negate, numeric_prefix(&s)),
    }
}

/// Text takes part in arithmetic through its numeric prefix.
fn to_number(v: Value) -> Value {
    match v {
        Value::Text(s) => numeric_prefix(&s),
        other => other,
    }
}

/// Logic, concatenation and arithmetic. Comparisons are handled by [`compare`],
/// because they need to know the affinity of each side.
fn eval_binary(op: BinaryOp, l: Value, r: Value) -> Value {
    use BinaryOp::*;
    match op {
        And => match (l.truthiness(), r.truthiness()) {
            (Some(false), _) | (_, Some(false)) => bool_value(false),
            (Some(true), Some(true)) => bool_value(true),
            _ => Value::Null,
        },
        Or => match (l.truthiness(), r.truthiness()) {
            (Some(true), _) | (_, Some(true)) => bool_value(true),
            (Some(false), Some(false)) => bool_value(false),
            _ => Value::Null,
        },
        _ if l.is_null() || r.is_null() => Value::Null,
        Concat => Value::Text(format!("{l}{r}")),
        Eq | NotEq | Lt | LtEq | Gt | GtEq => {
            compare(l, None, r, None).map_or(Value::Null, |ord| bool_value(op.holds(ord)))
        }
        Add | Subtract | Multiply | Divide | Remainder => arithmetic(op, l, r),
    }
}

fn arithmetic(op: BinaryOp, l: Value, r: Value) -> Value {
    use BinaryOp::*;
    if op == Remainder {
        return remainder(l, r);
    }
    let (l, r) = (to_number(l), to_number(r));
    if let (Value::Integer(a), Value::Integer(b)) = (&l, &r) {
        let exact = match op {
            Add => a.checked_add(*b),
            Subtract => a.checked_sub(*b),
            Multiply => a.checked_mul(*b),
            _ if *b == 0 => return Value::Null,
            _ => a.checked_div(*b),
        };
        // On overflow SQLite falls back to floating point.
        if let Some(i) = exact {
            return Value::Integer(i);
        }
    }
    let (a, b) = (l.to_f64_lossy(), r.to_f64_lossy());
    let result = match op {
        Add => a + b,
        Subtract => a - b,
        Multiply => a * b,
        _ if b == 0.0 => return Value::Null,
        _ => a / b,
    };
    if result.is_nan() {
        Value::Null
    } else {
        Value::Real(result)
    }
}

/// `l % r`. SQLite reads both sides as integers, text by its leading digits
/// (`'1e3'` is 1), and returns a real if either side was a real number.
fn remainder(l: Value, r: Value) -> Value {
    let (a, b) = (to_integer(&l), to_integer(&r));
    if b == 0 {
        return Value::Null;
    }
    let rem = a.checked_rem(b).unwrap_or(0);
    match (to_number(l), to_number(r)) {
        (Value::Integer(_), Value::Integer(_)) => Value::Integer(rem),
        _ => Value::Real(rem as f64),
    }
}

/// A row of values in the order ORDER BY, GROUP BY and DISTINCT use: NULL
/// first and equal to NULL, then numbers by value, then text byte by byte.
#[derive(Debug, Clone)]
pub struct RowKey(pub Vec<Value>);

impl Ord for RowKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0
            .iter()
            .zip(&other.0)
            .map(|(a, b)| a.compare(b))
            .find(|ord| ord.is_ne())
            .unwrap_or_else(|| self.0.len().cmp(&other.0.len()))
    }
}

impl PartialOrd for RowKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for RowKey {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for RowKey {}

impl fmt::Display for BinaryOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            BinaryOp::Add => "+",
            BinaryOp::Subtract => "-",
            BinaryOp::Multiply => "*",
            BinaryOp::Divide => "/",
            BinaryOp::Remainder => "%",
            BinaryOp::Concat => "||",
            BinaryOp::Eq => "=",
            BinaryOp::NotEq => "<>",
            BinaryOp::Lt => "<",
            BinaryOp::LtEq => "<=",
            BinaryOp::Gt => ">",
            BinaryOp::GtEq => ">=",
            BinaryOp::And => "AND",
            BinaryOp::Or => "OR",
        })
    }
}

/// Writes items separated by ", ".
pub(crate) fn comma_list<T: fmt::Display>(f: &mut fmt::Formatter<'_>, items: &[T]) -> fmt::Result {
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            f.write_str(", ")?;
        }
        write!(f, "{item}")?;
    }
    Ok(())
}

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let not = |negated: bool| if negated { "NOT " } else { "" };
        match self {
            Expr::Literal(Value::Text(s)) => write!(f, "'{}'", s.replace('\'', "''")),
            Expr::Literal(v) => write!(f, "{v}"),
            Expr::Column { name, .. } => f.write_str(name),
            Expr::Unary {
                op: UnaryOp::Not,
                expr,
            } => write!(f, "NOT {expr}"),
            Expr::Unary {
                op: UnaryOp::Negate,
                expr,
            } => write!(f, "-{expr}"),
            Expr::Unary {
                op: UnaryOp::Plus,
                expr,
            } => write!(f, "+{expr}"),
            Expr::Binary { op, left, right } => write!(f, "({left} {op} {right})"),
            Expr::IsNull { expr, negated } => write!(f, "{expr} IS {}NULL", not(*negated)),
            Expr::Is {
                left,
                right,
                distinct,
            } => write!(f, "{left} IS {}{right}", not(*distinct)),
            Expr::IsTruth {
                expr,
                truth,
                negated,
            } => write!(
                f,
                "{expr} IS {}{}",
                not(*negated),
                if *truth { "TRUE" } else { "FALSE" }
            ),
            Expr::Between {
                expr,
                low,
                high,
                negated,
            } => write!(f, "{expr} {}BETWEEN {low} AND {high}", not(*negated)),
            Expr::InList {
                expr,
                list,
                negated,
            } => {
                write!(f, "{expr} {}IN (", not(*negated))?;
                comma_list(f, list)?;
                f.write_str(")")
            }
            Expr::Case {
                operand,
                branches,
                otherwise,
            } => {
                f.write_str("CASE")?;
                if let Some(operand) = operand {
                    write!(f, " {operand}")?;
                }
                for (when, then) in branches {
                    write!(f, " WHEN {when} THEN {then}")?;
                }
                if let Some(otherwise) = otherwise {
                    write!(f, " ELSE {otherwise}")?;
                }
                f.write_str(" END")
            }
            Expr::Cast {
                expr, type_name, ..
            } => write!(f, "CAST({expr} AS {type_name})"),
            Expr::Function { func, args } => {
                write!(f, "{}(", func.name())?;
                comma_list(f, args)?;
                f.write_str(")")
            }
            Expr::OuterColumn { name, .. } => f.write_str(name),
            Expr::Subquery { kind, query } => match kind {
                SubqueryKind::Scalar => write!(f, "({})", query.sql),
                SubqueryKind::Exists { negated } => {
                    write!(f, "{}EXISTS ({})", not(*negated), query.sql)
                }
                SubqueryKind::In { expr, negated } => {
                    write!(f, "{expr} {}IN ({})", not(*negated), query.sql)
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(v: Value) -> Box<Expr> {
        Box::new(Expr::Literal(v))
    }

    fn bin(op: BinaryOp, l: Value, r: Value) -> Value {
        Expr::Binary {
            op,
            left: lit(l),
            right: lit(r),
        }
        .eval(&[])
        .unwrap()
    }

    fn column(affinity: Affinity) -> Box<Expr> {
        Box::new(Expr::Column {
            index: 0,
            name: "c".into(),
            affinity,
        })
    }

    #[test]
    fn null_makes_comparisons_unknown() {
        assert_eq!(bin(BinaryOp::Eq, Value::Null, Value::Null), Value::Null);
        assert_eq!(
            bin(BinaryOp::Lt, Value::Integer(1), Value::Null),
            Value::Null
        );
    }

    #[test]
    fn and_or_use_three_valued_logic() {
        let (t, f, n) = (Value::Integer(1), Value::Integer(0), Value::Null);
        assert_eq!(bin(BinaryOp::And, n.clone(), f.clone()), Value::Integer(0));
        assert_eq!(bin(BinaryOp::And, n.clone(), t.clone()), Value::Null);
        assert_eq!(bin(BinaryOp::Or, n.clone(), t), Value::Integer(1));
        assert_eq!(bin(BinaryOp::Or, n, f), Value::Null);
    }

    #[test]
    fn integer_arithmetic_matches_sqlite() {
        assert_eq!(
            bin(BinaryOp::Divide, Value::Integer(7), Value::Integer(2)),
            Value::Integer(3)
        );
        assert_eq!(
            bin(BinaryOp::Divide, Value::Integer(-7), Value::Integer(2)),
            Value::Integer(-3)
        );
        assert_eq!(
            bin(BinaryOp::Divide, Value::Integer(1), Value::Integer(0)),
            Value::Null
        );
        assert_eq!(
            bin(BinaryOp::Remainder, Value::Integer(-7), Value::Integer(3)),
            Value::Integer(-1)
        );
        assert_eq!(
            bin(BinaryOp::Add, Value::Integer(i64::MAX), Value::Integer(1)),
            Value::Real(i64::MAX as f64 + 1.0)
        );
        assert_eq!(
            bin(
                BinaryOp::Add,
                Value::Text("12abc".into()),
                Value::Integer(1)
            ),
            Value::Integer(13)
        );
    }

    #[test]
    fn comparisons_apply_affinity() {
        let (five, text_five) = (Value::Integer(5), Value::Text("5".into()));
        let eq = |l: &Value, la, r: &Value, ra| {
            compare(l.clone(), la, r.clone(), ra) == Some(Ordering::Equal)
        };
        let (integer, text, blob) = (
            Some(Affinity::Integer),
            Some(Affinity::Text),
            Some(Affinity::Blob),
        );
        // An INTEGER column turns '5' into 5, whatever the other side is.
        assert!(eq(&five, integer, &text_five, None));
        assert!(eq(&five, integer, &text_five, text));
        // A TEXT column turns the literal 5 into '5'...
        assert!(eq(&text_five, text, &five, None));
        // ...but not the 5 in an untyped column: both are columns.
        assert!(!eq(&text_five, text, &five, blob));
        // Two expressions compare as they are.
        assert!(!eq(&five, None, &text_five, None));
        // `+column` drops the column's affinity.
        let plus = Expr::Unary {
            op: UnaryOp::Plus,
            expr: column(Affinity::Integer),
        };
        assert_eq!(plus.affinity(), None);
    }

    #[test]
    fn in_list_follows_three_valued_logic() {
        let in_list = |v: Value, list: Vec<Value>| {
            Expr::InList {
                expr: lit(v),
                list: list.into_iter().map(Expr::Literal).collect(),
                negated: false,
            }
            .eval(&[])
            .unwrap()
        };
        let (one, two, null) = (Value::Integer(1), Value::Integer(2), Value::Null);
        assert_eq!(in_list(null.clone(), vec![]), Value::Integer(0));
        assert_eq!(in_list(null.clone(), vec![one.clone()]), Value::Null);
        assert_eq!(
            in_list(one.clone(), vec![null.clone(), one.clone()]),
            Value::Integer(1)
        );
        assert_eq!(in_list(two.clone(), vec![null, one.clone()]), Value::Null);
        assert_eq!(in_list(two, vec![one]), Value::Integer(0));
    }

    #[test]
    fn cast_follows_sqlite_conversions() {
        let text = |s: &str| Value::Text(s.into());
        assert_eq!(cast(text("12abc"), Affinity::Integer), Value::Integer(12));
        assert_eq!(cast(text("1e3"), Affinity::Integer), Value::Integer(1));
        assert_eq!(
            cast(text("99999999999999999999"), Affinity::Integer),
            Value::Integer(i64::MAX)
        );
        assert_eq!(
            cast(Value::Real(-3.7), Affinity::Integer),
            Value::Integer(-3)
        );
        assert_eq!(cast(text("1e3"), Affinity::Numeric), Value::Integer(1000));
        assert_eq!(cast(text("1e17"), Affinity::Numeric), Value::Real(1e17));
        assert_eq!(cast(text("abc"), Affinity::Numeric), Value::Integer(0));
        assert_eq!(cast(Value::Real(3.0), Affinity::Numeric), Value::Real(3.0));
        assert_eq!(cast(text("abc"), Affinity::Real), Value::Real(0.0));
        assert_eq!(cast(Value::Real(1.5), Affinity::Text), text("1.5"));
    }
}
