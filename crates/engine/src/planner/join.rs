//! Planning FROM: the order to join the tables in, how to read each one,
//! and where each condition goes.
//!
//! Every order is a left-deep nested loop: the first table, joined to the
//! second, that joined to the third, and so on. Adding a table costs what
//! reading it costs, once for a table kept in memory or once per left row
//! for a lookup that uses the left row's values. The search keeps, for each
//! set of tables joined so far, the cheapest way found to join them, which
//! is exact up to 8 tables; beyond that it keeps the 16 cheapest sets at
//! each step, as SQLite does with its "N best paths".
//!
//! A LEFT JOIN's right table comes after every table before it in FROM, and
//! only its ON condition may narrow how it is read: a WHERE condition on it
//! must see the NULLs of the left rows it did not match.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;

use crate::catalog::{Catalog, TableSchema};
use crate::error::Result;
use crate::expr::Expr;
use crate::plan::{Estimate, JoinKind, PlanNode, Rejected, Why};

use super::cost::{self, REPLAY_ROW, Sizes};
use super::paths::{self, Kind, Mask, Path, Target, Term, Wanted};
use super::{Rows, columns_read, has_subquery, split_and, with_filter};

/// One FROM item, as the join planner sees it.
pub(super) struct Source<'q> {
    pub rows: Rows<'q>,
    pub name: String,
    pub kind: JoinKind,
    /// Its place in FROM.
    pub position: usize,
    /// Where its columns sit in a joined row.
    pub offset: usize,
    pub width: usize,
    /// Its columns the query reads.
    pub used: BTreeSet<usize>,
}

/// One AND-ed part of WHERE or ON.
struct Conjunct {
    expr: Expr,
    /// The FROM items it reads, by place in FROM.
    tables: Mask,
    /// For a part of a LEFT JOIN's ON condition, that join's item.
    left_on: Option<usize>,
    /// The share of rows it keeps.
    keep: f64,
}

/// The conditions of a FROM clause and the terms they give each table.
pub(super) struct Analysis<'q> {
    items: Vec<Source<'q>>,
    conjuncts: Vec<Conjunct>,
    /// Per item: the terms it may use to find its rows.
    terms: Vec<Vec<Term>>,
    /// Per item: the share of its rows its own conditions keep, and the
    /// conditions that read it.
    own_keep: Vec<f64>,
    own: Vec<Vec<(usize, f64)>>,
    involving: Vec<Vec<usize>>,
    /// Per item that is a table: its row counts.
    sizes: Vec<Option<Sizes<'q>>>,
    /// Every item.
    all: Mask,
    /// Per item: the items its terms read. Its paths depend only on which
    /// of these are joined already, so they are worked out once for each.
    reads: Vec<Mask>,
    paths: RefCell<PathCache>,
}

/// The join order chosen and the ones turned down.
pub(super) struct Choice {
    /// Items by their place in FROM, in join order.
    pub order: Vec<usize>,
    /// The chosen order in one line, and the orders turned down.
    pub alternatives: Alternatives,
}

#[derive(Clone, Default)]
pub(super) struct Alternatives {
    pub plan: Option<String>,
    pub rejected: Vec<Rejected>,
}

impl<'q> Analysis<'q> {
    /// `on` holds each item's ON condition, bound.
    pub fn new(
        catalog: &'q Catalog,
        items: Vec<Source<'q>>,
        on: Vec<Option<Expr>>,
        predicate: Option<Expr>,
    ) -> Analysis<'q> {
        let sizes: Vec<Option<Sizes<'q>>> = items.iter().map(|i| sizes(catalog, i)).collect();
        let mut conjuncts = Vec::new();
        let tables_of = |e: &Expr, items: &[Source]| -> Mask {
            // A subquery may read any column of the row, so it waits for all.
            if has_subquery(e) {
                return items.iter().fold(0, |m, i| m | (1 << i.position));
            }
            columns_read(e)
                .filter_map(|c| {
                    items
                        .iter()
                        .find(|i| (i.offset..i.offset + i.width).contains(&c))
                })
                .fold(0, |m, i| m | (1 << i.position))
        };
        for (k, on) in on.into_iter().enumerate() {
            let Some(on) = on else { continue };
            let left = (items[k].kind == JoinKind::Left).then_some(k);
            for expr in split_and(on) {
                conjuncts.push((expr, left));
            }
        }
        conjuncts.extend(predicate.into_iter().flat_map(split_and).map(|e| (e, None)));

