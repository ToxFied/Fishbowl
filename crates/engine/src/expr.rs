//! Bound expressions: SQL expressions whose column names have already been
//! resolved to positions in a row, so evaluating them is just arithmetic.

use std::fmt;

use crate::value::{Value, numeric_prefix};

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
    Column {
        index: usize,
        name: String,
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
}

impl Expr {
    pub fn eval(&self, row: &[Value]) -> Value {
        match self {
            Expr::Literal(v) => v.clone(),
            Expr::Column { index, .. } => row.get(*index).cloned().unwrap_or(Value::Null),
            Expr::Unary { op, expr } => eval_unary(*op, expr.eval(row)),
            Expr::Binary { op, left, right } => {
                let l = left.eval(row);
                // AND and OR can skip the right side when the left decides it.
                match (op, l.truthiness()) {
                    (BinaryOp::And, Some(false)) => return Value::Integer(0),
                    (BinaryOp::Or, Some(true)) => return Value::Integer(1),
                    _ => {}
                }
                eval_binary(*op, l, right.eval(row))
            }
            Expr::IsNull { expr, negated } => bool_value(expr.eval(row).is_null() != *negated),
        }
    }
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
        Eq | NotEq | Lt | LtEq | Gt | GtEq => {
            let ord = l.compare(&r);
            bool_value(match op {
                Eq => ord.is_eq(),
                NotEq => ord.is_ne(),
                Lt => ord.is_lt(),
                LtEq => ord.is_le(),
                Gt => ord.is_gt(),
                _ => ord.is_ge(),
            })
        }
        Concat => Value::Text(format!("{l}{r}")),
        Add | Subtract | Multiply | Divide | Remainder => {
            arithmetic(op, to_number(l), to_number(r))
        }
    }
}

fn arithmetic(op: BinaryOp, l: Value, r: Value) -> Value {
    use BinaryOp::*;
    if op == Remainder {
        // SQLite computes % on integers, then returns a real if either side was real.
        let (a, b) = (l.to_i64_lossy(), r.to_i64_lossy());
        if b == 0 {
            return Value::Null;
        }
        let rem = a.checked_rem(b).unwrap_or(0);
        return match (&l, &r) {
            (Value::Integer(_), Value::Integer(_)) => Value::Integer(rem),
            _ => Value::Real(rem as f64),
        };
    }
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

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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
            Expr::IsNull { expr, negated } => {
                write!(f, "{expr} IS {}NULL", if *negated { "NOT " } else { "" })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bin(op: BinaryOp, l: Value, r: Value) -> Value {
        Expr::Binary {
            op,
            left: Box::new(Expr::Literal(l)),
            right: Box::new(Expr::Literal(r)),
        }
        .eval(&[])
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
}
