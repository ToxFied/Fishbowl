//! Plans: the tree of steps the executor will run for a statement.

use std::fmt;

use serde::Serialize;

use crate::access::IndexAccess;
use crate::catalog::{IndexSchema, TableSchema};
use crate::expr::{AggregateCall, Expr};
use crate::value::Affinity;
use crate::write::OnConflict;

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
        on_conflict: OnConflict,
    },
    Update {
        table: String,
        /// Column position and its new value, computed from the old row.
        assignments: Vec<(usize, Expr)>,
        predicate: Option<Expr>,
    },
    Delete {
        table: String,
        predicate: Option<Expr>,
    },
    DropTable {
        table: String,
        if_exists: bool,
    },
    CreateView {
        view: crate::catalog::View,
        if_not_exists: bool,
        /// A TEMP view lasts until the database is closed and is not saved.
        temporary: bool,
    },
    DropView {
        view: String,
        if_exists: bool,
    },
    Query(QueryPlan),
    CreateIndex {
        table: String,
        index: IndexSchema,
        if_not_exists: bool,
    },
    DropIndex {
        name: String,
        if_exists: bool,
    },
    /// `INSERT INTO table SELECT ...`: the query's rows, stored like VALUES.
    InsertSelect {
        table: String,
        targets: Vec<usize>,
        query: QueryPlan,
        on_conflict: OnConflict,
    },
}

#[derive(Debug, Clone)]
pub struct QueryPlan {
    pub columns: Vec<String>,
    /// The affinity each result column brings to a comparison, as
    /// [`Expr::affinity`] gives it. A view or a subquery passes it on.
    pub affinities: Vec<Option<Affinity>>,
    pub root: PlanNode,
}

/// A SELECT inside an expression.
#[derive(Debug, Clone)]
pub struct Subquery {
    pub plan: QueryPlan,
    /// Whether it reads a column of an enclosing query. A correlated
    /// subquery runs again for every row; any other runs once.
    pub correlated: bool,
    /// The SQL text, for display.
    pub sql: String,
}

/// Two subqueries are the same expression when they are the same SQL.
impl PartialEq for Subquery {
    fn eq(&self, other: &Self) -> bool {
        self.sql == other.sql
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    /// Inner and cross joins: only pairs that meet the condition.
    Inner,
    /// LEFT JOIN: also every left row that matched nothing, padded with NULLs.
    Left,
}

/// How a compound SELECT combines its two sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOp {
    UnionAll,
    Union,
    Intersect,
    Except,
}

impl SetOp {
    pub fn keyword(self) -> &'static str {
        match self {
            SetOp::UnionAll => "UNION ALL",
            SetOp::Union => "UNION",
            SetOp::Intersect => "INTERSECT",
            SetOp::Except => "EXCEPT",
        }
    }
}

