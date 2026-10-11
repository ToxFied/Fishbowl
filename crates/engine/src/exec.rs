//! Runs query plans using the iterator ("Volcano") model: every step has a
//! `next()` that asks its input for a row, does its work, and hands one row up.
//! Rows flow one at a time from the scan at the bottom to the root.
//!
//! Sort and Aggregate are the exceptions that prove the rule: they cannot
//! return anything until they have seen every input row, so they read it all
//! on the first call and then hand rows out one at a time.

use std::cmp::Ordering;
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};

use crate::access;
use crate::error::{Error, Result};
use crate::expr::{Accumulator, AggregateCall, Expr, RowKey};
use crate::plan::{PlanNode, PlanProfile, SortKey};
use crate::storage::{Row, Storage, TableScan};
use crate::value::{Affinity, Value};

/// Shared state for one statement: where rows live and how much work is left.
pub struct ExecContext<'a> {
    pub storage: &'a Storage,
    budget: Option<u64>,
    rows_processed: u64,
}

impl<'a> ExecContext<'a> {
    pub fn new(storage: &'a Storage, budget: Option<u64>) -> Self {
        ExecContext {
            storage,
            budget,
            rows_processed: 0,
        }
    }

    /// Counts one row of work and stops the statement once the budget is spent,
    /// so a runaway query fails cleanly instead of hanging.
    fn charge(&mut self) -> Result<()> {
        self.rows_processed += 1;
        match self.budget {
            Some(limit) if self.rows_processed > limit => Err(Error::BudgetExceeded(limit)),
            _ => Ok(()),
        }
    }
}

/// A plan node that is running: the node, its input, its cursor state and a
/// count of the rows it has produced.
struct Operator<'p> {
    node: &'p PlanNode,
    input: Option<Box<Operator<'p>>>,
    state: State,
    rows_out: u64,
}

enum State {
    /// Filter and Project keep nothing between rows.
    None,
    Scan {
        scan: TableScan,
    },
    IndexScan(access::Scan),
    OneRow {
        done: bool,
    },
    /// Sort and Aggregate: the finished rows, computed on the first call.
    Buffered(Option<std::vec::IntoIter<Row>>),
    Distinct(BTreeSet<RowKey>),
    /// Rows Limit may still return (`None` for no limit), set on the first call.
    Limit {
        started: bool,
        remaining: Option<u64>,
    },
}

