//! Access paths: how a scan finds a table's rows.
//!
//! A SeqScan reads every row. An IndexScan reads only the rows an index
//! points to: it walks down the index's B-tree to the first matching entry,
//! reads entries in order until it passes the last one, and fetches each
//! row by its rowid. For a table with an `INTEGER PRIMARY KEY`, that column
//! is the rowid, so the table's own B-tree serves as the index.
//!
//! The rule for now is simple: if a WHERE term is an equality, a range or
//! an `IN` list on the first column of an index, use that index. The M5
//! planner will weigh costs instead.
//!
//! The Filter above the scan keeps the whole WHERE clause. The index only has
//! to narrow the rows down, never to decide them, so the bounds here can
//! stay simple without risking a wrong answer.

use std::collections::VecDeque;
use std::fmt;

use crate::catalog::TableSchema;
use crate::error::{Error, Result};
use crate::expr::{BinaryOp, Expr};
use crate::plan::PlanNode;
use crate::storage::{IndexCursor, Row, Storage, TableScan};
use crate::value::{Affinity, Value};

/// Which B-tree an IndexScan reads, and which part of it.
#[derive(Debug, Clone)]
pub struct IndexAccess {
    /// The index to read, or `None` for the table's own B-tree, keyed by the
    /// `INTEGER PRIMARY KEY` column.
    pub index: Option<String>,
    /// The column the lookup is on.
    pub column: String,
    pub lookup: Lookup,
}

#[derive(Debug, Clone)]
pub enum Lookup {
    /// Every value from `low` to `high`. NULL is never in a range, because
    /// `NULL < 5` is not true.
    Range {
        low: Option<Bound>,
        high: Option<Bound>,
    },
    /// Each of these values, sorted, without repeats. One value is `=`.
    Values(Vec<Value>),
}

#[derive(Debug, Clone)]
pub struct Bound {
    pub value: Value,
    pub inclusive: bool,
}

impl Lookup {
    /// The stretches of the index to read, in order, as (start, stop).
    fn ranges(&self) -> VecDeque<(Bound, Option<Bound>)> {
        let at = |value: &Value| Bound {
            value: value.clone(),
            inclusive: true,
        };
        match self {
            // NULL sorts first, so starting just past NULL skips every NULL.
            Lookup::Range { low, high } => VecDeque::from([(
                low.clone().unwrap_or(Bound {
                    value: Value::Null,
                    inclusive: false,
                }),
                high.clone(),
            )]),
            Lookup::Values(values) => values.iter().map(|v| (at(v), Some(at(v)))).collect(),
        }
    }
}

impl fmt::Display for IndexAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.index {
            Some(index) => write!(f, "USING INDEX {index} (")?,
            None => f.write_str("USING INTEGER PRIMARY KEY (")?,
        }
        let column = &self.column;
        match &self.lookup {
            Lookup::Values(values) if values.len() == 1 => {
                write!(f, "{column} = {}", literal(&values[0]))?
            }
            Lookup::Values(values) => {
                let list: Vec<String> = values.iter().map(literal).collect();
                write!(f, "{column} IN ({})", list.join(", "))?
            }
            Lookup::Range { low, high } => {
                let low = low.as_ref().map(|b| {
                    let op = if b.inclusive { ">=" } else { ">" };
                    format!("{column} {op} {}", literal(&b.value))
                });
                let high = high.as_ref().map(|b| {
                    let op = if b.inclusive { "<=" } else { "<" };
                    format!("{column} {op} {}", literal(&b.value))
                });
                let parts: Vec<String> = low.into_iter().chain(high).collect();
                f.write_str(&parts.join(" AND "))?
            }
        }
        f.write_str(")")
    }
}

/// A value as SQL would spell it: text in quotes.
fn literal(value: &Value) -> String {
    match value {
        Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
        other => other.to_string(),
    }
}

