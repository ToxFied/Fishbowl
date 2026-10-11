//! Access paths: how a scan finds a table's rows.
//!
//! A SeqScan reads every row. An IndexScan reads only the rows an index
//! points to: it walks down the index's B-tree to the first matching entry,
//! reads entries in order until it passes the last one, and fetches each
//! row by its rowid. For a table with an `INTEGER PRIMARY KEY`, that column
//! is the rowid, so the table's own B-tree serves as the index.
//!
//! A lookup can fix the index's first columns with equalities (`a = 5 AND
//! b IN (1, 2)`) and then bound the next one (`c > 3`). The values come from
//! constants, or from the current row of an enclosing join or query, so a
//! join can look up the matching rows for each row it holds. When the index
//! holds every column the query reads, the table is not read at all: the
//! rows are rebuilt from the index entries (a covering index).
//!
//! Which access path to use is the planner's choice (`planner/paths.rs`).
//! The Filter or join condition above the scan keeps every condition. The
//! index only has to narrow the rows down, never to decide them, so the
//! bounds here can stay simple without risking a wrong answer.

use std::cmp::Ordering;
use std::collections::VecDeque;
use std::fmt;

use crate::error::{Error, Result};
use crate::expr::{Env, Expr};
use crate::storage::{IndexCursor, Row, Storage, TableScan};
use crate::value::{Affinity, Value};

/// Which B-tree an IndexScan reads, and which part of it.
#[derive(Debug, Clone)]
pub struct IndexAccess {
    /// The index to read, or `None` for the table's own B-tree, keyed by the
    /// `INTEGER PRIMARY KEY` column.
    pub index: Option<String>,
    /// The names of the B-tree's key columns, in order.
    pub columns: Vec<String>,
    /// The values the first `eq.len()` key columns must equal.
    pub eq: Vec<KeyValue>,
    /// Bounds on the key column after them.
    pub low: Vec<KeyBound>,
    pub high: Vec<KeyBound>,
    /// Set when the index holds every column the query reads.
    pub covering: Option<Covering>,
}

/// What one key column must equal.
#[derive(Debug, Clone)]
pub enum KeyValue {
    /// `column = expr`, worked out when the scan starts.
    Expr { expr: Expr, convert: Convert },
    /// `column IN (...)`: constants, converted, sorted and without repeats.
    List(Vec<Value>),
}

#[derive(Debug, Clone)]
pub struct KeyBound {
    pub expr: Expr,
    pub convert: Convert,
    pub inclusive: bool,
}

/// How a value is converted before the lookup, the way the comparison will
/// convert it: `a = '5'` on an INTEGER column looks for the number 5.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Convert {
    Numeric,
    Text,
    AsIs,
}

impl Convert {
    fn apply(self, v: Value) -> Value {
        match self {
            Convert::Numeric => v.apply_affinity(Affinity::Numeric),
            Convert::Text => v.apply_affinity(Affinity::Text),
            Convert::AsIs => v,
        }
    }
}

/// How to rebuild a table row from an index entry: each index column's
/// place in the row, and the INTEGER PRIMARY KEY's, which holds the rowid.
/// Columns the query never reads stay NULL.
#[derive(Debug, Clone)]
pub struct Covering {
    pub width: usize,
    pub positions: Vec<usize>,
    pub rowid: Option<usize>,
}

impl IndexAccess {
    /// Every row of the table, in rowid order.
    pub fn whole_table() -> IndexAccess {
        IndexAccess {
            index: None,
            columns: Vec::new(),
            eq: Vec::new(),
            low: Vec::new(),
            high: Vec::new(),
            covering: None,
        }
    }

    /// The expressions the lookup evaluates, for finding the columns it reads.
    pub fn exprs(&self) -> Vec<&Expr> {
        let eq = self.eq.iter().filter_map(|k| match k {
            KeyValue::Expr { expr, .. } => Some(expr),
            KeyValue::List(_) => None,
        });
        let bounds = self.low.iter().chain(&self.high).map(|b| &b.expr);
        eq.chain(bounds).collect()
    }