impl<'p> Operator<'p> {
    fn build(node: &'p PlanNode) -> Operator<'p> {
        let input = node
            .children()
            .first()
            .map(|child| Box::new(Operator::build(child)));
        let state = match node {
            PlanNode::SeqScan { table, .. } => State::Scan {
                scan: TableScan::new(table),
            },
            PlanNode::IndexScan { table, access, .. } => {
                State::IndexScan(access::Scan::new(table, access))
            }
            PlanNode::OneRow => State::OneRow { done: false },
            PlanNode::Filter { .. } | PlanNode::Project { .. } => State::None,
            PlanNode::Sort { .. } | PlanNode::Aggregate { .. } => State::Buffered(None),
            PlanNode::Distinct { .. } => State::Distinct(BTreeSet::new()),
            PlanNode::Limit { .. } => State::Limit {
                started: false,
                remaining: None,
            },
        };
        Operator {
            node,
            input,
            state,
            rows_out: 0,
        }
    }

    fn next(&mut self, ctx: &mut ExecContext) -> Result<Option<Row>> {
        let row = self.produce(ctx)?;
        if row.is_some() {
            ctx.charge()?;
            self.rows_out += 1;
        }
        Ok(row)
    }

    fn produce(&mut self, ctx: &mut ExecContext) -> Result<Option<Row>> {
        let input = &mut self.input;
        match (self.node, &mut self.state) {
            (PlanNode::SeqScan { .. }, State::Scan { scan }) => scan.next(ctx.storage),
            (PlanNode::IndexScan { .. }, State::IndexScan(scan)) => scan.next(ctx.storage),
            (PlanNode::OneRow, State::OneRow { done }) => {
                let first = !*done;
                *done = true;
                Ok(first.then(Vec::new))
            }
            (PlanNode::Filter { predicate, .. }, _) => {
                while let Some(row) = pull(input, ctx)? {
                    if predicate.eval(&row)?.truthiness() == Some(true) {
                        return Ok(Some(row));
                    }
                }
                Ok(None)
            }
            (PlanNode::Project { exprs, .. }, _) => match pull(input, ctx)? {
                Some(row) => Ok(Some(
                    exprs.iter().map(|e| e.eval(&row)).collect::<Result<_>>()?,
                )),
                None => Ok(None),
            },
            (PlanNode::Sort { keys, .. }, State::Buffered(buffer)) => {
                if buffer.is_none() {
                    *buffer = Some(sort(input, ctx, keys)?.into_iter());
                }
                Ok(buffer.as_mut().and_then(Iterator::next))
            }
            (
                PlanNode::Aggregate {
                    group_by,
                    aggregates,
                    row_from,
                    width,
                    ..
                },
                State::Buffered(buffer),
            ) => {
                if buffer.is_none() {
                    let groups = aggregate(input, ctx, group_by, aggregates, *row_from, *width)?;
                    *buffer = Some(groups.into_iter());
                }
                Ok(buffer.as_mut().and_then(Iterator::next))
            }
            (PlanNode::Distinct { .. }, State::Distinct(seen)) => {
                while let Some(row) = pull(input, ctx)? {
                    if seen.insert(RowKey(row.clone())) {
                        return Ok(Some(row));
                    }
                }
                Ok(None)
            }
            (PlanNode::Limit { limit, offset, .. }, State::Limit { started, remaining }) => {
                if !*started {
                    *started = true;
                    // A negative LIMIT means no limit, a negative OFFSET none.
                    *remaining = u64::try_from(integer_clause(limit)?).ok();
                    let skip = match offset {
                        Some(offset) => integer_clause(offset)?.max(0),
                        None => 0,
                    };
                    for _ in 0..skip {
                        if pull(input, ctx)?.is_none() {
                            break;
                        }
                    }
                }
                match remaining {
                    Some(0) => Ok(None),
                    Some(n) => {
                        *n -= 1;
                        pull(input, ctx)
                    }
                    None => pull(input, ctx),
                }
            }
            _ => unreachable!("an operator's state always matches its plan node"),
        }
    }

    fn profile(&self, next_id: &mut usize) -> PlanProfile {
        let id = *next_id;
        *next_id += 1;
        PlanProfile {
            id,
            operator: self.node.operator(),
            detail: self.node.detail(),
            reason: self.node.reason().map(str::to_string),
            rows_out: self.rows_out,
            children: self.input.iter().map(|i| i.profile(next_id)).collect(),
        }
    }
}

/// The next row from an operator's input, if it has one.
fn pull(input: &mut Option<Box<Operator>>, ctx: &mut ExecContext) -> Result<Option<Row>> {
    match input {
        Some(op) => op.next(ctx),
        None => Ok(None),
    }
}

/// LIMIT and OFFSET must be integers. `'5'` and `5.0` convert; `5.5`, text
/// and NULL are a "datatype mismatch", as in SQLite.
fn integer_clause(expr: &Expr) -> Result<i64> {
    match expr.eval(&[])?.apply_affinity(Affinity::Numeric) {
        Value::Integer(i) => Ok(i),
        _ => Err(Error::Runtime("datatype mismatch".into())),
    }
}

/// Reads every input row and sorts it by the ORDER BY keys. The sort is
/// stable, so rows that tie stay in the order they arrived, like SQLite's.
fn sort(
    input: &mut Option<Box<Operator>>,
    ctx: &mut ExecContext,
    keys: &[SortKey],
) -> Result<Vec<Row>> {
    let mut keyed = Vec::new();
    while let Some(row) = pull(input, ctx)? {
        let values = keys
            .iter()
            .map(|k| k.expr.eval(&row))
            .collect::<Result<Vec<_>>>()?;
        keyed.push((values, row));
    }
    keyed.sort_by(|(a, _), (b, _)| compare_sort_keys(a, b, keys));
    Ok(keyed.into_iter().map(|(_, row)| row).collect())
}

fn compare_sort_keys(a: &[Value], b: &[Value], keys: &[SortKey]) -> Ordering {
    for ((x, y), key) in a.iter().zip(b).zip(keys) {
        let null_first = if key.nulls_first {
            Ordering::Less
        } else {
            Ordering::Greater
        };
        let ord = match (x.is_null(), y.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) => null_first,
            (false, true) => null_first.reverse(),
            _ if key.descending => x.compare(y).reverse(),
            _ => x.compare(y),
        };
        if ord.is_ne() {
            return ord;
        }
    }
    Ordering::Equal
}

