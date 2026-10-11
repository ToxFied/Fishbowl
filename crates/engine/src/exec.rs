//! Runs query plans using the iterator ("Volcano") model: every step has a
//! `next()` that asks its input for a row, does its work, and hands one row up.
//! Rows flow one at a time from the scan at the bottom to the root.
//!
//! Sort and Aggregate are the exceptions that prove the rule: they cannot
//! return anything until they have seen every input row, so they read it all
//! on the first call and then hand rows out one at a time.
//!
//! A step can have two inputs (a join, a compound SELECT) and can start its
//! input over ([`Operator::rewind`]), which is how a nested-loop join reads
//! its right side once per left row. Subqueries are small plans of their own,
//! run by the step whose expression contains them.
//!
//! When the context carries a [`Recorder`], every step also reports what it
//! did with each row: passed it on, dropped it, or ran a subquery for it.
//! See `trace.rs`.

use std::cmp::Ordering;
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::ptr;
use std::rc::Rc;

use crate::access;
use crate::error::{Error, Result, unsupported};
use crate::expr::{Accumulator, AggregateCall, Env, Expr, RowKey, SubqueryKind};
use crate::plan::{Choice, JoinKind, PlanNode, PlanProfile, SetOp, SortKey, Subquery};
use crate::storage::{Row, Storage, TableScan};
use crate::trace::{EventKind, MAX_SCAN_ROWIDS, MAX_SCAN_ROWS, Recorder, ScanRows, Trace};
use crate::value::{Affinity, Value};

/// Shared state for one statement: where rows live and how much work is left.
pub struct ExecContext<'a> {
    pub storage: &'a Storage,
    budget: Option<u64>,
    rows_processed: u64,
    /// The rows that enclosing queries are working on, innermost last, for
    /// correlated subqueries to read.
    outer: Vec<Row>,
    /// Set only while tracing, so an untraced query copies no rows.
    trace: Option<Recorder>,
}

impl<'a> ExecContext<'a> {
    pub fn new(storage: &'a Storage, budget: Option<u64>) -> Self {
        ExecContext {
            storage,
            budget,
            rows_processed: 0,
            outer: Vec::new(),
            trace: None,
        }
    }

    /// Records up to `limit` events of the next query run in this context.
    pub fn start_trace(&mut self, limit: usize) {
        self.trace = Some(Recorder::new(limit));
    }

    /// The trace of the query run since [`ExecContext::start_trace`], if any.
    pub fn take_trace(&mut self) -> Option<Trace> {
        self.trace.take().map(Recorder::finish)
    }