/// One step in a query plan. Each step pulls rows from its input.
#[derive(Debug, Clone)]
pub enum PlanNode {
    /// Reads every row of a table, in storage order.
    SeqScan { table: String, reason: String },
    /// Reads only the rows an index points to, in index order. A Filter
    /// above it still checks the whole WHERE clause.
    IndexScan {
        table: String,
        access: IndexAccess,
        reason: String,
    },
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
    /// For each row from `left`, reads every row of `right` again and passes
    /// on the joined pairs where `condition` holds. Each output row is the
    /// left row followed by the right row.
    NestedLoopJoin {
        left: Box<PlanNode>,
        right: Box<PlanNode>,
        kind: JoinKind,
        condition: Option<Expr>,
        /// Columns in a right row, for the NULLs of an unmatched LEFT JOIN row.
        right_width: usize,
        reason: String,
    },
    /// Combines the rows of two queries. UNION ALL passes on both inputs in
    /// turn; the others drop duplicate rows and return the rest sorted, the
    /// way SQLite's temporary index returns them.
    Compound {
        op: SetOp,
        left: Box<PlanNode>,
        right: Box<PlanNode>,
    },
    /// The rows of a view or a subquery in FROM, under the name the outer
    /// query uses for them.
    SubqueryScan { input: Box<PlanNode>, name: String },
    /// Reads all its input once and keeps the rows in memory, so a join can
    /// read its right side again for every left row without redoing the work.
    Materialize { input: Box<PlanNode> },
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
            PlanNode::IndexScan { .. } => "IndexScan",
            PlanNode::OneRow => "OneRow",
            PlanNode::Filter { .. } => "Filter",
            PlanNode::Project { .. } => "Project",
            PlanNode::Aggregate { .. } => "Aggregate",
            PlanNode::Sort { .. } => "Sort",
            PlanNode::Distinct { .. } => "Distinct",
            PlanNode::Limit { .. } => "Limit",
            PlanNode::NestedLoopJoin { .. } => "NestedLoopJoin",
            PlanNode::Compound { op, .. } => match op {
                SetOp::UnionAll => "UnionAll",
                SetOp::Union => "Union",
                SetOp::Intersect => "Intersect",
                SetOp::Except => "Except",
            },
            PlanNode::SubqueryScan { .. } => "SubqueryScan",
            PlanNode::Materialize { .. } => "Materialize",
        }
    }

    pub fn detail(&self) -> String {
        match self {
            PlanNode::SeqScan { table, .. } => table.clone(),
            PlanNode::IndexScan { table, access, .. } => format!("{table} {access}"),
            PlanNode::OneRow | PlanNode::Distinct { .. } | PlanNode::Materialize { .. } => {
                String::new()
            }
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
            PlanNode::NestedLoopJoin {
                kind, condition, ..
            } => match (kind, condition) {
                (JoinKind::Inner, None) => "CROSS".into(),
                (JoinKind::Inner, Some(c)) => format!("INNER ON {c}"),
                (JoinKind::Left, None) => "LEFT".into(),
                (JoinKind::Left, Some(c)) => format!("LEFT ON {c}"),
            },
            PlanNode::Compound { .. } => String::new(),
            PlanNode::SubqueryScan { name, .. } => name.clone(),
        }
    }

    /// Why the planner chose this step, when there was a choice to explain.
    pub fn reason(&self) -> Option<&str> {
        match self {
            PlanNode::SeqScan { reason, .. }
            | PlanNode::IndexScan { reason, .. }
            | PlanNode::Sort { reason, .. }
            | PlanNode::NestedLoopJoin { reason, .. } => Some(reason),
            _ => None,
        }
    }

    pub fn children(&self) -> Vec<&PlanNode> {
        match self {
            PlanNode::SeqScan { .. } | PlanNode::IndexScan { .. } | PlanNode::OneRow => vec![],
            PlanNode::Filter { input, .. }
            | PlanNode::Project { input, .. }
            | PlanNode::Aggregate { input, .. }
            | PlanNode::Sort { input, .. }
            | PlanNode::Distinct { input }
            | PlanNode::Limit { input, .. }
            | PlanNode::SubqueryScan { input, .. }
            | PlanNode::Materialize { input } => vec![input],
            PlanNode::NestedLoopJoin { left, right, .. }
            | PlanNode::Compound { left, right, .. } => {
                vec![left, right]
            }
        }
    }

    /// The expressions this step evaluates, where the executor looks for
    /// subqueries to run.
    pub fn exprs(&self) -> Vec<&Expr> {
        match self {
            PlanNode::Filter { predicate, .. } => vec![predicate],
            PlanNode::Project { exprs, .. } => exprs.iter().collect(),
            PlanNode::Aggregate {
                group_by,
                aggregates,
                ..
            } => group_by
                .iter()
                .map(|k| &k.expr)
                .chain(aggregates.iter().flat_map(|a| &a.args))
                .collect(),
            PlanNode::Sort { keys, .. } => keys.iter().map(|k| &k.expr).collect(),
            PlanNode::Limit { limit, offset, .. } => std::iter::once(limit).chain(offset).collect(),
            PlanNode::NestedLoopJoin { condition, .. } => condition.iter().collect(),
            // An index lookup's values are constants, worked out when planning.
            PlanNode::SeqScan { .. }
            | PlanNode::IndexScan { .. }
            | PlanNode::OneRow
            | PlanNode::Distinct { .. }
            | PlanNode::Compound { .. }
            | PlanNode::SubqueryScan { .. }
            | PlanNode::Materialize { .. } => vec![],
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
