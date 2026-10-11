//! Access paths for one table: every way to read its rows, what each costs,
//! and the cheapest.
//!
//! The ways are a full scan, a lookup in the table's own B-tree by its
//! INTEGER PRIMARY KEY, and for each index, a lookup on its longest usable
//! prefix: equalities (or `IN` lists) on its first columns, then a range on
//! the next. An index can also be read whole, when it holds every column the
//! query reads (a covering index) or when its order saves a sort.
//!
//! A term's other side may read tables joined earlier, so the same lookup
//! runs once per row of the join's left side with that row's values.

use std::collections::BTreeSet;

use crate::access::{Convert, Covering, IndexAccess, KeyBound, KeyValue};
use crate::catalog::{IndexSchema, TableSchema};
use crate::expr::{BinaryOp, Expr};
use crate::plan::{Estimate, IndexHint, PlanNode, Rejected, Why};
use crate::value::{Affinity, Value};

use super::cost::{self, Sizes, TABLE_ROW};

/// A set of FROM items, by their place in FROM. SQLite allows at most 64
/// tables in a join for the same reason.
pub(super) type Mask = u64;

/// What one condition says about one column of one table.
#[derive(Debug, Clone)]
pub(super) struct Term {
    /// The column's place in its table's rows.
    pub column: usize,
    pub op: Op,
    /// The condition it came from.
    pub conjunct: usize,
    /// The FROM items the other side reads. 0 for a constant, or a value
    /// from an enclosing query, which is fixed while this query runs.
    pub needs: Mask,
    pub convert: Convert,
}

#[derive(Debug, Clone)]
pub(super) enum Op {
    Eq(Expr),
    /// `IN (...)` of constants, converted, sorted and without repeats.
    In(Vec<Value>),
    Low(Expr, bool),
    High(Expr, bool),
}