struct Group {
    /// The input row plain columns are read from.
    row: Row,
    accumulators: Vec<Accumulator>,
}

/// Reads every input row, sorts it into a group by its GROUP BY values and
/// feeds it to that group's aggregates. Groups come out ordered by their
/// values, which is the order SQLite's GROUP BY produces.
fn aggregate(
    input: &mut Option<Box<Operator>>,
    ctx: &mut ExecContext,
    group_by: &[SortKey],
    aggregates: &[AggregateCall],
    row_from: Option<usize>,
    width: usize,
) -> Result<Vec<Row>> {
    let new_group = |row| Group {
        row,
        accumulators: aggregates.iter().map(Accumulator::new).collect(),
    };
    let mut groups: BTreeMap<RowKey, Group> = BTreeMap::new();
    while let Some(row) = pull(input, ctx)? {
        let key = RowKey(
            group_by
                .iter()
                .map(|k| k.expr.eval(&row))
                .collect::<Result<_>>()?,
        );
        let args = aggregates
            .iter()
            .map(|a| a.args.iter().map(|e| e.eval(&row)).collect())
            .collect::<Result<Vec<Vec<Value>>>>()?;
        let step = |group: &mut Group| {
            let mut chosen = false;
            for (i, (acc, args)) in group.accumulators.iter_mut().zip(&args).enumerate() {
                let took = acc.step(args);
                chosen |= took && row_from == Some(i);
            }
            chosen
        };
        match groups.entry(key) {
            // A group's first row is its row until a min() or max() picks another.
            Entry::Vacant(entry) => {
                step(entry.insert(new_group(row)));
            }
            Entry::Occupied(entry) => {
                let group = entry.into_mut();
                if step(group) {
                    group.row = row;
                }
            }
        }
    }
    // Without GROUP BY there is always exactly one group, even for no rows.
    if group_by.is_empty() && groups.is_empty() {
        groups.insert(RowKey(Vec::new()), new_group(vec![Value::Null; width]));
    }
    // The map holds groups in ascending order; a descending key reorders them.
    let mut groups: Vec<(RowKey, Group)> = groups.into_iter().collect();
    if group_by.iter().any(|k| k.descending) {
        groups.sort_by(|(a, _), (b, _)| compare_sort_keys(&a.0, &b.0, group_by));
    }
    groups
        .into_iter()
        .map(|(_, group)| {
            let mut row = group.row;
            for acc in &group.accumulators {
                row.push(acc.finish()?);
            }
            Ok(row)
        })
        .collect()
}

/// Runs a query plan to completion and returns its rows plus the plan tree
/// annotated with how many rows each step produced.
pub fn run_query(ctx: &mut ExecContext, root: &PlanNode) -> Result<(Vec<Row>, PlanProfile)> {
    let mut op = Operator::build(root);
    let mut rows = Vec::new();
    while let Some(row) = op.next(ctx)? {
        rows.push(row);
    }
    let profile = op.profile(&mut 0);
    Ok((rows, profile))
}