/// Puts an IndexScan under the WHERE filter when an index can narrow the
/// scan, and otherwise explains why every row is read. Plans that are not a
/// single-table scan pass through unchanged.
pub fn use_index(root: PlanNode, table: Option<&TableSchema>) -> PlanNode {
    let Some(table) = table.filter(|t| t.rowid_alias.is_some() || !t.indexes.is_empty()) else {
        return root;
    };
    let seq_scan = |reason: String| PlanNode::SeqScan {
        table: table.name.clone(),
        reason,
    };
    match root {
        PlanNode::Filter { input, predicate } if matches!(*input, PlanNode::SeqScan { .. }) => {
            let input = match choose(table, &predicate) {
                Some((access, reason)) => PlanNode::IndexScan {
                    table: table.name.clone(),
                    access,
                    reason,
                },
                None => seq_scan(format!(
                    "no WHERE term is an equality or range on the first column of an index \
                     on {}, so every row is read",
                    table.name
                )),
            };
            PlanNode::Filter {
                input: Box::new(input),
                predicate,
            }
        }
        PlanNode::SeqScan { .. } => seq_scan(format!(
            "there is no WHERE clause to narrow the rows down, so every row of {} is read",
            table.name
        )),
        other => other,
    }
}

/// What one WHERE term says about one column.
struct Term {
    column: usize,
    lookup: Lookup,
}

/// Picks the best usable term: an equality over an `IN` list over a range,
/// the rowid over an index, a unique index over another, and otherwise the
/// first term written. Range terms on the chosen column are combined, so
/// `a > 5 AND a < 9` reads only from 5 to 9.
fn choose(table: &TableSchema, predicate: &Expr) -> Option<(IndexAccess, String)> {
    let mut conjuncts = Vec::new();
    split_and(predicate, &mut conjuncts);
    let terms: Vec<Term> = conjuncts.iter().filter_map(|e| term(table, e)).collect();

    let rank = |term: &Term| {
        let (index, unique) = index_on(table, term.column)?;
        let kind = match &term.lookup {
            Lookup::Values(values) if values.len() <= 1 => 2,
            Lookup::Values(_) => 1,
            Lookup::Range { .. } => 0,
        };
        Some((kind, index.is_none(), unique))
    };
    let best = terms
        .iter()
        .filter_map(|t| Some((rank(t)?, t)))
        // `max_by_key` keeps the last of equals; reversing keeps the first.
        .rev()
        .max_by_key(|(rank, _)| *rank)
        .map(|(_, t)| t)?;

    let column = best.column;
    let lookup = match &best.lookup {
        Lookup::Range { .. } => {
            let ranges = terms.iter().filter(|t| t.column == column);
            let (mut low, mut high) = (None, None);
            for term in ranges {
                if let Lookup::Range { low: l, high: h } = &term.lookup {
                    low = tighter(low, l.clone(), true);
                    high = tighter(high, h.clone(), false);
                }
            }
            Lookup::Range { low, high }
        }
        values => values.clone(),
    };
    let (index, _) = index_on(table, column)?;
    let name = &table.columns[column].name;
    let access = IndexAccess {
        index: index.clone(),
        column: name.clone(),
        lookup,
    };
    let how = match &access.lookup {
        Lookup::Values(values) if values.len() <= 1 => format!("is an equality on {name}"),
        Lookup::Values(_) => format!("lists values of {name}"),
        Lookup::Range { .. } => format!("is a range on {name}"),
    };
    let reason = match &index {
        Some(index) => format!(
            "the WHERE term {how}, and index {index} keeps {name} in order, so only the \
             matching entries are read and each leads to its row by rowid"
        ),
        None => format!(
            "the WHERE term {how}, the INTEGER PRIMARY KEY. It is the rowid the table's own \
             B-tree is sorted by, so only the matching rows are read"
        ),
    };
    Some((access, reason))
}

/// The tree that has `column` first: `Some(None)` for the rowid, or the
/// name of an index, preferring a unique one. Also says whether it is unique.
fn index_on(table: &TableSchema, column: usize) -> Option<(Option<String>, bool)> {
    if table.rowid_alias == Some(column) {
        return Some((None, true));
    }
    let leads = |unique: bool| {
        table
            .indexes
            .iter()
            .find(|i| i.unique == unique && i.columns[0].position == column)
            .map(|i| (Some(i.name.clone()), unique))
    };
    leads(true).or_else(|| leads(false))
}

