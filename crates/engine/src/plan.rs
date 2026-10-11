//! Plans: the tree of steps the executor will run for a statement.

use std::fmt;

use serde::Serialize;

use crate::catalog::TableSchema;
use crate::expr::{AggregateCall, Expr};

/// What a statement will do, decided before any row is touched.
#[derive(Debug, Clone)]
pub enum Plan {
    CreateTable {
        schema: TableSchema,
        if_not_exists: bool,
    },
    Insert {
        table: String,
        /// For each value in a VALUES row, the column it fills.
        targets: Vec<usize>,
        rows: Vec<Vec<Expr>>,
    },
    Query(QueryPlan),
}

#[derive(Debug, Clone)]
pub struct QueryPlan {
    pub columns: Vec<String>,
    pub root: PlanNode,
}

/// One step in a query plan. Each step pulls rows from its input.
#[derive(Debug, Clone)]
pub enum PlanNode {
    /// Reads every row of a table, in storage order.
    SeqScan { table: String, reason: String },
    /// Produces one empty row, for `SELECT` without `FROM`.
    OneRow,
    /// Passes on only the rows where the predicate is true.
    Filter {
        input: Box<PlanNode>,
        predicate: Expr,
    },
    /// Computes the output columns from each input row.
    Project {
        input: Box<PlanNode>,
        exprs: Vec<Expr>,
    },
    /// Splits rows into groups with equal `group_by` values (one group when
    /// there is no GROUP BY) and runs the aggregates over each group. Groups
    /// come out sorted by their values, in each key's direction.
    ///
    /// Each output row is one input row from the group followed by one value
    /// per aggregate, so expressions above can still read plain columns. The
    /// input row is the group's first, or the row a `min()`/`max()` chose
    /// (the aggregate at index `row_from`), as in SQLite.
    Aggregate {
        input: Box<PlanNode>,
        group_by: Vec<SortKey>,
        aggregates: Vec<AggregateCall>,
        row_from: Option<usize>,
        /// Columns in an input row, for the all-NULL row of an empty table.
        width: usize,
    },
    /// Reads all its input, then returns it in order. Rows that tie keep
    /// their input order.
    Sort {
        input: Box<PlanNode>,
        keys: Vec<SortKey>,
        reason: String,
    },
    /// Drops rows equal to one it has already returned.
    Distinct { input: Box<PlanNode> },
    /// Skips `offset` rows, then returns at most `limit`.
    Limit {
        input: Box<PlanNode>,
        limit: Expr,
        offset: Option<Expr>,
    },
}

/// One ORDER BY term.
#[derive(Debug, Clone)]
pub struct SortKey {
    pub expr: Expr,
    pub descending: bool,
    pub nulls_first: bool,
}

impl fmt::Display for SortKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.expr)?;
        if self.descending {
            f.write_str(" DESC")?;
        }
        // NULLs come first ascending and last descending unless told otherwise.
        match (self.descending, self.nulls_first) {
            (false, false) => f.write_str(" NULLS LAST"),
            (true, true) => f.write_str(" NULLS FIRST"),
            _ => Ok(()),
        }
    }
}

fn joined<T: ToString>(items: &[T]) -> String {
    items
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

impl PlanNode {
    pub fn operator(&self) -> &'static str {
        match self {
            PlanNode::SeqScan { .. } => "SeqScan",
            PlanNode::OneRow => "OneRow",
            PlanNode::Filter { .. } => "Filter",
            PlanNode::Project { .. } => "Project",
            PlanNode::Aggregate { .. } => "Aggregate",
            PlanNode::Sort { .. } => "Sort",
            PlanNode::Distinct { .. } => "Distinct",
            PlanNode::Limit { .. } => "Limit",
        }
    }

    pub fn detail(&self) -> String {
        match self {
            PlanNode::SeqScan { table, .. } => table.clone(),
            PlanNode::OneRow | PlanNode::Distinct { .. } => String::new(),
            PlanNode::Filter { predicate, .. } => predicate.to_string(),
            PlanNode::Project { exprs, .. } => joined(exprs),
            PlanNode::Aggregate {
                group_by,
                aggregates,
                ..
            } => match (aggregates.is_empty(), group_by.is_empty()) {
                (_, true) => joined(aggregates),
                (true, false) => format!("GROUP BY {}", joined(group_by)),
                (false, false) => {
                    format!("{} GROUP BY {}", joined(aggregates), joined(group_by))
                }
            },
            PlanNode::Sort { keys, .. } => joined(keys),
            PlanNode::Limit { limit, offset, .. } => match offset {
                Some(offset) => format!("{limit} OFFSET {offset}"),
                None => limit.to_string(),
            },
        }
    }

    /// Why the planner chose this step, when there was a choice to explain.
    pub fn reason(&self) -> Option<&str> {
        match self {
            PlanNode::SeqScan { reason, .. } | PlanNode::Sort { reason, .. } => Some(reason),
            _ => None,
        }
    }

    pub fn children(&self) -> Vec<&PlanNode> {
        match self {
            PlanNode::SeqScan { .. } | PlanNode::OneRow => vec![],
            PlanNode::Filter { input, .. }
            | PlanNode::Project { input, .. }
            | PlanNode::Aggregate { input, .. }
            | PlanNode::Sort { input, .. }
            | PlanNode::Distinct { input }
            | PlanNode::Limit { input, .. } => vec![input],
        }
    }
}

/// A plan node after execution, with what actually happened. This is what
/// the UI draws and what `EXPLAIN ANALYZE` would print in other databases.
#[derive(Debug, Clone, Serialize)]
pub struct PlanProfile {
    /// Pre-order position in the tree, stable for a given plan.
    pub id: usize,
    pub operator: &'static str,
    pub detail: String,
    pub reason: Option<String>,
    /// Rows this step passed to its parent.
    pub rows_out: u64,
    pub children: Vec<PlanProfile>,
}

impl PlanProfile {
    /// Renders the tree as indented text, for the CLI.
    pub fn render(&self) -> String {
        let mut out = String::new();
        self.render_into(&mut out, 0);
        out
    }

    fn render_into(&self, out: &mut String, depth: usize) {
        let indent = "  ".repeat(depth);
        let detail = if self.detail.is_empty() {
            String::new()
        } else {
            format!(" {}", self.detail)
        };
        out.push_str(&format!(
            "{indent}{}{detail}  [rows={}]\n",
            self.operator, self.rows_out
        ));
        if let Some(reason) = &self.reason {
            out.push_str(&format!("{indent}  why: {reason}\n"));
        }
        for child in &self.children {
            child.render_into(out, depth + 1);
        }
    }
}
