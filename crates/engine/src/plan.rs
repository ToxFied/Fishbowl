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
        hint: IndexHint,
        /// Column position and its new value, computed from the old row.
        assignments: Vec<(usize, Expr)>,
        predicate: Option<Expr>,
    },
    Delete {
        table: String,
        hint: IndexHint,
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
    /// `ANALYZE`, `ANALYZE table` or `ANALYZE index`: count rows and values
    /// so the planner can use real sizes. These tables, or all of them.
    Analyze {
        tables: Option<Vec<String>>,
    },
    /// `REINDEX`: the name checked out and there is nothing to rebuild.
    Reindex,
    /// `INSERT INTO table SELECT ...`: the query's rows, stored like VALUES.
    InsertSelect {
        table: String,
        targets: Vec<usize>,
        query: QueryPlan,
        on_conflict: OnConflict,
    },
}

/// What `INDEXED BY` or `NOT INDEXED` after a table name lets the planner
/// read the table through.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IndexHint {
    #[default]
    Any,
    /// `NOT INDEXED`: the table itself or a rowid lookup, never an index.
    NoIndex,
    /// `INDEXED BY`: only this index, by its place in the table's list.
    Index(usize),
}

#[derive(Debug, Clone)]
pub struct QueryPlan {
    pub columns: Vec<String>,
    /// The affinity each result column brings to a comparison, as
    /// [`Expr::affinity`] gives it. A view or a subquery passes it on.
    pub affinities: Vec<Option<Affinity>>,
    pub root: PlanNode,
    /// What the planner expects the whole query to produce and cost, for a
    /// query that reads this one in FROM.
    pub estimate: Estimate,
}

/// The planner's guess at what a step will do.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Estimate {
    /// Rows it will pass on.
    pub rows: f64,
    /// Work, in rows read: a table row read in a scan costs 3.
    pub cost: f64,
}

/// A plan the planner considered and turned down, with its estimate.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Rejected {
    pub plan: String,
    pub rows: f64,
    pub cost: f64,
}

/// What the planner weighed for one step, as the profile reports it.
#[derive(Debug, Clone, Serialize)]
pub struct Choice {
    /// The plan chosen here in one line, to set beside the rejected ones.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// What the planner expected this step to produce and cost.
    pub estimate: Estimate,
    /// Plans for this step it turned down, cheapest first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rejected: Vec<Rejected>,
}

/// Why a step is in the plan: a sentence, and when there was a choice of
/// costed alternatives, the estimate for this one and the ones turned down,
/// cheapest first.
#[derive(Debug, Clone, Default)]
pub struct Why {
    pub reason: String,
    /// The chosen plan in one line, when it is compared with others.
    pub plan: Option<String>,
    pub estimate: Option<Estimate>,
    pub rejected: Vec<Rejected>,
}

impl Why {
    pub fn new(reason: impl Into<String>) -> Why {
        Why {
            reason: reason.into(),
            ..Why::default()
        }
    }
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
    SeqScan { table: String, why: Why },
    /// Reads only the rows an index points to, in index order. A Filter
    /// above it still checks the whole WHERE clause. In a join, the lookup
    /// can use values from the join's current left row.
    IndexScan {
        table: String,
        access: IndexAccess,
        why: Why,
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
        why: Why,
    },
    /// Drops rows equal to one it has already returned.
    Distinct { input: Box<PlanNode> },
    /// Skips `offset` rows, then returns at most `limit`.
    Limit {
        input: Box<PlanNode>,
        limit: Expr,
        offset: Option<Expr>,
    },
    /// For each row from `left`, reads `right` again and passes on the
    /// joined pairs where `condition` holds. Each output row is the left row
    /// followed by the right row. The right side is either a Materialize
    /// step, replayed from memory, or a lookup that uses the left row.
    NestedLoopJoin {
        left: Box<PlanNode>,
        right: Box<PlanNode>,
        kind: JoinKind,
        condition: Option<Expr>,
        /// Columns in a right row, for the NULLs of an unmatched LEFT JOIN row.
        right_width: usize,
        why: Why,
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
    pub fn why(&self) -> Option<&Why> {
        match self {
            PlanNode::SeqScan { why, .. }
            | PlanNode::IndexScan { why, .. }
            | PlanNode::Sort { why, .. }
            | PlanNode::NestedLoopJoin { why, .. } => Some(why),
            _ => None,
        }
    }

    pub fn reason(&self) -> Option<&str> {
        self.why().map(|why| why.reason.as_str())
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
            // A lookup's values, which may read an enclosing query's row.
            PlanNode::IndexScan { access, .. } => access.exprs(),
            PlanNode::SeqScan { .. }
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
    /// The planner's estimate and the plans it turned down, for a step
    /// where it compared costs. Its fields sit beside the others in JSON.
    #[serde(flatten)]
    pub choice: Option<Box<Choice>>,
    /// Rows this step passed to its parent.
    pub rows_out: u64,
    pub children: Vec<PlanProfile>,
}

/// An estimate as the CLI prints it: a whole number.
fn round(n: f64) -> String {
    format!("{:.0}", n.max(0.0))
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
        if let Some(choice) = &self.choice {
            out.push_str(&format!(
                "{indent}  estimate: rows={} cost={}\n",
                round(choice.estimate.rows),
                round(choice.estimate.cost)
            ));
            for r in &choice.rejected {
                out.push_str(&format!(
                    "{indent}  rejected: {}  [rows={} cost={}]\n",
                    r.plan,
                    round(r.rows),
                    round(r.cost)
                ));
            }
        }
        for child in &self.children {
            child.render_into(out, depth + 1);
        }
    }
}