/// Reads the terms a condition gives table `table`, whose columns sit at
/// `offset..offset + width` in a joined row. `tables_of` tells which FROM
/// items an expression reads.
pub(super) fn terms_of(
    conjunct: usize,
    expr: &Expr,
    schema: &TableSchema,
    offset: usize,
    tables_of: &dyn Fn(&Expr) -> Option<Mask>,
) -> Vec<Term> {
    let width = schema.columns.len();
    let column_of = |e: &Expr| match e {
        Expr::Column { index, .. } if (offset..offset + width).contains(index) => {
            Some(*index - offset)
        }
        _ => None,
    };
    // The other side must not read this table, and is worked out once per
    // scan, so it may not hold a subquery.
    let other = |e: &Expr| {
        let needs = tables_of(e)?;
        let reads_self = super::columns_read(e).any(|c| (offset..offset + width).contains(&c));
        (!reads_self).then_some(needs)
    };
    let term = |column: usize, op: Op, other_side: &Expr, needs: Mask| {
        let convert = convert_for(schema.columns[column].affinity, other_side.affinity())?;
        Some(Term {
            column,
            op,
            conjunct,
            needs,
            convert,
        })
    };
    let mut terms = Vec::new();
    match expr {
        Expr::Binary { op, left, right } => {
            for (col, value, op) in [(left, right, Some(*op)), (right, left, flip(*op))] {
                let (Some(column), Some(op)) = (column_of(col), op) else {
                    continue;
                };
                let Some(needs) = other(value) else {
                    continue;
                };
                let v = (**value).clone();
                let op = match op {
                    BinaryOp::Eq => Op::Eq(v),
                    BinaryOp::Gt => Op::Low(v, false),
                    BinaryOp::GtEq => Op::Low(v, true),
                    BinaryOp::Lt => Op::High(v, false),
                    BinaryOp::LtEq => Op::High(v, true),
                    _ => continue,
                };
                terms.extend(term(column, op, value, needs));
                // `a = b` on two columns of one table says nothing usable.
                break;
            }
        }
        Expr::Between {
            expr,
            low,
            high,
            negated: false,
        } => {
            if let Some(column) = column_of(expr) {
                for (bound, is_low) in [(low, true), (high, false)] {
                    let Some(needs) = other(bound) else { continue };
                    let v = (**bound).clone();
                    let op = if is_low {
                        Op::Low(v, true)
                    } else {
                        Op::High(v, true)
                    };
                    terms.extend(term(column, op, bound, needs));
                }
            }
        }
        Expr::InList {
            expr,
            list,
            negated: false,
        } => {
            // The list's values have no affinity: the column's decides.
            let values: Option<Vec<Value>> = list.iter().map(constant).collect();
            if let (Some(column), Some(values)) = (column_of(expr), values) {
                let convert = convert_for(schema.columns[column].affinity, None)
                    .expect("a column's own affinity always converts");
                let mut values: Vec<Value> = values
                    .into_iter()
                    .filter(|v| !v.is_null())
                    .map(|v| convert_value(convert, v))
                    .collect();
                values.sort_by(|a, b| a.compare(b));
                values.dedup_by(|a, b| a.compare(b).is_eq());
                terms.push(Term {
                    column,
                    op: Op::In(values),
                    conjunct,
                    needs: 0,
                    convert,
                });
            }
        }
        _ => {}
    }
    terms
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

/// The value of an expression that reads no column, such as `5` or `-2.5`.
fn constant(expr: &Expr) -> Option<Value> {
    let mut fixed = true;
    expr.walk(&mut |e| {
        fixed &= !matches!(
            e,
            Expr::Column { .. } | Expr::OuterColumn { .. } | Expr::Subquery { .. }
        );
    });
    if fixed { expr.eval(&[]).ok() } else { None }
}

/// How the comparison `column = other` converts the other side's value, so
/// the lookup finds what the comparison would match, following the rules in
/// [`crate::expr::compare`]. `None` when it would convert the column's
/// values instead, which reorders them: a TEXT column holding `'5'` equals
/// the INTEGER 5, but the index keeps it among the text.
fn convert_for(column: Affinity, other: Option<Affinity>) -> Option<Convert> {
    let numeric = |a| matches!(a, Affinity::Integer | Affinity::Real | Affinity::Numeric);
    match other {
        Some(b) if numeric(column) || numeric(b) => numeric(column).then_some(Convert::Numeric),
        Some(_) => Some(Convert::AsIs),
        None if numeric(column) => Some(Convert::Numeric),
        // A TEXT column holds only text, so the other side becomes text.
        None if column == Affinity::Text => Some(Convert::Text),
        None => Some(Convert::AsIs),
    }
}

fn convert_value(convert: Convert, v: Value) -> Value {
    match convert {
        Convert::Numeric => v.apply_affinity(Affinity::Numeric),
        Convert::Text => v.apply_affinity(Affinity::Text),
        Convert::AsIs => v,
    }
}

/// An order the query would like rows in, as columns of this table, so a
/// path that already returns rows that way saves a sort.
#[derive(Debug, Clone)]
pub(super) struct Wanted {
    pub columns: Vec<usize>,
    /// GROUP BY and DISTINCT take the columns in any order.
    pub any_order: bool,
    /// The result columns, for the cost of a sort.
    pub width: usize,
    /// For ORDER BY, which an ordered path saves a Sort step. GROUP BY,
    /// DISTINCT and compound SELECTs gain nothing in Fishbowl, but SQLite
    /// reads rows this way, and that decides which of two equal values
    /// (`2`, `2.0`) a group shows.
    pub sorts: bool,
}

/// One way to read a table.
#[derive(Debug, Clone)]
pub(super) struct Path {
    pub kind: Kind,
    /// Rows one run reads, before the conditions it did not use.
    pub rows: f64,
    /// The work of one run.
    pub cost: f64,
    /// FROM items whose current row the lookup uses.
    pub needs: Mask,
    /// Whether rows come out in the wanted order.
    pub ordered: bool,
}

#[derive(Debug, Clone)]
pub(super) enum Kind {
    Seq,
    /// `index` is `None` for the rowid. `eq`, `low` and `high` are terms.
    Index {
        index: Option<usize>,
        eq: Vec<usize>,
        low: Vec<usize>,
        high: Vec<usize>,
        covering: bool,
    },
}

/// The table a path reads, and what the query needs of it.
pub(super) struct Target<'a> {
    pub sizes: Sizes<'a>,
    pub hint: IndexHint,
    /// The name the query uses for it.
    pub name: &'a str,
    pub terms: &'a [Term],
    /// The conditions on this table alone, and the share of rows each keeps.
    pub own: &'a [(usize, f64)],
    /// Columns the query reads, or `None` for all (UPDATE and DELETE).
    pub used: Option<&'a BTreeSet<usize>>,
    pub wanted: Option<&'a Wanted>,
}