        let mut terms: Vec<Vec<Term>> = vec![Vec::new(); items.len()];
        let mut found: Vec<Vec<(usize, Term)>> = vec![Vec::new(); conjuncts.len()];
        for (c, (expr, left_on)) in conjuncts.iter().enumerate() {
            for (k, item) in items.iter().enumerate() {
                let Rows::Table(schema) = &item.rows else {
                    continue;
                };
                // A LEFT JOIN's table uses its ON condition; others never do.
                if *left_on != (item.kind == JoinKind::Left).then_some(k) {
                    continue;
                }
                let of = |e: &Expr| (!has_subquery(e)).then(|| tables_of(e, &items));
                for term in paths::terms_of(c, expr, schema, item.offset, &of) {
                    found[c].push((k, term.clone()));
                    terms[k].push(term);
                }
            }
        }
        let conjuncts = conjuncts
            .into_iter()
            .zip(&found)
            .map(|((expr, left_on), found)| {
                let sized: Vec<(&Term, Sizes)> = found
                    .iter()
                    .filter_map(|(k, t)| Some((t, sizes[*k]?)))
                    .collect();
                Conjunct {
                    tables: tables_of(&expr, &items),
                    keep: paths::selectivity(&sized),
                    expr,
                    left_on,
                }
            })
            .collect::<Vec<Conjunct>>();
        let bit = |k: usize| 1 << items[k].position;
        let items_mask = (0..items.len()).fold(0, |m, k| m | bit(k));
        let reads = terms
            .iter()
            .map(|terms| terms.iter().fold(0, |m, t| m | t.needs))
            .collect();
        let own: Vec<Vec<(usize, f64)>> = (0..items.len())
            .map(|k| {
                (0..conjuncts.len())
                    .filter(|&c| conjuncts[c].tables == bit(k))
                    .map(|c| (c, conjuncts[c].keep))
                    .collect()
            })
            .collect();
        let own_keep = own
            .iter()
            .map(|own| own.iter().map(|(_, keep)| keep).product())
            .collect();
        let involving = (0..items.len())
            .map(|k| {
                (0..conjuncts.len())
                    .filter(|&c| conjuncts[c].tables & bit(k) != 0)
                    .collect()
            })
            .collect();
        Analysis {
            items,
            conjuncts,
            terms,
            own_keep,
            own,
            involving,
            sizes,
            all: items_mask,
            reads,
            paths: RefCell::new(HashMap::new()),
        }
    }

    /// The share of item `k`'s rows that its own conditions keep.
    fn own_keep(&self, k: usize) -> f64 {
        self.own_keep[k]
    }

    fn target<'a>(&'a self, k: usize, wanted: Option<&'a Wanted>) -> Option<Target<'a>> {
        let item = &self.items[k];
        Some(Target {
            sizes: self.sizes[k]?,
            name: &item.name,
            terms: &self.terms[k],
            own: &self.own[k],
            used: Some(&item.used),
            wanted,
        })
    }

    /// The cheapest way to add item `k` after the items in `mask`, which
    /// produce `rows` rows: the paths considered, the one chosen, and what
    /// adding it costs in all.
    fn step(&self, k: usize, mask: Mask, rows: f64) -> Step {
        let item = &self.items[k];
        let first = mask == 0;
        let keep = self.own_keep(k);
        let (paths, chosen, cost) = match self.target(k, None) {
            Some(target) => {
                let key = (k, mask & self.reads[k]);
                let cached = self.paths.borrow().get(&key).cloned();
                let paths = cached.unwrap_or_else(|| {
                    let paths: Rc<[Path]> = target.paths(mask).into();
                    self.paths.borrow_mut().insert(key, Rc::clone(&paths));
                    paths
                });
                let n = target.sizes.rows();
                let total = |p: &Path| {
                    if p.needs != 0 {
                        rows * p.cost
                    } else if first {
                        p.cost
                    } else {
                        p.cost + rows * n * keep * REPLAY_ROW
                    }
                };
                let mut best = 0;
                for (i, p) in paths.iter().enumerate().skip(1) {
                    if total(p) < total(&paths[best]) {
                        best = i;
                    }
                }
                let cost = total(&paths[best]);
                (paths, Some(best), cost)
            }
            None => {
                let estimate = self.subquery_estimate(k);
                let replay = if first {
                    0.0
                } else {
                    rows * estimate.rows * keep * REPLAY_ROW
                };
                (Rc::from([]), None, estimate.cost + replay)
            }
        };
        let joined = mask | (1 << item.position);
        let newly = self.involving[k]
            .iter()
            .map(|&c| &self.conjuncts[c])
            .filter(|c| c.tables & !joined == 0)
            .map(|c| c.keep)
            .product::<f64>();
        let mut out = rows.max(1.0) * self.item_rows(k) * newly;
        if first {
            out = self.item_rows(k) * newly;
        }
        if item.kind == JoinKind::Left {
            out = out.max(rows);
        }
        Step {
            paths,
            chosen,
            cost,
            rows: out,
        }
    }

    fn item_rows(&self, k: usize) -> f64 {
        match self.sizes[k] {
            Some(sizes) => sizes.rows(),
            None => self.subquery_estimate(k).rows,
        }
    }

    fn subquery_estimate(&self, k: usize) -> Estimate {
        match &self.items[k].rows {
            Rows::Query(_, estimate) => *estimate,
            Rows::Table(_) => unreachable!("a table has sizes"),
        }
    }

    /// Whether item `k` may come after the items in `mask`.
    fn allowed(&self, k: usize, mask: Mask) -> bool {
        let item = &self.items[k];
        if mask & (1 << item.position) != 0 {
            return false;
        }
        let before: Mask = (1 << item.position) - 1;
        item.kind != JoinKind::Left || mask & before & self.all == before & self.all
    }

    /// Searches for the cheapest order.
    pub fn choose(&self) -> Choice {
        let n = self.items.len();
        let width = if n <= 8 { usize::MAX } else { 16 };
        let mut beam = vec![State::start()];
        for level in 0..n {
            // Each way to add one more table, without copying its order yet.
            let mut next: Vec<(usize, usize, State)> = Vec::new();
            for (p, state) in beam.iter().enumerate() {
                for k in 0..n {
                    if self.allowed(k, state.mask) {
                        let step = self.step(k, state.mask, state.rows);
                        let grown = State {
                            order: Vec::new(),
                            mask: state.mask | 1 << self.items[k].position,
                            rows: step.rows,
                            cost: state.cost + step.cost,
                            rows_along: state.rows_along + step.rows,
                        };
                        next.push((p, k, grown));
                    }
                }
            }
            // Equal costs go to the order with smaller results along the
            // way, then to the one closest to FROM order.
            let key = |(p, k, s): &(usize, usize, State)| (s.cost, s.rows_along, *p, *k);
            let better = |a: &(usize, usize, State), b: &(usize, usize, State)| {
                let (a, b) = (key(a), key(b));
                a.0.total_cmp(&b.0)
                    .then_with(|| a.1.total_cmp(&b.1))
                    .then_with(|| (a.2, a.3).cmp(&(b.2, b.3)))
            };
            // Complete orders all join the same tables; keep them apart.
            if level + 1 < n {
                let mut best: BTreeMap<Mask, usize> = BTreeMap::new();
                for (i, candidate) in next.iter().enumerate() {
                    let slot = best.entry(candidate.2.mask).or_insert(i);
                    if better(candidate, &next[*slot]).is_lt() {
                        *slot = i;
                    }
                }
                let mut kept: Vec<usize> = best.into_values().collect();
                if kept.len() > width {
                    kept.select_nth_unstable_by(width - 1, |&a, &b| better(&next[a], &next[b]));
                    kept.truncate(width);
                }
                kept.sort_by(|&a, &b| better(&next[a], &next[b]));
                let mut slots: Vec<Option<(usize, usize, State)>> =
                    next.into_iter().map(Some).collect();
                next = kept
                    .into_iter()
                    .map(|i| slots[i].take().expect("kept once"))
                    .collect();
            } else {
                next.sort_by(better);
            }
            beam = next
                .into_iter()
                .map(|(p, k, mut state)| {
                    state.order = beam[p].order.clone();
                    state.order.push(k);
                    state
                })
                .collect();
        }
        let as_written: Vec<usize> = (0..n).collect();
        let mut rejected: Vec<Rejected> = beam
            .iter()
            .skip(1)
            .take(3)
            .map(|s| self.rejected(s, &as_written))
            .collect();
        if !beam.iter().take(4).any(|s| s.order == as_written) {
            let written = as_written
                .iter()
                .fold(State::start(), |s, &k| self.extend(&s, k));
            rejected.push(self.rejected(&written, &as_written));
        }
        Choice {
            order: beam[0]
                .order
                .iter()
                .map(|&k| self.items[k].position)
                .collect(),
            alternatives: Alternatives {
                plan: Some(self.rejected(&beam[0], &as_written).plan),
                rejected,
            },
        }
    }

    fn extend(&self, state: &State, k: usize) -> State {
        let step = self.step(k, state.mask, state.rows);
        let mut next = state.clone();
        next.order.push(k);
        next.mask |= 1 << self.items[k].position;
        next.rows = step.rows;
        next.rows_along += step.rows;
        next.cost += step.cost;
        next
    }

    /// A join order turned down, as "t1 scanned, then t2 by index t2_a".
    fn rejected(&self, state: &State, as_written: &[usize]) -> Rejected {
        let (mut mask, mut rows) = (0, 1.0);
        let mut steps = Vec::new();
        for &k in &state.order {
            let step = self.step(k, mask, rows);
            steps.push(match (step.chosen, self.target(k, None)) {
                (Some(i), Some(target)) => short(&target, &step.paths[i]),
                _ => self.items[k].name.clone(),
            });
            mask |= 1 << self.items[k].position;
            rows = step.rows;
        }
        let mut plan = steps.join(", then ");
        if state.order == as_written {
            plan.push_str(" (FROM order)");
        }
        Rejected {
            plan,
            rows: state.rows,
            cost: state.cost,
        }
    }

    /// Builds the plan for the items in their current order, which is the
    /// join order: each read the cheapest way, each condition as low as it
    /// can go. `wanted` is an order a single table could save a sort with;
    /// the answer says whether the plan already returns rows in it.
    pub fn build(self, wanted: Option<&Wanted>, alternatives: Alternatives) -> Result<Built> {
        let n = self.items.len();
        let reads_only = |c: &Conjunct, k: usize| c.tables == 1 << self.items[k].position;
        let level_of = |c: &Conjunct| {
            (0..n)
                .rev()
                .find(|&k| c.tables & (1 << self.items[k].position) != 0)
                .unwrap_or(0)
        };
        // Per item: conditions on its rows alone, conditions on its join,
        // and filters after its join.
        let mut own: Vec<Vec<Expr>> = vec![Vec::new(); n];
        let mut on_join: Vec<Vec<Expr>> = vec![Vec::new(); n];
        let mut after: Vec<Vec<Expr>> = vec![Vec::new(); n];
        for c in &self.conjuncts {
            let expr = c.expr.clone();
            if let Some(k) = c.left_on {
                // A LEFT JOIN's ON condition stays on its join, unless it
                // reads only the right table: then it filters that table.
                if reads_only(c, k) && !has_subquery(&c.expr) {
                    own[k].push(expr);
                } else {
                    on_join[k].push(expr);
                }
                continue;
            }
            let level = level_of(c);
            let inner = self.items[level].kind == JoinKind::Inner;
            if (level == 0 || reads_only(c, level) && inner) && !(has_subquery(&c.expr) && n > 1) {
                own[level].push(expr);
            } else if !inner {
                after[level].push(expr);
            } else {
                on_join[level].push(expr);
            }
        }

        let mut root: Option<PlanNode> = None;
        let mut ordered = false;
        let (mut mask, mut rows, mut cost) = (0, 1.0, 0.0);
        let width = self.items.len();
        for k in 0..width {
            let step = self.step(k, mask, rows);
            let item = &self.items[k];
            // The table's own conditions see only its row, so their columns
            // move to where the table's columns start.
            let mut filters = std::mem::take(&mut own[k]);
            if item.offset > 0 {
                filters
                    .iter_mut()
                    .for_each(|e| e.shift_columns(item.offset));
            }
            let (node, lookup, how) = match (&item.rows, step.chosen) {
                // A single table with an order to keep weighs the sort too.
                (Rows::Table(_), Some(_)) if n == 1 && wanted.is_some() => {
                    let target = self.target(k, wanted).expect("a table");
                    let paths = target.paths(mask);
                    let chosen = target.best(&paths);
                    ordered = chosen.ordered;
                    (target.node(&paths, chosen), false, String::new())
                }
                (Rows::Table(_), Some(i)) => {
                    let target = self.target(k, None).expect("a table");
                    let chosen = &step.paths[i];
                    let node = target.node(&step.paths, chosen);
                    (node, chosen.needs != 0, short(&target, chosen))
                }
                (Rows::Query(node, _), _) => ((**node).clone(), false, String::new()),
                (Rows::Table(_), None) => unreachable!("a table has paths"),
            };
            let node = with_filter(node, filters);
            mask |= 1 << item.position;
            cost += step.cost;
            let previous_rows = rows;
            rows = step.rows;
            let joined = match root {
                None => node,
                Some(left) => {
                    let right = if lookup {
                        node
                    } else {
                        PlanNode::Materialize {
                            input: Box::new(node),
                        }
                    };
                    let reason = if lookup {
                        format!(
                            "each of about {} on the left finds its matching rows: {how}, \
                             instead of reading every row of {}",
                            paths::count(previous_rows, "row"),
                            item.name
                        )
                    } else {
                        format!(
                            "no index finds the rows of {} that match a left row, so each left \
                             row is checked against every row of {}, kept in memory",
                            item.name, item.name
                        )
                    };
                    PlanNode::NestedLoopJoin {
                        left: Box::new(left),
                        right: Box::new(right),
                        kind: item.kind,
                        condition: super::and_all(std::mem::take(&mut on_join[k])),
                        right_width: item.width,
                        why: Why {
                            reason,
                            estimate: Some(Estimate { rows, cost }),
                            // The last join holds the choice of order.
                            ..if k == width - 1 {
                                Why {
                                    plan: alternatives.plan.clone(),
                                    rejected: alternatives.rejected.clone(),
                                    ..Why::default()
                                }
                            } else {
                                Why::default()
                            }
                        },
                    }
                }
            };
            root = Some(with_filter(joined, std::mem::take(&mut after[k])));
        }
        Ok(Built {
            root: root.expect("FROM has an item"),
            ordered,
            estimate: Estimate { rows, cost },
        })
    }
}

