//! Plans: the tree of steps the executor will run for a statement.

use serde::Serialize;

use crate::catalog::TableSchema;
use crate::expr::Expr;

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
}

impl PlanNode {
    pub fn operator(&self) -> &'static str {
        match self {
            PlanNode::SeqScan { .. } => "SeqScan",
            PlanNode::OneRow => "OneRow",
            PlanNode::Filter { .. } => "Filter",
            PlanNode::Project { .. } => "Project",
        }
    }

    pub fn detail(&self) -> String {
        match self {
            PlanNode::SeqScan { table, .. } => table.clone(),
            PlanNode::OneRow => String::new(),
            PlanNode::Filter { predicate, .. } => predicate.to_string(),
            PlanNode::Project { exprs, .. } => exprs
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", "),
        }
    }

    /// Why the planner chose this step, when there was a choice to explain.
    pub fn reason(&self) -> Option<&str> {
        match self {
            PlanNode::SeqScan { reason, .. } => Some(reason),
            _ => None,
        }
    }

    pub fn children(&self) -> Vec<&PlanNode> {
        match self {
            PlanNode::SeqScan { .. } | PlanNode::OneRow => vec![],
            PlanNode::Filter { input, .. } | PlanNode::Project { input, .. } => vec![input],
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