impl Target<'_> {
    fn schema(&self) -> &TableSchema {
        self.sizes.table
    }

    /// Every path whose terms use only the items in `available`, in the
    /// order SQLite tries them, which settles ties: the full scan, the
    /// rowid, then each index from the newest. `INDEXED BY` leaves only the
    /// paths through its index, the full scan of it included, and `NOT
    /// INDEXED` only the table and the rowid.
    pub fn paths(&self, available: Mask) -> Vec<Path> {
        let forced = match self.hint {
            IndexHint::Index(i) => Some(i),
            _ => None,
        };
        let schema = self.schema();
        let n = self.sizes.rows();
        // Order only matters when one is wanted; joins skip the bookkeeping.
        let fixed: BTreeSet<usize> = match self.wanted {
            Some(_) => self
                .terms
                .iter()
                .filter(|t| t.needs & !available == 0 && matches!(t.op, Op::Eq(_)))
                .map(|t| t.column)
                .collect(),
            None => BTreeSet::new(),
        };
        let rowid: Vec<usize> = schema.rowid_alias.into_iter().collect();

        let mut paths = Vec::new();
        if forced.is_none() {
            paths.push(Path {
                kind: Kind::Seq,
                rows: n,
                cost: n * TABLE_ROW,
                needs: 0,
                ordered: self.gives_order(&rowid, &fixed),
            });
        }
        if let Some(alias) = schema.rowid_alias.filter(|_| forced.is_none()) {
            let found = self.lookup(available, [alias]);
            if !found.eq.is_empty() || found.ranged() {
                let mut rows = if found.eq.is_empty() { n } else { found.combos };
                rows = range_rows(rows, found.has_low(), found.has_high());
                let per_combo = rows / found.combos;
                paths.push(Path {
                    rows,
                    cost: found.combos * (cost::seek(n) + per_combo * TABLE_ROW),
                    needs: self.needs(&found),
                    ordered: self.gives_order(&rowid, &fixed),
                    kind: Kind::Index {
                        index: None,
                        eq: found.eq,
                        low: found.low,
                        high: found.high,
                        covering: false,
                    },
                });
            }
        }
        for (i, index) in schema.indexes.iter().enumerate().rev() {
            if self.hint == IndexHint::NoIndex || forced.is_some_and(|f| f != i) {
                continue;
            }
            let in_index = |c: usize| index.columns.iter().any(|i| i.position == c);
            let found = self.lookup(available, index.columns.iter().map(|c| c.position));
            let covering = self.used.is_some_and(|used| {
                used.iter()
                    .all(|&c| in_index(c) || schema.rowid_alias == Some(c))
            });
            let ordered = self.wanted.is_some() && {
                let mut order: Vec<usize> = index.columns.iter().map(|c| c.position).collect();
                order.extend(&rowid);
                self.gives_order(&order, &fixed)
            };
            let entry = self.sizes.entry(index);
            let fetch = if covering { 0.0 } else { TABLE_ROW };
            let k = found.eq.len();
            if k == 0 && !found.ranged() {
                // Reading the whole index only pays off when it is narrower
                // than the table, or saves a sort.
                if (covering && self.sizes.narrower(index))
                    || (ordered && self.used.is_some())
                    || forced == Some(i)
                {
                    paths.push(Path {
                        rows: n,
                        cost: n * (entry + fetch),
                        needs: 0,
                        ordered,
                        kind: Kind::Index {
                            index: Some(i),
                            eq: Vec::new(),
                            low: Vec::new(),
                            high: Vec::new(),
                            covering,
                        },
                    });
                }
                continue;
            }
            let per_key = if k == 0 {
                n
            } else {
                self.sizes.rows_per_key(index, k)
            };
            let per_combo = range_rows(per_key, found.has_low(), found.has_high());
            paths.push(Path {
                rows: found.combos * per_combo,
                cost: found.combos * (cost::seek(n) + per_combo * (entry + fetch)),
                needs: self.needs(&found),
                ordered,
                kind: Kind::Index {
                    index: Some(i),
                    eq: found.eq,
                    low: found.low,
                    high: found.high,
                    covering,
                },
            });
        }
        paths
    }

    /// The longest usable prefix of `columns`: an equality or `IN` list on
    /// each leading column, then the bounds on the next. A term is usable
    /// when the items it reads are all in `available`.
    fn lookup(&self, available: Mask, columns: impl IntoIterator<Item = usize>) -> Lookup {
        let mut found = Lookup {
            eq: Vec::new(),
            low: Vec::new(),
            high: Vec::new(),
            combos: 1.0,
        };
        let on = |column: usize| {
            self.terms
                .iter()
                .enumerate()
                .filter(move |(_, t)| t.column == column && t.needs & !available == 0)
        };
        for column in columns {
            let eq = on(column).find(|(_, t)| matches!(t.op, Op::Eq(_)));
            let list = || on(column).find(|(_, t)| matches!(t.op, Op::In(_)));
            match eq.or_else(list) {
                Some((i, t)) => {
                    if let Op::In(values) = &t.op {
                        found.combos *= values.len() as f64;
                    }
                    found.eq.push(i);
                }
                None => {
                    for (i, t) in on(column) {
                        match t.op {
                            Op::Low(..) => found.low.push(i),
                            Op::High(..) => found.high.push(i),
                            _ => {}
                        }
                    }
                    break;
                }
            }
        }
        found
    }

    fn needs(&self, found: &Lookup) -> Mask {
        found
            .eq
            .iter()
            .chain(&found.low)
            .chain(&found.high)
            .fold(0, |m, &t| m | self.terms[t].needs)
    }

    /// Whether rows in `order` (columns, ending with the rowid when there is
    /// one) satisfy the wanted order. A column fixed by an equality can be
    /// skipped anywhere. With one table, the rowid makes every row distinct,
    /// so whatever is wanted after it already holds.
    fn gives_order(&self, order: &[usize], fixed: &BTreeSet<usize>) -> bool {
        let Some(wanted) = self.wanted else {
            return false;
        };
        let rowid = self.schema().rowid_alias;
        let want: Vec<usize> = wanted
            .columns
            .iter()
            .copied()
            .filter(|c| !fixed.contains(c))
            .collect();
        let mut order = order.iter().copied().filter(|c| !fixed.contains(c));
        let mut seen = BTreeSet::new();
        while seen.len() < want.len() {
            let Some(c) = order.next() else {
                return false;
            };
            let next_ok = if wanted.any_order {
                want.contains(&c)
            } else {
                want[seen.len()] == c
            };
            if !next_ok {
                return false;
            }
            seen.insert(c);
            if Some(c) == rowid {
                return true;
            }
        }
        true
    }

    /// The cheapest path, counting the sort a path that is not in the wanted
    /// order would need. Earlier paths win ties.
    pub fn best<'p>(&self, paths: &'p [Path]) -> &'p Path {
        let mut best = &paths[0];
        for path in &paths[1..] {
            if self.total(path) < self.total(best) {
                best = path;
            }
        }
        best
    }

    /// A path's cost with the sort it needs, which sorts the rows the
    /// conditions it did not use let through.
    pub fn total(&self, path: &Path) -> f64 {
        match self.wanted {
            Some(wanted) if !path.ordered => {
                let used: Vec<usize> = match &path.kind {
                    Kind::Seq => Vec::new(),
                    Kind::Index { eq, low, high, .. } => eq
                        .iter()
                        .chain(low)
                        .chain(high)
                        .map(|&t| self.terms[t].conjunct)
                        .collect(),
                };
                let keep: f64 = self
                    .own
                    .iter()
                    .filter(|(c, _)| !used.contains(c))
                    .map(|(_, keep)| keep)
                    .product();
                (path.cost + cost::sort(path.rows * keep, wanted.width)) * cost::SORT_PENALTY
            }
            _ => path.cost,
        }
    }

    /// The plan step for a path, explained, with the paths it beat.
    pub fn node(&self, paths: &[Path], chosen: &Path) -> PlanNode {
        let schema = self.schema();
        let estimate = Estimate {
            rows: chosen.rows,
            cost: self.total(chosen),
        };
        let mut rejected: Vec<Rejected> = paths
            .iter()
            .filter(|p| !std::ptr::eq(*p, chosen))
            .map(|p| Rejected {
                plan: self.describe(p),
                rows: p.rows,
                cost: self.total(p),
            })
            .collect();
        rejected.sort_by(|a, b| a.cost.total_cmp(&b.cost));
        let mut reason = match &chosen.kind {
            Kind::Seq if paths.len() == 1 => format!(
                "no index on {} helps this query, so its rows are read in rowid order",
                schema.name
            ),
            Kind::Seq => format!(
                "reading all {} rows of {} is the cheapest way here",
                rows(chosen.rows),
                schema.name
            ),
            Kind::Index {
                index,
                eq,
                low,
                high,
                covering,
            } => {
                let tree = match index {
                    Some(i) => format!("index {}", schema.indexes[*i].name),
                    None => "the INTEGER PRIMARY KEY".to_string(),
                };
                let whole = eq.is_empty() && low.is_empty() && high.is_empty();
                let found = rows(chosen.rows);
                match (whole, *covering) {
                    (true, true) => format!(
                        "{tree} holds every column the query reads and is narrower than the \
                         table, so reading it whole beats reading the table"
                    ),
                    (true, false) => format!(
                        "{tree} is read whole, in key order, and each entry leads to its row by \
                         rowid"
                    ),
                    _ if chosen.needs != 0 => format!(
                        "for each row on the left, {tree} finds about {}",
                        count(chosen.rows, "matching row")
                    ),
                    (false, true) => format!(
                        "{tree} finds about {found} of the {} rows, and holds every column the \
                         query reads, so the table is not read",
                        rows(self.sizes.rows())
                    ),
                    (false, false) => format!(
                        "{tree} finds about {found} of the {} rows, and each leads to its row \
                         by rowid",
                        rows(self.sizes.rows())
                    ),
                }
            }
        };
        if self.sizes.stats.is_none() {
            reason.push_str(&format!(
                ". {} has no statistics yet (ANALYZE counts them), so sizes are SQLite's \
                 guesses",
                schema.name
            ));
        }
        match self.wanted {
            Some(w) if chosen.ordered && w.sorts => reason.push_str(
                ". Rows come out in ORDER BY order already, so there is no Sort step, and a \
                 LIMIT can stop reading early",
            ),
            Some(_) if chosen.ordered => reason.push_str(
                ". Rows come out ordered by the columns being grouped, which is how SQLite reads \
                 them, so equal values such as 2 and 2.0 meet in the same order",
            ),
            _ => {}
        }
        let why = Why {
            reason,
            plan: Some(self.describe(chosen)),
            estimate: Some(estimate),
            rejected,
        };
        match &chosen.kind {
            Kind::Seq => PlanNode::SeqScan {
                table: schema.name.clone(),
                why,
            },
            Kind::Index { .. } => PlanNode::IndexScan {
                table: schema.name.clone(),
                access: self.access(chosen),
                why,
            },
        }
    }

    /// The lookup a path describes, ready to run.
    pub fn access(&self, path: &Path) -> IndexAccess {
        let schema = self.schema();
        let Kind::Index {
            index,
            eq,
            low,
            high,
            covering,
        } = &path.kind
        else {
            return IndexAccess::whole_table();
        };
        let index: Option<&IndexSchema> = index.map(|i| &schema.indexes[i]);
        let columns: Vec<String> = match index {
            Some(index) => index.columns.iter().map(|c| c.name.clone()).collect(),
            None => vec![
                schema.columns[schema.rowid_alias.expect("a rowid lookup")]
                    .name
                    .clone(),
            ],
        };
        // A constant is converted now, so the plan shows what it looks for.
        let fold = |expr: &Expr, convert: Convert| match constant(expr) {
            Some(v) => Expr::Literal(convert_value(convert, v)),
            None => expr.clone(),
        };
        let eq = eq
            .iter()
            .map(|&t| match &self.terms[t].op {
                Op::Eq(expr) => KeyValue::Expr {
                    expr: fold(expr, self.terms[t].convert),
                    convert: self.terms[t].convert,
                },
                Op::In(values) => KeyValue::List(values.clone()),
                Op::Low(..) | Op::High(..) => unreachable!("an equality term"),
            })
            .collect();
        let bounds = |terms: &[usize]| {
            terms
                .iter()
                .map(|&t| match &self.terms[t].op {
                    Op::Low(expr, inclusive) | Op::High(expr, inclusive) => KeyBound {
                        expr: fold(expr, self.terms[t].convert),
                        convert: self.terms[t].convert,
                        inclusive: *inclusive,
                    },
                    _ => unreachable!("a range term"),
                })
                .collect()
        };
        IndexAccess {
            index: index.map(|i| i.name.clone()),
            columns,
            eq,
            low: bounds(low),
            high: bounds(high),
            covering: match (index, covering) {
                (Some(index), true) => Some(Covering {
                    width: schema.columns.len(),
                    positions: index.columns.iter().map(|c| c.position).collect(),
                    rowid: schema.rowid_alias,
                }),
                _ => None,
            },
        }
    }

    /// A path in one line, the way the plan would show it.
    pub fn describe(&self, path: &Path) -> String {
        match &path.kind {
            Kind::Seq => format!("SeqScan {}", self.schema().name),
            Kind::Index { .. } => format!("IndexScan {} {}", self.schema().name, self.access(path)),
        }
    }
}

