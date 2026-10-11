//! Runs query plans using the iterator ("Volcano") model: every step has a
//! `next()` that asks its input for a row, does its work, and hands one row up.
//! Rows flow one at a time from the scan at the bottom to the root.

use crate::error::{Error, Result};
use crate::expr::Expr;
use crate::plan::{PlanNode, PlanProfile};
use crate::storage::{Row, Storage, TableScan};

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

/// A plan node that is running. It mirrors [`PlanNode`] and adds cursor state
/// and a count of rows produced.
struct Operator {
    kind: OperatorKind,
    rows_out: u64,
}

enum OperatorKind {
    SeqScan {
        scan: TableScan,
    },
    OneRow {
        done: bool,
    },
    Filter {
        input: Box<Operator>,
        predicate: Expr,
    },
    Project {
        input: Box<Operator>,
        exprs: Vec<Expr>,
    },
}

impl Operator {
    fn build(node: &PlanNode) -> Operator {
        let kind = match node {
            PlanNode::SeqScan { table, .. } => OperatorKind::SeqScan {
                scan: TableScan::new(table),
            },
            PlanNode::OneRow => OperatorKind::OneRow { done: false },
            PlanNode::Filter { input, predicate } => OperatorKind::Filter {
                input: Box::new(Operator::build(input)),
                predicate: predicate.clone(),
            },
            PlanNode::Project { input, exprs } => OperatorKind::Project {
                input: Box::new(Operator::build(input)),
                exprs: exprs.clone(),
            },
        };
        Operator { kind, rows_out: 0 }
    }

    fn next(&mut self, ctx: &mut ExecContext) -> Result<Option<Row>> {
        let row = match &mut self.kind {
            OperatorKind::SeqScan { scan } => scan.next(ctx.storage)?,
            OperatorKind::OneRow { done } => {
                let first = !*done;
                *done = true;
                first.then(Vec::new)
            }
            OperatorKind::Filter { input, predicate } => loop {
                match input.next(ctx)? {
                    Some(row) if predicate.eval(&row).truthiness() == Some(true) => {
                        break Some(row);
                    }
                    Some(_) => continue,
                    None => break None,
                }
            },
            OperatorKind::Project { input, exprs } => input
                .next(ctx)?
                .map(|row| exprs.iter().map(|e| e.eval(&row)).collect()),
        };
        if row.is_some() {
            ctx.charge()?;
            self.rows_out += 1;
        }
        Ok(row)
    }

    fn profile(&self, node: &PlanNode, next_id: &mut usize) -> PlanProfile {
        let id = *next_id;
        *next_id += 1;
        let children = match (&self.kind, node.children().as_slice()) {
            (OperatorKind::Filter { input, .. } | OperatorKind::Project { input, .. }, [child]) => {
                vec![input.profile(child, next_id)]
            }
            _ => vec![],
        };
        PlanProfile {
            id,
            operator: node.operator(),
            detail: node.detail(),
            reason: node.reason().map(str::to_string),
            rows_out: self.rows_out,
            children,
        }
    }
}

/// Runs a query plan to completion and returns its rows plus the plan tree
/// annotated with how many rows each step produced.
pub fn run_query(ctx: &mut ExecContext, root: &PlanNode) -> Result<(Vec<Row>, PlanProfile)> {
    let mut op = Operator::build(root);
    let mut rows = Vec::new();
    while let Some(row) = op.next(ctx)? {
        rows.push(row);
    }
    let profile = op.profile(root, &mut 0);
    Ok((rows, profile))
}