    /// The stretches of the B-tree to read, in order, for the values the
    /// keys have now. An equality with NULL matches nothing.
    fn stretches(&self, row: &[Value], env: &mut dyn Env) -> Result<VecDeque<Stretch>> {
        let mut prefixes: Vec<Vec<Value>> = vec![Vec::new()];
        for key in &self.eq {
            let values = match key {
                KeyValue::Expr { expr, convert } => match expr.eval_with(row, env)? {
                    Value::Null => Vec::new(),
                    v => vec![convert.apply(v)],
                },
                KeyValue::List(values) => values.clone(),
            };
            prefixes = prefixes
                .into_iter()
                .flat_map(|p| {
                    values.iter().map(move |v| {
                        let mut p = p.clone();
                        p.push(v.clone());
                        p
                    })
                })
                .collect();
        }
        let (Some(low), Some(high)) = (
            tightest(&self.low, row, env, true)?,
            tightest(&self.high, row, env, false)?,
        ) else {
            // A bound compared with NULL is never true.
            return Ok(VecDeque::new());
        };
        let ranged = !self.low.is_empty() || !self.high.is_empty();
        Ok(prefixes
            .into_iter()
            .map(|prefix| {
                let with = |bound: &Bound| {
                    let mut values = prefix.clone();
                    values.push(bound.value.clone());
                    (values, bound.inclusive)
                };
                // NULL sorts first, so starting just past NULL skips every
                // NULL: no range ever holds one.
                let start = match &low {
                    Some(bound) => Some(with(bound)),
                    None if ranged => Some(with(&Bound {
                        value: Value::Null,
                        inclusive: false,
                    })),
                    None if prefix.is_empty() => None,
                    None => Some((prefix.clone(), true)),
                };
                let stop = match &high {
                    Some(bound) => Some(with(bound)),
                    None if prefix.is_empty() => None,
                    None => Some((prefix.clone(), true)),
                };
                Stretch { start, stop }
            })
            .collect())
    }
}

/// One stretch of a B-tree: from the first key at or past `start` to the
/// last key at or before `stop`, comparing only as many values as each has.
struct Stretch {
    start: Option<(Vec<Value>, bool)>,
    stop: Option<(Vec<Value>, bool)>,
}

struct Bound {
    value: Value,
    inclusive: bool,
}

/// The narrowest of several bounds on one side, or `None` when one of them
/// is NULL. `Some(None)` means there is no bound on this side.
fn tightest(
    bounds: &[KeyBound],
    row: &[Value],
    env: &mut dyn Env,
    is_low: bool,
) -> Result<Option<Option<Bound>>> {
    let mut best: Option<Bound> = None;
    for bound in bounds {
        let value = match bound.expr.eval_with(row, env)? {
            Value::Null => return Ok(None),
            v => bound.convert.apply(v),
        };
        let wins = best.as_ref().is_none_or(|b| match value.compare(&b.value) {
            Ordering::Equal => !bound.inclusive,
            ord => ord.is_gt() == is_low,
        });
        if wins {
            best = Some(Bound {
                value,
                inclusive: bound.inclusive,
            });
        }
    }
    Ok(Some(best))
}

impl fmt::Display for IndexAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.index, &self.covering) {
            (Some(index), Some(_)) => write!(f, "USING COVERING INDEX {index}")?,
            (Some(index), None) => write!(f, "USING INDEX {index}")?,
            (None, _) => f.write_str("USING INTEGER PRIMARY KEY")?,
        }
        let mut terms = Vec::new();
        for (key, column) in self.eq.iter().zip(&self.columns) {
            terms.push(match key {
                KeyValue::Expr { expr, .. } => format!("{column} = {}", shown(expr)),
                KeyValue::List(values) => {
                    let list: Vec<String> = values.iter().map(literal).collect();
                    format!("{column} IN ({})", list.join(", "))
                }
            });
        }
        if let Some(column) = self.columns.get(self.eq.len()) {
            for (bounds, strict, loose) in [(&self.low, ">", ">="), (&self.high, "<", "<=")] {
                for b in bounds {
                    let op = if b.inclusive { loose } else { strict };
                    terms.push(format!("{column} {op} {}", shown(&b.expr)));
                }
            }
        }
        if !terms.is_empty() {
            write!(f, " ({})", terms.join(" AND "))?;
        }
        Ok(())
    }
}