/// A lookup's terms, and how many separate keys its `IN` lists make.
struct Lookup {
    eq: Vec<usize>,
    low: Vec<usize>,
    high: Vec<usize>,
    combos: f64,
}

impl Lookup {
    fn ranged(&self) -> bool {
        !self.low.is_empty() || !self.high.is_empty()
    }
    fn has_low(&self) -> bool {
        !self.low.is_empty()
    }
    fn has_high(&self) -> bool {
        !self.high.is_empty()
    }
}

/// The rows a range keeps of `rows`: a quarter with one bound and a 64th
/// with two, as SQLite guesses, but not below 10 unless there were fewer.
fn range_rows(rows: f64, low: bool, high: bool) -> f64 {
    let share = match (low, high) {
        (false, false) => return rows,
        (true, true) => 64.0,
        _ => 4.0,
    };
    if rows > 10.0 {
        (rows / share).max(10.0)
    } else {
        rows
    }
}

/// "1 row", "about 10 rows": a count and its noun.
pub(super) fn count(n: f64, noun: &str) -> String {
    let shown = rows(n);
    if shown == "1" {
        format!("1 {noun}")
    } else {
        format!("{shown} {noun}s")
    }
}

/// A row count as the plan shows it: whole numbers, with separators.
pub(super) fn rows(n: f64) -> String {
    let n = n.round().max(0.0) as u64;
    let digits = n.to_string();
    let mut out = String::new();
    for (i, d) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(d);
    }
    out
}

/// The share of rows a condition keeps, for estimating rows: an equality
/// keeps one value's rows, as counted or guessed for an index, an `IN` list
/// as many values' rows, and a range a quarter (a 64th with both ends).
/// Anything else keeps half.
pub(super) fn selectivity(terms: &[(&Term, Sizes)]) -> f64 {
    let (mut share, mut low, mut high): (Option<f64>, bool, bool) = (None, false, false);
    for (term, sizes) in terms {
        let one = sizes.rows_per_value(term.column) / sizes.rows();
        match &term.op {
            Op::Eq(_) => share = Some(share.map_or(one, |s| s.min(one))),
            Op::In(values) => share = Some(values.len() as f64 * one),
            Op::Low(..) => low = true,
            Op::High(..) => high = true,
        }
    }
    let share = share.unwrap_or(match (low, high) {
        (true, true) => 1.0 / 64.0,
        (true, false) | (false, true) => 0.25,
        (false, false) => 0.5,
    });
    share.min(1.0)
}