/// The narrower of two bounds on the same side of a range.
fn tighter(current: Option<Bound>, new: Option<Bound>, is_low: bool) -> Option<Bound> {
    match (current, new) {
        (Some(a), Some(b)) => {
            let ord = b.value.compare(&a.value);
            let b_wins = match ord {
                std::cmp::Ordering::Equal => !b.inclusive,
                _ => ord.is_gt() == is_low,
            };
            Some(if b_wins { b } else { a })
        }
        (a, b) => a.or(b),
    }
}

fn split_and<'e>(expr: &'e Expr, out: &mut Vec<&'e Expr>) {
    match expr {
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => {
            split_and(left, out);
            split_and(right, out);
        }
        other => out.push(other),
    }
}

/// Reads `column = 5`, `5 < column`, `column BETWEEN 1 AND 9` and
/// `column IN (1, 2)` where the other side is a constant.
fn term(table: &TableSchema, expr: &Expr) -> Option<Term> {
    let column_of = |e: &Expr| match e {
        Expr::Column { index, .. } if *index < table.columns.len() => Some(*index),
        _ => None,
    };
    let (column, lookup) = match expr {
        Expr::Binary { op, left, right } => {
            let (column, op, value) = match (column_of(left), column_of(right)) {
                (Some(c), _) => (c, *op, constant(right)?),
                (None, Some(c)) => (c, flip(*op)?, constant(left)?),
                _ => return None,
            };
            let bound = |inclusive| {
                Some(Bound {
                    value: value.clone(),
                    inclusive,
                })
            };
            if value.is_null() {
                return None;
            }
            let lookup = match op {
                BinaryOp::Eq => Lookup::Values(vec![value.clone()]),
                BinaryOp::Gt => range(bound(false), None),
                BinaryOp::GtEq => range(bound(true), None),
                BinaryOp::Lt => range(None, bound(false)),
                BinaryOp::LtEq => range(None, bound(true)),
                _ => return None,
            };
            (column, lookup)
        }
        Expr::Between {
            expr,
            low,
            high,
            negated: false,
        } => {
            let bound = |e: &Expr| {
                Some(Bound {
                    value: constant(e)?,
                    inclusive: true,
                })
            };
            (
                column_of(expr)?,
                range(bound(low)?.into(), bound(high)?.into()),
            )
        }
        Expr::InList {
            expr,
            list,
            negated: false,
        } => {
            let values = list.iter().map(constant).collect::<Option<Vec<_>>>()?;
            (column_of(expr)?, Lookup::Values(values))
        }
        _ => return None,
    };
    Some(Term {
        column,
        lookup: convert(lookup, table.columns[column].affinity)?,
    })
}

fn range(low: Option<Bound>, high: Option<Bound>) -> Lookup {
    Lookup::Range { low, high }
}

/// The same comparison with its sides swapped: `5 < a` is `a > 5`.
fn flip(op: BinaryOp) -> Option<BinaryOp> {
    Some(match op {
        BinaryOp::Eq => BinaryOp::Eq,
        BinaryOp::Lt => BinaryOp::Gt,
        BinaryOp::LtEq => BinaryOp::GtEq,
        BinaryOp::Gt => BinaryOp::Lt,
        BinaryOp::GtEq => BinaryOp::LtEq,
        _ => return None,
    })
}

/// The value of an expression that reads no column and calls no function,
/// such as `5`, `-2.5` or `'abc'`. It must also have no affinity of its own
/// (unlike `CAST(...)`), so the comparison uses the column's alone.
fn constant(expr: &Expr) -> Option<Value> {
    let mut fixed = expr.affinity().is_none();
    expr.walk(&mut |e| {
        fixed &= !matches!(
            e,
            Expr::Column { .. }
                | Expr::Function { .. }
                | Expr::OuterColumn { .. }
                | Expr::Subquery { .. }
        );
    });
    if !fixed {
        return None;
    }
    expr.eval(&[]).ok()
}