/// A key's expression as the plan shows it: a constant as SQL would spell it.
fn shown(expr: &Expr) -> String {
    match expr {
        Expr::Literal(v) => literal(v),
        other => other.to_string(),
    }
}

/// A value as SQL would spell it: text in quotes.
fn literal(value: &Value) -> String {
    match value {
        Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
        other => other.to_string(),
    }
}

/// A running IndexScan: the stretches of the tree still to read and the
/// cursor reading the current one.
pub struct Scan {
    table: String,
    index: Option<String>,
    covering: Option<Covering>,
    stretches: VecDeque<Stretch>,
    cursor: Option<Cursor>,
    stop: Option<(Vec<Value>, bool)>,
}

enum Cursor {
    Table(TableScan),
    Index(IndexCursor),
}

impl Scan {
    /// Starts a scan, working out the lookup's values from `row`, the row of
    /// the enclosing join (empty when there is none), and `env`, which holds
    /// the rows of enclosing queries.
    pub fn start(
        table: &str,
        access: &IndexAccess,
        row: &[Value],
        env: &mut dyn Env,
    ) -> Result<Scan> {
        Ok(Scan {
            table: table.to_string(),
            index: access.index.clone(),
            covering: access.covering.clone(),
            stretches: access.stretches(row, env)?,
            cursor: None,
            stop: None,
        })
    }

    /// The next row and its rowid.
    pub fn next_entry(&mut self, storage: &Storage) -> Result<Option<(i64, Row)>> {
        loop {
            let cursor = match &mut self.cursor {
                Some(cursor) => cursor,
                None => {
                    let Some(stretch) = self.stretches.pop_front() else {
                        return Ok(None);
                    };
                    self.stop = stretch.stop;
                    self.cursor.insert(match &self.index {
                        Some(index) => Cursor::Index(IndexCursor::new(index, stretch.start)),
                        None => Cursor::Table(TableScan::from_value(
                            &self.table,
                            stretch
                                .start
                                .map(|(mut v, inclusive)| (v.remove(0), inclusive)),
                        )),
                    })
                }
            };
            let found = match cursor {
                Cursor::Table(scan) => scan
                    .next_entry(storage)?
                    .filter(|(rowid, _)| !past(&[Value::Integer(*rowid)], &self.stop)),
                Cursor::Index(entries) => match entries.next(storage)? {
                    Some(key) if !past(&key.values, &self.stop) => Some(match &self.covering {
                        Some(covering) => {
                            let mut row = vec![Value::Null; covering.width];
                            for (value, &position) in
                                key.values.into_iter().zip(&covering.positions)
                            {
                                row[position] = value;
                            }
                            if let Some(position) = covering.rowid {
                                row[position] = Value::Integer(key.rowid);
                            }
                            (key.rowid, row)
                        }
                        None => (key.rowid, self.fetch(storage, key.rowid)?),
                    }),
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

    fn fetch(&self, storage: &Storage, rowid: i64) -> Result<Row> {
        storage.get(&self.table, rowid)?.ok_or_else(|| {
            Error::Storage(format!(
                "an index on {} points to row {rowid}, which does not exist",
                self.table
            ))
        })
    }
}

/// Whether a key lies beyond the end of the stretch being read, comparing
/// as many of its values as the stop has.
fn past(values: &[Value], stop: &Option<(Vec<Value>, bool)>) -> bool {
    let Some((stop, inclusive)) = stop else {
        return false;
    };
    let ord = values
        .iter()
        .zip(stop)
        .map(|(a, b)| a.compare(b))
        .find(|o| o.is_ne())
        .unwrap_or(Ordering::Equal);
    match ord {
        Ordering::Greater => true,
        Ordering::Equal => !inclusive,
        Ordering::Less => false,
    }
}