    fn record(&mut self, step: usize, kind: EventKind, row: &[Value], rowid: Option<i64>) {
        if let Some(recorder) = &mut self.trace {
            recorder.record(step, kind, row, rowid);
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

/// A plan node that is running: the node, its inputs, its cursor state and a
/// count of the rows it has produced.
struct Operator<'p> {
    /// Pre-order position in the plan, the id the profile and trace use.
    id: usize,
    node: &'p PlanNode,
    input: Option<Box<Operator<'p>>>,
    /// The second input of a join or a compound SELECT.
    right: Option<Box<Operator<'p>>>,
    /// The subqueries in this step's expressions.
    subqueries: Vec<SubqueryOp<'p>>,
    state: State,
    rows_out: u64,
    /// The rowid of the row a scan just read, for the trace.
    rowid: Option<i64>,
}

enum State {
    /// Filter and Project keep nothing between rows.
    None,
    Scan {
        scan: TableScan,
    },
    /// An IndexScan starts on its first call, when its lookup's values can
    /// be worked out. `left` is the join's current left row, which they
    /// may read.
    IndexScan {
        scan: Option<access::Scan>,
        left: Row,
    },
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
    /// NestedLoopJoin: the left row being joined, followed by the right row
    /// being tried, and whether any right row has matched it yet.
    Join {
        pair: Option<Row>,
        left_width: usize,
        matched: bool,
    },
    /// UNION ALL: whether the left input has run out.
    Append {
        left_done: bool,
    },
    /// Materialize: every input row, read on the first call, and the
    /// position of the next one to return.
    Materialized {
        rows: Option<Vec<Row>>,
        next: usize,
    },
}

impl State {
    fn start(node: &PlanNode) -> State {
        match node {
            PlanNode::SeqScan { table, .. } => State::Scan {
                scan: TableScan::new(table),
            },
            PlanNode::IndexScan { .. } => State::IndexScan {
                scan: None,
                left: Vec::new(),
            },
            PlanNode::OneRow => State::OneRow { done: false },
            PlanNode::Filter { .. } | PlanNode::Project { .. } | PlanNode::SubqueryScan { .. } => {
                State::None
            }
            PlanNode::Compound {
                op: SetOp::UnionAll,
                ..
            } => State::Append { left_done: false },
            PlanNode::Sort { .. } | PlanNode::Aggregate { .. } | PlanNode::Compound { .. } => {
                State::Buffered(None)
            }
            PlanNode::Distinct { .. } => State::Distinct(BTreeSet::new()),
            PlanNode::Limit { .. } => State::Limit {
                started: false,
                remaining: None,
            },
            PlanNode::NestedLoopJoin { .. } => State::Join {
                pair: None,
                left_width: 0,
                matched: false,
            },
            PlanNode::Materialize { .. } => State::Materialized {
                rows: None,
                next: 0,
            },
        }
    }
}

impl<'p> Operator<'p> {
    /// Builds the running tree, numbering steps in pre-order: a step, its
    /// inputs, then its subqueries.
    fn build(node: &'p PlanNode, next_id: &mut usize) -> Operator<'p> {
        let id = *next_id;
        *next_id += 1;
        let mut children = node.children().into_iter();
        let input = children
            .next()
            .map(|c| Box::new(Operator::build(c, next_id)));
        let right = children
            .next()
            .map(|c| Box::new(Operator::build(c, next_id)));
        let mut subqueries = Vec::new();
        for expr in node.exprs() {
            expr.walk(&mut |e| {
                if let Expr::Subquery { kind, query } = e {
                    subqueries.push(SubqueryOp::new(kind, query, next_id));
                }
            });
        }
        Operator {
            id,
            node,
            input,
            right,
            subqueries,
            state: State::start(node),
            rows_out: 0,
            rowid: None,
        }
    }

    /// Starts this step and its inputs over from the first row. Row counts
    /// keep adding up, so the plan shows the total work of every run.
    fn rewind(&mut self) {
        self.rewind_for(&[]);
    }

    /// Starts over for a join's left row, which a lookup below may read.
    fn rewind_for(&mut self, left: &[Value]) {
        self.state = State::start(self.node);
        if let State::IndexScan { left: row, .. } = &mut self.state {
            *row = left.to_vec();
        }
        for child in self.input.iter_mut().chain(self.right.iter_mut()) {
            child.rewind_for(left);
        }
    }

    /// Starts a join's right side over for the next left row. A Materialize
    /// step that has read its input already returns its rows again from
    /// memory; anything else starts over, and a lookup uses the new row.
    fn replay(&mut self, left: &[Value]) {
        match &mut self.state {
            State::Materialized {
                rows: Some(_),
                next,
            } => *next = 0,
            _ => self.rewind_for(left),
        }
    }

    fn next(&mut self, ctx: &mut ExecContext) -> Result<Option<Row>> {
        let row = self.produce(ctx)?;
        if let Some(row) = &row {
            ctx.charge()?;
            self.rows_out += 1;
            ctx.record(self.id, EventKind::Out, row, self.rowid.take());
        }
        Ok(row)
    }

    fn produce(&mut self, ctx: &mut ExecContext) -> Result<Option<Row>> {
        let id = self.id;
        let input = &mut self.input;
        let subqueries = &mut self.subqueries;
        let read = |rowid: &mut Option<i64>, entry: Option<(i64, Row)>| {
            entry.map(|(found, row)| {
                *rowid = Some(found);
                row
            })
        };
        match (self.node, &mut self.state) {
            (PlanNode::SeqScan { .. }, State::Scan { scan }) => {
                Ok(read(&mut self.rowid, scan.next_entry(ctx.storage)?))
            }
            (PlanNode::IndexScan { table, access, .. }, State::IndexScan { scan, left }) => {
                let scan = match scan {
                    Some(scan) => scan,
                    None => {
                        let mut env = StepEnv { ctx, subqueries };
                        scan.insert(access::Scan::start(table, access, left, &mut env)?)
                    }
                };
                Ok(read(&mut self.rowid, scan.next_entry(ctx.storage)?))
            }
            (PlanNode::OneRow, State::OneRow { done }) => {
                let first = !*done;
                *done = true;
                Ok(first.then(Vec::new))
            }
            (PlanNode::Filter { predicate, .. }, _) => {
                while let Some(row) = pull(input, ctx)? {
                    if evaluate(predicate, &row, ctx, subqueries)?.truthiness() == Some(true) {
                        return Ok(Some(row));
                    }
                    ctx.record(id, EventKind::Drop, &row, None);
                }
                Ok(None)
            }
            (PlanNode::Project { exprs, .. }, _) => match pull(input, ctx)? {
                Some(row) => Ok(Some(
                    exprs
                        .iter()
                        .map(|e| evaluate(e, &row, ctx, subqueries))
                        .collect::<Result<_>>()?,
                )),
                None => Ok(None),
            },
            (PlanNode::Sort { keys, .. }, State::Buffered(buffer)) => {
                if buffer.is_none() {
                    *buffer = Some(sort(input, ctx, subqueries, keys)?.into_iter());
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
                    let groups = aggregate(
                        input, ctx, subqueries, group_by, aggregates, *row_from, *width,
                    )?;
                    *buffer = Some(groups.into_iter());
                }
                Ok(buffer.as_mut().and_then(Iterator::next))
            }
            (PlanNode::Distinct { .. }, State::Distinct(seen)) => {
                while let Some(row) = pull(input, ctx)? {
                    if seen.insert(RowKey(row.clone())) {
                        return Ok(Some(row));
                    }
                    ctx.record(id, EventKind::Drop, &row, None);
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
                        match pull(input, ctx)? {
                            Some(row) => ctx.record(id, EventKind::Drop, &row, None),
                            None => break,
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
            (
                PlanNode::NestedLoopJoin {
                    kind,
                    condition,
                    right_width,
                    ..
                },
                State::Join {
                    pair,
                    left_width,
                    matched,
                },
            ) => {
                let right = self.right.as_mut().expect("a join has a right input");
                loop {
                    let row = match pair {
                        Some(row) => row,
                        None => {
                            let Some(row) = pull(input, ctx)? else {
                                return Ok(None);
                            };
                            *left_width = row.len();
                            *matched = false;
                            right.replay(&row);
                            pair.insert(row)
                        }
                    };
                    // The pair reuses one buffer: the left row stays, and
                    // each right row replaces the one before it.
                    row.truncate(*left_width);
                    match right.next(ctx)? {
                        Some(right_row) => {
                            row.extend(right_row);
                            let keep = match condition {
                                Some(c) => {
                                    evaluate(c, row, ctx, subqueries)?.truthiness() == Some(true)
                                }
                                None => true,
                            };
                            if keep {
                                *matched = true;
                                return Ok(Some(row.clone()));
                            }
                            ctx.record(id, EventKind::Drop, row, None);
                        }
                        // The right side has run out for this left row.
                        None => {
                            let mut row = pair.take().expect("the pair is set above");
                            if *kind == JoinKind::Left && !*matched {
                                row.extend(std::iter::repeat_n(Value::Null, *right_width));
                                return Ok(Some(row));
                            }
                        }
                    }
                }
            }
            (PlanNode::Compound { .. }, State::Append { left_done }) => {
                if !*left_done {
                    if let Some(row) = pull(input, ctx)? {
                        return Ok(Some(row));
                    }
                    *left_done = true;
                }
                pull(&mut self.right, ctx)
            }
            (PlanNode::Compound { op, .. }, State::Buffered(buffer)) => {
                if buffer.is_none() {
                    let rows = combine(*op, input, &mut self.right, ctx)?;
                    *buffer = Some(rows.into_iter());
                }
                Ok(buffer.as_mut().and_then(Iterator::next))
            }
            (PlanNode::SubqueryScan { .. }, _) => pull(input, ctx),
            (PlanNode::Materialize { .. }, State::Materialized { rows, next }) => {
                if rows.is_none() {
                    let mut all = Vec::new();
                    while let Some(row) = pull(input, ctx)? {
                        all.push(row);
                    }
                    *rows = Some(all);
                }
                let row = rows.as_ref().and_then(|rows| rows.get(*next)).cloned();
                *next += 1;
                Ok(row)
            }
            _ => unreachable!("an operator's state always matches its plan node"),
        }
    }

    fn profile(&self) -> PlanProfile {
        let mut children: Vec<PlanProfile> = self
            .input
            .iter()
            .chain(&self.right)
            .map(|i| i.profile())
            .collect();
        children.extend(self.subqueries.iter().map(SubqueryOp::profile));
        PlanProfile {
            id: self.id,
            operator: self.node.operator(),
            detail: self.node.detail(),
            reason: self.node.reason().map(str::to_string),
            choice: self.node.why().and_then(|why| {
                Some(Box::new(Choice {
                    plan: why.plan.clone(),
                    estimate: why.estimate?,
                    rejected: why.rejected.clone(),
                }))
            }),
            rows_out: self.rows_out,
            children,
        }
    }

    /// Every scan step below and including this one, with its table.
    fn scans<'s>(&'s self, out: &mut Vec<(usize, &'s str)>) {
        if let PlanNode::SeqScan { table, .. } | PlanNode::IndexScan { table, .. } = self.node {
            out.push((self.id, table));
        }
        for child in self.input.iter().chain(&self.right) {
            child.scans(out);
        }
        for subquery in &self.subqueries {
            subquery.root.scans(out);
        }
    }
}

/// A subquery inside one step's expressions, with its own running plan.
struct SubqueryOp<'p> {
    id: usize,
    query: &'p Subquery,
    /// "scalar", "EXISTS" or "IN", for the plan tree.
    kind: &'static str,
    root: Operator<'p>,
    /// An uncorrelated subquery's answer, kept after its first run.
    cached: Option<Rc<[Value]>>,
    runs: u64,
}

impl<'p> SubqueryOp<'p> {
    fn new(kind: &SubqueryKind, query: &'p Subquery, next_id: &mut usize) -> Self {
        let id = *next_id;
        *next_id += 1;
        SubqueryOp {
            id,
            query,
            kind: match kind {
                SubqueryKind::Scalar => "scalar",
                SubqueryKind::Exists { .. } => "EXISTS",
                SubqueryKind::In { .. } => "IN",
            },
            root: Operator::build(&query.plan.root, next_id),
            cached: None,
            runs: 0,
        }
    }

    /// The first column of the subquery's rows, run for the outer `row`.
    fn run(&mut self, ctx: &mut ExecContext, row: &[Value], all_rows: bool) -> Result<Rc<[Value]>> {
        if let Some(values) = &self.cached {
            ctx.record(self.id, EventKind::Reuse, row, None);
            return Ok(Rc::clone(values));
        }
        if self.runs > 0 {
            self.root.rewind();
        }
        self.runs += 1;
        ctx.record(self.id, EventKind::Run, row, None);
        ctx.outer.push(row.to_vec());
        let values = self.read(ctx, all_rows);
        ctx.outer.pop();
        let values: Rc<[Value]> = values?.into();
        if !self.query.correlated {
            self.cached = Some(Rc::clone(&values));
        }
        Ok(values)
    }

    fn read(&mut self, ctx: &mut ExecContext, all_rows: bool) -> Result<Vec<Value>> {
        let mut values = Vec::new();
        while let Some(row) = self.root.next(ctx)? {
            values.push(row.into_iter().next().unwrap_or(Value::Null));
            if !all_rows {
                break;
            }
        }
        Ok(values)
    }

    fn profile(&self) -> PlanProfile {
        let (detail, reason) = if self.query.correlated {
            (
                format!("{}, correlated", self.kind),
                "it reads a column of the outer row, so it runs again for every row",
            )
        } else {
            (
                self.kind.to_string(),
                "it reads nothing from the outer row, so it runs once and the answer is reused",
            )
        };
        PlanProfile {
            id: self.id,
            operator: "Subquery",
            detail,
            reason: Some(reason.into()),
            choice: None,
            // How many times it ran.
            rows_out: self.runs,
            children: vec![self.root.profile()],
        }
    }
}

/// What an expression in one step can reach: the outer rows, and the step's
/// subqueries.
struct StepEnv<'s, 'p, 'c> {
    ctx: &'s mut ExecContext<'c>,
    subqueries: &'s mut [SubqueryOp<'p>],
}

impl Env for StepEnv<'_, '_, '_> {
    fn outer(&self, depth: usize, index: usize) -> Result<Value> {
        let rows = &self.ctx.outer;
        match rows.len().checked_sub(depth).and_then(|i| rows.get(i)) {
            Some(row) => Ok(row.get(index).cloned().unwrap_or(Value::Null)),
            None => unsupported("a column of an enclosing query here"),
        }
    }

    fn subquery(&mut self, query: &Subquery, row: &[Value], all_rows: bool) -> Result<Rc<[Value]>> {
        match self
            .subqueries
            .iter_mut()
            .find(|op| ptr::eq(op.query, query))
        {
            Some(op) => op.run(self.ctx, row, all_rows),
            None => unsupported("a subquery in this step"),
        }
    }
}

/// Evaluates one of a step's expressions on a row.
fn evaluate(
    expr: &Expr,
    row: &[Value],
    ctx: &mut ExecContext,
    subqueries: &mut [SubqueryOp],
) -> Result<Value> {
    expr.eval_with(row, &mut StepEnv { ctx, subqueries })
}

/// UNION, INTERSECT and EXCEPT: reads both inputs and returns the answer
/// sorted, each row once.
///
/// Rows like `1` and `1.0` are equal but print differently. Which one
/// SQLite returns follows from its temporary index: within one side the
/// first of equal rows stays, and a UNION's right side overwrites an equal
/// row from the left. INTERSECT and EXCEPT return the left side's rows.
fn combine(
    op: SetOp,
    left: &mut Option<Box<Operator>>,
    right: &mut Option<Box<Operator>>,
    ctx: &mut ExecContext,
) -> Result<Vec<Row>> {
    let mut rows = BTreeSet::new();
    while let Some(row) = pull(left, ctx)? {
        rows.insert(RowKey(row));
    }
    let mut other = BTreeSet::new();
    while let Some(row) = pull(right, ctx)? {
        other.insert(RowKey(row));
    }
    let rows: BTreeSet<RowKey> = match op {
        SetOp::Union => {
            for key in other {
                rows.replace(key);
            }
            rows
        }
        SetOp::Intersect => rows.into_iter().filter(|k| other.contains(k)).collect(),
        SetOp::Except => rows.into_iter().filter(|k| !other.contains(k)).collect(),
        SetOp::UnionAll => unreachable!("UNION ALL appends instead"),
    };
    Ok(rows.into_iter().map(|key| key.0).collect())
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
    subqueries: &mut [SubqueryOp],
    keys: &[SortKey],
) -> Result<Vec<Row>> {
    let mut keyed = Vec::new();
    while let Some(row) = pull(input, ctx)? {
        let values = keys
            .iter()
            .map(|k| evaluate(&k.expr, &row, ctx, subqueries))
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
    subqueries: &mut [SubqueryOp],
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
                .map(|k| evaluate(&k.expr, &row, ctx, subqueries))
                .collect::<Result<_>>()?,
        );
        let mut args = Vec::with_capacity(aggregates.len());
        for a in aggregates {
            args.push(
                a.args
                    .iter()
                    .map(|e| evaluate(e, &row, ctx, subqueries))
                    .collect::<Result<Vec<Value>>>()?,
            );
        }
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
    let mut op = Operator::build(root, &mut 0);
    let mut rows = Vec::new();
    while let Some(row) = op.next(ctx)? {
        rows.push(row);
    }
    if ctx.trace.is_some() {
        record_scans(ctx, &op)?;
    }
    Ok((rows, op.profile()))
}

/// Adds every scanned table's rows to the trace, so it can show the rows a
/// scan never read.
fn record_scans(ctx: &mut ExecContext, op: &Operator) -> Result<()> {
    let mut scans = Vec::new();
    op.scans(&mut scans);
    for (step, table) in scans {
        let row_count = ctx.storage.row_count(table);
        let (mut rowids, mut rows) = (Vec::new(), Vec::new());
        if row_count <= MAX_SCAN_ROWIDS {
            let mut scan = TableScan::new(table);
            while let Some((rowid, row)) = scan.next_entry(ctx.storage)? {
                rowids.push(rowid);
                if row_count <= MAX_SCAN_ROWS {
                    rows.push(row);
                }
            }
        }
        if let Some(recorder) = &mut ctx.trace {
            recorder.add_scan(ScanRows {
                step,
                table: table.to_string(),
                row_count,
                rowids,
                rows,
            });
        }
    }
    Ok(())
}