/// Converts the constants the way the comparison will: `a = '5'` on an
/// INTEGER column looks for the number 5. NULL in an `IN` list can never
/// match, so it is dropped. A comparison with NULL is never true, so it is
/// left to the Filter rather than turned into a lookup.
fn convert(lookup: Lookup, affinity: Affinity) -> Option<Lookup> {
    let convert = |v: Value| match affinity {
        Affinity::Integer | Affinity::Real | Affinity::Numeric => {
            v.apply_affinity(Affinity::Numeric)
        }
        // A TEXT column holds only text, so the other side becomes text.
        Affinity::Text => v.apply_affinity(Affinity::Text),
        Affinity::Blob => v,
    };
    match lookup {
        Lookup::Values(values) => {
            let mut values: Vec<Value> = values
                .into_iter()
                .filter(|v| !v.is_null())
                .map(convert)
                .collect();
            values.sort_by(|a, b| a.compare(b));
            values.dedup_by(|a, b| a.compare(b).is_eq());
            Some(Lookup::Values(values))
        }
        Lookup::Range { low, high } => {
            let bounds_null = [&low, &high]
                .into_iter()
                .any(|b| b.as_ref().is_some_and(|b| b.value.is_null()));
            if bounds_null {
                return None;
            }
            let convert = |b: Option<Bound>| {
                b.map(|b| Bound {
                    value: convert(b.value),
                    inclusive: b.inclusive,
                })
            };
            Some(range(convert(low), convert(high)))
        }
    }
}

/// How an UPDATE or DELETE finds its rows: through the index a SELECT with
/// the same WHERE would use, or else every row in rowid order.
pub fn for_write(table: &TableSchema, predicate: Option<&Expr>) -> IndexAccess {
    predicate
        .and_then(|p| choose(table, p))
        .map(|(access, _)| access)
        .unwrap_or_else(|| IndexAccess {
            index: None,
            column: String::new(),
            lookup: range(None, None),
        })
}

/// A running IndexScan: the stretches of the tree still to read and the
/// cursor reading the current one.
pub struct Scan {
    table: String,
    index: Option<String>,
    ranges: VecDeque<(Bound, Option<Bound>)>,
    cursor: Option<Cursor>,
    stop: Option<Bound>,
}

enum Cursor {
    Table(TableScan),
    Index(IndexCursor),
}

impl Scan {
    pub fn new(table: &str, access: &IndexAccess) -> Scan {
        Scan {
            table: table.to_string(),
            index: access.index.clone(),
            ranges: access.lookup.ranges(),
            cursor: None,
            stop: None,
        }
    }

    pub fn next(&mut self, storage: &Storage) -> Result<Option<Row>> {
        Ok(self.next_entry(storage)?.map(|(_, row)| row))
    }

    /// The next row and its rowid.
    pub fn next_entry(&mut self, storage: &Storage) -> Result<Option<(i64, Row)>> {
        loop {
            let cursor = match &mut self.cursor {
                Some(cursor) => cursor,
                None => {
                    let Some((start, stop)) = self.ranges.pop_front() else {
                        return Ok(None);
                    };
                    let start = Some((start.value, start.inclusive));
                    self.stop = stop;
                    self.cursor.insert(match &self.index {
                        Some(index) => Cursor::Index(IndexCursor::new(
                            index,
                            start.map(|(v, inclusive)| (vec![v], inclusive)),
                        )),
                        None => Cursor::Table(TableScan::from_value(&self.table, start)),
                    })
                }
            };
            let found = match cursor {
                Cursor::Table(scan) => scan
                    .next_entry(storage)?
                    .filter(|(rowid, _)| !past(&Value::Integer(*rowid), &self.stop)),
                Cursor::Index(entries) => match entries.next(storage)? {
                    Some(key) if !past(&key.values[0], &self.stop) => {
                        let row = storage.get(&self.table, key.rowid)?.ok_or_else(|| {
                            Error::Storage(format!(
                                "an index on {} points to row {}, which does not exist",
                                self.table, key.rowid
                            ))
                        })?;
                        Some((key.rowid, row))
                    }
                    _ => None,
                },
            };
            match found {
                Some(entry) => return Ok(Some(entry)),
                // This stretch is done; move on to the next one.
                None => self.cursor = None,
            }
        }
    }
}

/// Whether `value` lies beyond the end of the stretch being read.
fn past(value: &Value, stop: &Option<Bound>) -> bool {
    stop.as_ref()
        .is_some_and(|stop| match value.compare(&stop.value) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Equal => !stop.inclusive,
            std::cmp::Ordering::Less => false,
        })
}