/// The plan for FROM and WHERE, and whether its rows come out in the
/// wanted order.
pub(super) struct Built {
    pub root: PlanNode,
    pub ordered: bool,
    pub estimate: Estimate,
}

/// Each item's paths, by the items joined before it that its terms read.
type PathCache = HashMap<(usize, Mask), Rc<[Path]>>;

struct Step {
    paths: Rc<[Path]>,
    chosen: Option<usize>,
    cost: f64,
    rows: f64,
}

#[derive(Clone)]
struct State {
    order: Vec<usize>,
    mask: Mask,
    rows: f64,
    cost: f64,
    /// The rows after each step, added up.
    rows_along: f64,
}

impl State {
    fn start() -> State {
        State {
            order: Vec::new(),
            mask: 0,
            rows: 1.0,
            cost: 0.0,
            rows_along: 0.0,
        }
    }
}

fn sizes<'q>(catalog: &'q Catalog, item: &Source<'q>) -> Option<Sizes<'q>> {
    match &item.rows {
        Rows::Table(table) => Some(Sizes::new(table, catalog.stats(&table.name))),
        Rows::Query(..) => None,
    }
}

/// "t2 by index t2_a", "t1 scanned": how one step reads its table.
fn short(target: &Target, path: &Path) -> String {
    let table: &TableSchema = target.sizes.table;
    match &path.kind {
        Kind::Seq => format!("{} scanned", target.name),
        Kind::Index { index: None, .. } => format!("{} by rowid", target.name),
        Kind::Index { index: Some(i), .. } => {
            format!("{} by index {}", target.name, table.indexes[*i].name)
        }
    }
}

/// What a whole SELECT is expected to produce, for a query that reads it
/// in FROM: its join's estimate, with one row for an aggregate without
/// GROUP BY.
pub(super) fn query_estimate(join: Estimate, aggregate: bool, grouped: bool) -> Estimate {
    Estimate {
        rows: if aggregate && !grouped {
            1.0
        } else {
            join.rows
        },
        cost: join.cost
            + if aggregate {
                cost::sort(join.rows, 1)
            } else {
                0.0
            },
    }
}
