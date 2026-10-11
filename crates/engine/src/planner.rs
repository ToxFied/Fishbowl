//! Turns a parsed SQL statement into a [`Plan`].
//!
//! This is also where names are resolved ("binding"): `WHERE a > 1` becomes
//! "column 0 > 1", checked against the catalog. Anything Fishbowl does not
//! implement yet is rejected here with [`Error::Unsupported`], never ignored.

use std::cell::RefCell;

use sqlparser::ast;

use crate::catalog::{Catalog, Column, TableSchema, View};
use crate::error::{Error, Result, unsupported};
use crate::expr::{
    AggregateCall, AggregateFunc, BinaryOp, Expr, ScalarFunc, SubqueryKind, UnaryOp,
    is_sqlite_function,
};
use crate::plan::{JoinKind, Plan, PlanNode, QueryPlan, SetOp, SortKey, Subquery};
use crate::value::{Affinity, Value};

pub fn plan_statement(catalog: &Catalog, stmt: &ast::Statement) -> Result<Plan> {
    match stmt {
        ast::Statement::CreateTable(create) => plan_create_table(create),
        ast::Statement::Insert(insert) => plan_insert(catalog, insert),
        ast::Statement::Query(query) => {
            plan_query(&Planner::new(catalog), query, None).map(Plan::Query)
        }
        ast::Statement::CreateView(create) => plan_create_view(create),
        ast::Statement::Drop {
            object_type: ast::ObjectType::View,
            if_exists,
            names,
            cascade: false,
            restrict: false,
            purge: false,
            temporary: false,
            ..
        } => plan_drop_view(catalog, *if_exists, names),
        ast::Statement::Update(update) => plan_update(catalog, update),
        ast::Statement::Delete(delete) => plan_delete(catalog, delete),
        ast::Statement::Drop {
            object_type: ast::ObjectType::Table,
            if_exists,
            names,
            cascade: false,
            restrict: false,
            purge: false,
            temporary: false,
            ..
        } => plan_drop_table(catalog, *if_exists, names),
        other => unsupported(statement_kind(other)),
    }
}

fn plan_create_table(create: &ast::CreateTable) -> Result<Plan> {
    if create.query.is_some() {
        return unsupported("CREATE TABLE ... AS SELECT");
    }
    if !create.constraints.is_empty() {
        return unsupported("table constraints");
    }
    if create.columns.is_empty() {
        return Err(Error::Parse("a table needs at least one column".into()));
    }
    let name = object_name(&create.name)?;
    let mut columns: Vec<Column> = Vec::new();
    for def in &create.columns {
        let declared_type = match def.data_type {
            ast::DataType::Unspecified => String::new(),
            ref t => t.to_string(),
        };
        let mut not_null = false;
        for option in &def.options {
            match option.option {
                ast::ColumnOption::NotNull => not_null = true,
                ast::ColumnOption::Null => {}
                ref other => return unsupported(format!("column constraint {other}")),
            }
        }
        if columns
            .iter()
            .any(|c| c.name.eq_ignore_ascii_case(&def.name.value))
        {
            return Err(Error::Catalog(format!(
                "duplicate column name: {}",
                def.name.value
            )));
        }
        columns.push(Column {
            name: def.name.value.clone(),
            affinity: Affinity::from_declared_type(&declared_type),
            declared_type,
            not_null,
        });
    }
    Ok(Plan::CreateTable {
        schema: TableSchema { name, columns },
        if_not_exists: create.if_not_exists,
    })
}

fn plan_update(catalog: &Catalog, update: &ast::Update) -> Result<Plan> {
    if update.or.is_some()
        || update.from.is_some()
        || update.returning.is_some()
        || update.output.is_some()
        || !update.order_by.is_empty()
        || update.limit.is_some()
    {
        return unsupported("UPDATE OR / FROM / RETURNING / LIMIT");
    }
    let (schema, scope) = dml_target(catalog, &update.table)?;
    let mut binder = Binder::without_items(&scope);
    let mut assignments = Vec::new();
    for assignment in &update.assignments {
        let ast::AssignmentTarget::ColumnName(column) = &assignment.target else {
            return unsupported("UPDATE of a column tuple");
        };
        let name = object_name(column)?;
        let index = schema
            .column_index(&name)
            .ok_or_else(|| Error::Catalog(format!("no such column: {name}")))?;
        assignments.push((index, binder.bind(&assignment.value, Ctx::ROW)?));
    }
    Ok(Plan::Update {
        table: schema.name.clone(),
        assignments,
        predicate: update
            .selection
            .as_ref()
            .map(|e| binder.bind(e, Ctx::ROW))
            .transpose()?,
    })
}

fn plan_delete(catalog: &Catalog, delete: &ast::Delete) -> Result<Plan> {
    if !delete.tables.is_empty()
        || delete.using.is_some()
        || delete.returning.is_some()
        || delete.output.is_some()
        || !delete.order_by.is_empty()
        || delete.limit.is_some()
    {
        return unsupported("DELETE USING / RETURNING / LIMIT");
    }
    let (ast::FromTable::WithFromKeyword(from) | ast::FromTable::WithoutKeyword(from)) =
        &delete.from;
    let [target] = from.as_slice() else {
        return unsupported("DELETE from several tables");
    };
    let (schema, scope) = dml_target(catalog, target)?;
    let mut binder = Binder::without_items(&scope);
    Ok(Plan::Delete {
        table: schema.name.clone(),
        predicate: delete
            .selection
            .as_ref()
            .map(|e| binder.bind(e, Ctx::ROW))
            .transpose()?,
    })
}

fn plan_drop_table(catalog: &Catalog, if_exists: bool, names: &[ast::ObjectName]) -> Result<Plan> {
    let [name] = names else {
        return unsupported("DROP TABLE of several tables");
    };
    let name = object_name(name)?;
    if catalog.view(&name).is_some() {
        return Err(Error::Catalog(format!(
            "use DROP VIEW to delete view {name}"
        )));
    }
    let table = match catalog.table(&name) {
        Ok(schema) => schema.name.clone(),
        Err(_) if if_exists => name,
        Err(e) => return Err(e),
    };
    Ok(Plan::DropTable { table, if_exists })
}

fn plan_create_view(create: &ast::CreateView) -> Result<Plan> {
    if create.or_alter
        || create.or_replace
        || create.materialized
        || create.secure
        || create.options != ast::CreateTableOptions::None
        || !create.cluster_by.is_empty()
        || create.comment.is_some()
        || create.with_no_schema_binding
        || create.copy_grants
        || create.to.is_some()
        || create.params.is_some()
    {
        return unsupported("this CREATE VIEW option");
    }
    let mut columns = Vec::new();
    for column in &create.columns {
        if column.data_type.is_some() || column.options.is_some() {
            return unsupported("a type or option on a view column");
        }
        columns.push(column.name.value.clone());
    }
    // Like SQLite, the SELECT is checked when a query uses the view, not now.
    // A TEMP view behaves like any other: Fishbowl does not save views yet.
    Ok(Plan::CreateView {
        view: View {
            name: object_name(&create.name)?,
            columns,
            query: create.query.clone(),
        },
        if_not_exists: create.if_not_exists,
    })
}

fn plan_drop_view(catalog: &Catalog, if_exists: bool, names: &[ast::ObjectName]) -> Result<Plan> {
    let [name] = names else {
        return unsupported("DROP VIEW of several views");
    };
    let name = object_name(name)?;
    let view = match catalog.view(&name) {
        Some(view) => view.name.clone(),
        None if catalog.table(&name).is_ok() => {
            return Err(Error::Catalog(format!(
                "use DROP TABLE to delete table {name}"
            )));
        }
        None if if_exists => name,
        None => return Err(Error::Catalog(format!("no such view: {name}"))),
    };
    Ok(Plan::DropView { view, if_exists })
}

/// The table an INSERT, UPDATE or DELETE writes to. A view cannot be written.
fn writable_table<'a>(catalog: &'a Catalog, name: &ast::ObjectName) -> Result<&'a TableSchema> {
    let name = object_name(name)?;
    if catalog.view(&name).is_some() {
        return Err(Error::Catalog(format!(
            "cannot modify {name} because it is a view"
        )));
    }
    catalog.table(&name)
}

/// The one table an UPDATE or DELETE changes, and the names its expressions
/// can see.
fn dml_target<'a>(
    catalog: &'a Catalog,
    target: &ast::TableWithJoins,
) -> Result<(&'a TableSchema, Scope<'a>)> {
    if !target.joins.is_empty() {
        return unsupported("JOIN in UPDATE or DELETE");
    }
    let ast::TableFactor::Table {
        name,
        alias,
        args: None,
        ..
    } = &target.relation
    else {
        return unsupported("UPDATE or DELETE of a subquery or table function");
    };
    let schema = writable_table(catalog, name)?;
    let mut scope = Scope::new(None);
    scope.add_table(schema, alias.as_ref());
    Ok((schema, scope))
}

fn plan_insert(catalog: &Catalog, insert: &ast::Insert) -> Result<Plan> {
    if insert.or.is_some() || insert.on.is_some() || insert.returning.is_some() {
        return unsupported("INSERT OR / ON CONFLICT / RETURNING");
    }
    let ast::TableObject::TableName(table_name) = &insert.table else {
        return unsupported("INSERT INTO a table function");
    };
    let schema = writable_table(catalog, table_name)?;
    let targets = if insert.columns.is_empty() {
        (0..schema.columns.len()).collect()
    } else {
        let mut targets = Vec::new();
        for column in &insert.columns {
            let name = object_name(column)?;
            let index = schema.column_index(&name).ok_or_else(|| {
                Error::Catalog(format!("table {} has no column named {name}", schema.name))
            })?;
            targets.push(index);
        }
        targets
    };

    let Some(source) = &insert.source else {
        return unsupported("INSERT without VALUES");
    };
    let ast::SetExpr::Values(values) = source.body.as_ref() else {
        return unsupported("INSERT ... SELECT");
    };
    // VALUES rows are constants: there is no table in scope to reference.
    let scope = Scope::new(None);
    let mut binder = Binder::without_items(&scope);
    let mut rows = Vec::new();
    for row in &values.rows {
        if row.content.len() != targets.len() {
            return Err(Error::Catalog(format!(
                "{} values for {} columns",
                row.content.len(),
                targets.len()
            )));
        }
        rows.push(
            row.content
                .iter()
                .map(|e| binder.bind(e, Ctx::CONSTANT))
                .collect::<Result<_>>()?,
        );
    }
    Ok(Plan::Insert {
        table: schema.name.clone(),
        targets,
        rows,
    })
}

/// What planning a query needs besides its syntax tree: the catalog, and the
/// views being expanded, to catch a view defined in terms of itself.
struct Planner<'c> {
    catalog: &'c Catalog,
    views: RefCell<Vec<String>>,
}

impl<'c> Planner<'c> {
    fn new(catalog: &'c Catalog) -> Self {
        Planner {
            catalog,
            views: RefCell::new(Vec::new()),
        }
    }
}

/// Plans a query. `outer` holds the names of the queries it is nested in,
/// for a subquery that reads their columns.
fn plan_query(cx: &Planner, query: &ast::Query, outer: Option<&Scope>) -> Result<QueryPlan> {
    if query.with.is_some() {
        return unsupported("WITH (common table expressions)");
    }
    if query.fetch.is_some()
        || !query.locks.is_empty()
        || query.for_clause.is_some()
        || query.settings.is_some()
        || query.format_clause.is_some()
        || !query.pipe_operators.is_empty()
    {
        return unsupported("this query clause");
    }
    let order_by = query.order_by.as_ref();
    let mut plan = match query.body.as_ref() {
        ast::SetExpr::Select(select) => plan_select(cx, select, order_by, outer)?,
        ast::SetExpr::SetOperation { .. } => plan_compound(cx, &query.body, order_by, outer)?,
        ast::SetExpr::Query(_) => return unsupported("parenthesized SELECT"),
        ast::SetExpr::Values(_) => return unsupported("VALUES as a query"),
        _ => return unsupported("this kind of query"),
    };
    if let Some((limit, offset)) = limit_clause(query)? {
        // LIMIT and OFFSET are constants: no table is in scope.
        let scope = Scope::new(None);
        let mut constants = Binder::without_items(&scope);
        plan.root = PlanNode::Limit {
            input: Box::new(plan.root),
            limit: constants.bind(limit, Ctx::CONSTANT)?,
            offset: match offset {
                Some(offset) => Some(constants.bind(offset, Ctx::CONSTANT)?),
                None => None,
            },
        };
    }
    Ok(plan)
}

fn plan_select(
    cx: &Planner,
    select: &ast::Select,
    order_by: Option<&ast::OrderBy>,
    outer: Option<&Scope>,
) -> Result<QueryPlan> {
    let distinct = select_is_distinct(select)?;
    let group_by_terms = match &select.group_by {
        ast::GroupByExpr::Expressions(terms, modifiers) if modifiers.is_empty() => terms,
        _ => return unsupported("GROUP BY ALL or GROUP BY modifiers"),
    };

    let (scope, from) = plan_from(cx, &select.from, outer)?;
    let items = select_items(&scope, &select.projection)?;
    let mut binder = Binder {
        scope: &scope,
        items: &items,
        aggregates: Vec::new(),
        cx: Some(cx),
    };

    let predicate = match &select.selection {
        Some(predicate) => Some(binder.bind(predicate, Ctx::ROW)?),
        None => None,
    };
    let mut root = join_tree(from, &mut binder, predicate)?;

    // Binding the result columns finds any aggregates, which is what makes
    // this an aggregate query.
    let exprs = items
        .iter()
        .map(|item| binder.bind_item(item, Ctx::GROUPED.without_aliases()))
        .collect::<Result<Vec<_>>>()?;
    let is_aggregate = !group_by_terms.is_empty() || !binder.aggregates.is_empty();
    if !is_aggregate && select.having.is_some() {
        return Err(Error::Parse(
            "HAVING clause on a non-aggregate query".into(),
        ));
    }
    // Expressions evaluated on the rows that reach the result: aggregated
    // rows in an aggregate query, table rows otherwise.
    let output = if is_aggregate { Ctx::GROUPED } else { Ctx::ROW };

    // ORDER BY before HAVING, so aggregates are numbered in SQLite's order.
    let sort_keys = match order_by {
        Some(order_by) => binder.sort_keys(order_by, output)?,
        None => Vec::new(),
    };
    let having = match &select.having {
        Some(having) => Some(binder.bind(having, output)?),
        None => None,
    };
    if is_aggregate {
        // Grouping sorts rows by the GROUP BY values. When ORDER BY has as
        // many terms, SQLite sorts each GROUP BY term in the direction of the
        // matching ORDER BY term, hoping to make the final sort unnecessary.
        // That direction decides the order of rows that tie in ORDER BY.
        let same_length = sort_keys.len() == group_by_terms.len();
        let group_by = group_by_terms
            .iter()
            .enumerate()
            .map(|(i, term)| {
                let descending = same_length && sort_keys[i].descending;
                Ok(SortKey {
                    expr: binder.result_term(term, i, Clause::GroupBy, Ctx::GROUP_BY)?,
                    descending,
                    nulls_first: !descending,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let aggregates = binder.aggregates;
        // Plain columns come from the row the last min() or max() chose.
        let row_from = aggregates.iter().rposition(|a| a.func.is_min_max());
        if let Some(i) = row_from
            && aggregates[i].distinct
        {
            let mut reads_plain_column = false;
            let mut check = |e: &Expr| {
                if let Expr::Column { index, .. } = e
                    && *index < scope.width()
                    && !group_by.iter().any(|k| k.expr == *e)
                {
                    reads_plain_column = true;
                }
            };
            exprs.iter().for_each(|e| e.walk(&mut check));
            sort_keys.iter().for_each(|k| k.expr.walk(&mut check));
            having.iter().for_each(|e| e.walk(&mut check));
            if reads_plain_column {
                return unsupported("plain columns beside min(DISTINCT ...) or max(DISTINCT ...)");
            }
        }
        root = PlanNode::Aggregate {
            input: Box::new(root),
            group_by,
            aggregates,
            row_from,
            width: scope.width(),
        };
        if let Some(predicate) = having {
            root = PlanNode::Filter {
                input: Box::new(root),
                predicate,
            };
        }
    }

    if !sort_keys.is_empty() {
        // Sorting happens before DISTINCT removes duplicates, which gives the
        // same answer only when every sort key is one of the result columns.
        if distinct && sort_keys.iter().any(|k| !exprs.contains(&k.expr)) {
            return unsupported("ORDER BY a term that is not in the SELECT DISTINCT list");
        }
        root = PlanNode::Sort {
            input: Box::new(root),
            keys: sort_keys,
            reason: "no index returns rows in this order, so all of them are read and sorted"
                .into(),
        };
    }
    let affinities = exprs.iter().map(Expr::affinity).collect();
    root = PlanNode::Project {
        input: Box::new(root),
        exprs,
    };
    if distinct {
        root = PlanNode::Distinct {
            input: Box::new(root),
        };
    }
    Ok(QueryPlan {
        columns: items.into_iter().map(|item| item.name).collect(),
        affinities,
        root,
    })
}

/// Plans `a UNION b EXCEPT c ...`. SQLite gives every operator the same
/// precedence and works left to right: `(a UNION b) EXCEPT c`. The parser
/// gives INTERSECT a higher one, so the tree is flattened and rebuilt.
fn plan_compound(
    cx: &Planner,
    body: &ast::SetExpr,
    order_by: Option<&ast::OrderBy>,
    outer: Option<&Scope>,
) -> Result<QueryPlan> {
    let mut selects = Vec::new();
    let mut ops = Vec::new();
    flatten_compound(body, &mut selects, &mut ops)?;
    let mut arms = selects
        .iter()
        .map(|select| plan_select(cx, select, None, outer));
    let first = arms
        .next()
        .expect("a compound SELECT has at least two sides")?;
    let mut root = first.root;
    for (op, arm) in ops.into_iter().zip(arms) {
        let arm = arm?;
        if arm.columns.len() != first.columns.len() {
            return Err(Error::Parse(format!(
                "SELECTs to the left and right of {} do not have the same number of result columns",
                op.keyword()
            )));
        }
        root = PlanNode::Compound {
            op,
            left: Box::new(root),
            right: Box::new(arm.root),
        };
    }
    if let Some(order_by) = order_by {
        root = PlanNode::Sort {
            input: Box::new(root),
            keys: compound_sort_keys(cx, order_by, &selects, &first.columns, outer)?,
            reason: "no index returns rows in this order, so all of them are read and sorted"
                .into(),
        };
    }
    Ok(QueryPlan {
        columns: first.columns,
        affinities: first.affinities,
        root,
    })
}

fn flatten_compound<'q>(
    body: &'q ast::SetExpr,
    selects: &mut Vec<&'q ast::Select>,
    ops: &mut Vec<SetOp>,
) -> Result<()> {
    match body {
        ast::SetExpr::Select(select) => selects.push(select),
        ast::SetExpr::SetOperation {
            left,
            op,
            set_quantifier,
            right,
        } => {
            use ast::{SetOperator as O, SetQuantifier as Q};
            flatten_compound(left, selects, ops)?;
            ops.push(match (op, set_quantifier) {
                (O::Union, Q::All) => SetOp::UnionAll,
                (O::Union, Q::None | Q::Distinct) => SetOp::Union,
                (O::Intersect, Q::None) => SetOp::Intersect,
                (O::Except, Q::None) => SetOp::Except,
                _ => return unsupported(format!("{op} {set_quantifier}")),
            });
            flatten_compound(right, selects, ops)?;
        }
        ast::SetExpr::Query(_) => return unsupported("parenthesized SELECT"),
        ast::SetExpr::Values(_) => return unsupported("VALUES as a query"),
        _ => return unsupported("this kind of query"),
    }
    Ok(())
}

/// ORDER BY on a compound SELECT sorts by result columns. A term picks one by
/// position, by an `AS` name, or by being the same expression as one of the
/// columns, trying each SELECT from the left, as SQLite does.
fn compound_sort_keys(
    cx: &Planner,
    order_by: &ast::OrderBy,
    selects: &[&ast::Select],
    columns: &[String],
    outer: Option<&Scope>,
) -> Result<Vec<SortKey>> {
    let mut keys = Vec::new();
    for (i, term) in order_by_terms(order_by)?.iter().enumerate() {
        let (descending, nulls_first) = sort_direction(term)?;
        let position = match folded_integer(&term.expr) {
            Some(n) => match usize::try_from(n)
                .ok()
                .filter(|n| (1..=columns.len()).contains(n))
            {
                Some(n) => n - 1,
                None => {
                    return Err(Error::Parse(format!(
                        "{} ORDER BY term out of range - should be between 1 and {}",
                        ordinal(i + 1),
                        columns.len()
                    )));
                }
            },
            None => {
                let mut found = None;
                for select in selects {
                    found = matching_column(cx, select, &term.expr, outer)?;
                    if found.is_some() {
                        break;
                    }
                }
                found.ok_or_else(|| {
                    Error::Parse(format!(
                        "{} ORDER BY term does not match any column in the result set",
                        ordinal(i + 1)
                    ))
                })?
            }
        };
        keys.push(SortKey {
            expr: Expr::Column {
                index: position,
                name: columns[position].clone(),
                affinity: Affinity::Blob,
            },
            descending,
            nulls_first,
        });
    }
    Ok(keys)
}

/// The result column of `select` that `term` names or repeats, if any.
fn matching_column(
    cx: &Planner,
    select: &ast::Select,
    term: &ast::Expr,
    outer: Option<&Scope>,
) -> Result<Option<usize>> {
    let (scope, _) = plan_from(cx, &select.from, outer)?;
    let items = select_items(&scope, &select.projection)?;
    if let ast::Expr::Identifier(ident) = term
        && let Some(i) = items
            .iter()
            .position(|item| item.is_alias && item.name.eq_ignore_ascii_case(&ident.value))
    {
        return Ok(Some(i));
    }
    let mut binder = Binder {
        scope: &scope,
        items: &[],
        aggregates: Vec::new(),
        cx: Some(cx),
    };
    // A term that does not even bind here cannot match this SELECT.
    let Ok(term) = binder.bind(term, Ctx::GROUPED) else {
        return Ok(None);
    };
    for (i, item) in items.iter().enumerate() {
        if binder.bind_item(item, Ctx::GROUPED)? == term {
            return Ok(Some(i));
        }
    }
    Ok(None)
}
/// Whether the SELECT has DISTINCT, after rejecting every part of a SELECT
/// that Fishbowl does not handle.
fn select_is_distinct(select: &ast::Select) -> Result<bool> {
    if select.top.is_some()
        || select.into.is_some()
        || select.exclude.is_some()
        || select.select_modifiers.is_some()
        || select.prewhere.is_some()
        || select.qualify.is_some()
        || select.value_table_mode.is_some()
        || !select.optimizer_hints.is_empty()
        || !select.lateral_views.is_empty()
        || !select.connect_by.is_empty()
        || !select.cluster_by.is_empty()
        || !select.distribute_by.is_empty()
        || !select.sort_by.is_empty()
        || !select.named_window.is_empty()
        || select.flavor != ast::SelectFlavor::Standard
    {
        return unsupported("this SELECT clause");
    }
    match &select.distinct {
        // `SELECT ALL` is the default behavior, spelled out.
        None | Some(ast::Distinct::All) => Ok(false),
        Some(ast::Distinct::Distinct) => Ok(true),
        Some(ast::Distinct::On(_)) => unsupported("DISTINCT ON"),
    }
}

/// One table, view or subquery in FROM, planned, and how it joins the ones
/// before it.
struct FromItem<'q> {
    node: PlanNode,
    name: String,
    kind: JoinKind,
    /// The ON condition, bound once every table in FROM is in scope.
    on: Option<&'q ast::Expr>,
}

/// Plans each table in FROM and puts their columns in one scope, in order.
/// A row of the join is a row of each table, one after the other.
fn plan_from<'q, 's>(
    cx: &Planner,
    from: &'q [ast::TableWithJoins],
    outer: Option<&'s Scope<'s>>,
) -> Result<(Scope<'s>, Vec<FromItem<'q>>)> {
    let mut scope = Scope::new(outer);
    let mut items = Vec::new();
    for table in from {
        // A comma between tables is a join with no condition.
        add_joins(cx, table, JoinKind::Inner, None, &mut scope, &mut items)?;
    }
    Ok((scope, items))
}

/// Adds `a JOIN b ON ... JOIN c ...` to FROM, joined to what is there with
/// `kind` and `on`.
fn add_joins<'q>(
    cx: &Planner,
    table: &'q ast::TableWithJoins,
    kind: JoinKind,
    on: Option<&'q ast::Expr>,
    scope: &mut Scope,
    items: &mut Vec<FromItem<'q>>,
) -> Result<()> {
    add_relation(cx, &table.relation, kind, on, scope, items)?;
    for join in &table.joins {
        let (kind, on) = join_kind(join)?;
        add_relation(cx, &join.relation, kind, on, scope, items)?;
    }
    Ok(())
}

fn join_kind(join: &ast::Join) -> Result<(JoinKind, Option<&ast::Expr>)> {
    use ast::JoinOperator as J;
    if join.global {
        return unsupported("GLOBAL JOIN");
    }
    let (kind, constraint) = match &join.join_operator {
        J::Join(c) | J::Inner(c) | J::CrossJoin(c) => (JoinKind::Inner, c),
        J::Left(c) | J::LeftOuter(c) => (JoinKind::Left, c),
        J::Right(_) | J::RightOuter(_) => return unsupported("RIGHT JOIN"),
        J::FullOuter(_) => return unsupported("FULL JOIN"),
        _ => return unsupported("this kind of JOIN"),
    };
    match constraint {
        ast::JoinConstraint::On(e) => Ok((kind, Some(e))),
        ast::JoinConstraint::None => Ok((kind, None)),
        ast::JoinConstraint::Using(_) => unsupported("JOIN ... USING"),
        ast::JoinConstraint::Natural => unsupported("NATURAL JOIN"),
    }
}

/// Adds one table, view or subquery to FROM. A join in parentheses is added
/// table by table, where that keeps its meaning: first in FROM, after a
/// comma, or when every join in it is an inner join.
fn add_relation<'q>(
    cx: &Planner,
    factor: &'q ast::TableFactor,
    kind: JoinKind,
    on: Option<&'q ast::Expr>,
    scope: &mut Scope,
    items: &mut Vec<FromItem<'q>>,
) -> Result<()> {
    if let ast::TableFactor::NestedJoin {
        table_with_joins,
        alias: None,
    } = factor
    {
        let all_inner = table_with_joins
            .joins
            .iter()
            .all(|j| join_kind(j).is_ok_and(|(kind, _)| kind == JoinKind::Inner));
        let flat = items.is_empty() || kind == JoinKind::Inner && (on.is_none() || all_inner);
        if !flat {
            return unsupported("a join in parentheses after LEFT JOIN or before ON");
        }
        return add_joins(cx, table_with_joins, kind, on, scope, items);
    }
    let (node, name) = plan_table(cx, factor, scope)?;
    items.push(FromItem {
        node,
        name,
        kind,
        on,
    });
    Ok(())
}

/// Plans one table, view or subquery in FROM and adds its columns to `scope`.
/// Returns the plan and the name it goes by.
fn plan_table(
    cx: &Planner,
    factor: &ast::TableFactor,
    scope: &mut Scope,
) -> Result<(PlanNode, String)> {
    match factor {
        ast::TableFactor::Table {
            name,
            alias,
            args: None,
            ..
        } => {
            let name = object_name(name)?;
            if let Some(view) = cx.catalog.view(&name) {
                return plan_view(cx, view, alias.as_ref(), scope);
            }
            let schema = cx.catalog.table(&name)?;
            let name = scope.add_table(schema, alias.as_ref());
            Ok((scan(schema), name))
        }
        ast::TableFactor::Derived {
            lateral: false,
            subquery,
            alias,
            sample: None,
        } => {
            if alias.as_ref().is_some_and(|a| !a.columns.is_empty()) {
                return unsupported("column names after a subquery's alias");
            }
            // A subquery in FROM sees the same enclosing queries as this one,
            // but not the other tables in this FROM.
            let plan = plan_query(cx, subquery, scope.parent)?;
            let alias = alias.as_ref().map(|a| a.name.value.clone());
            let name = alias.clone().unwrap_or_else(|| "subquery".into());
            scope.add_query(alias, &plan.columns, &plan.affinities);
            let node = PlanNode::SubqueryScan {
                input: Box::new(plan.root),
                name: name.clone(),
            };
            Ok((node, name))
        }
        _ => unsupported("subquery or table function in FROM"),
    }
}

/// A view in FROM is planned like a subquery, from its stored SELECT.
fn plan_view(
    cx: &Planner,
    view: &View,
    alias: Option<&ast::TableAlias>,
    scope: &mut Scope,
) -> Result<(PlanNode, String)> {
    if alias.is_some_and(|a| !a.columns.is_empty()) {
        return unsupported("column names after a view's alias");
    }
    if cx
        .views
        .borrow()
        .iter()
        .any(|v| v.eq_ignore_ascii_case(&view.name))
    {
        return Err(Error::Catalog(format!(
            "view {} is circularly defined",
            view.name
        )));
    }
    // A view cannot see the columns of the query that uses it.
    cx.views.borrow_mut().push(view.name.clone());
    let plan = plan_query(cx, &view.query, None);
    cx.views.borrow_mut().pop();
    let plan = plan?;
    let columns = if view.columns.is_empty() {
        plan.columns
    } else if view.columns.len() == plan.columns.len() {
        view.columns.clone()
    } else {
        return Err(Error::Catalog(format!(
            "expected {} columns for '{}' but got {}",
            view.columns.len(),
            view.name,
            plan.columns.len()
        )));
    };
    let name = alias.map_or_else(|| view.name.clone(), |a| a.name.value.clone());
    scope.add_query(Some(name.clone()), &columns, &plan.affinities);
    let node = PlanNode::SubqueryScan {
        input: Box::new(plan.root),
        name: name.clone(),
    };
    Ok((node, name))
}

/// How the rows of a table are read.
fn scan(schema: &TableSchema) -> PlanNode {
    PlanNode::SeqScan {
        table: schema.name.clone(),
        reason: format!("{} has no index, so every row is read", schema.name),
    }
}

/// Joins the FROM items left to right with nested loops, and places every
/// condition from WHERE and ON as low in the tree as it can go: on the table
/// it reads, or at the first join where all the columns it reads are there.
/// A condition then drops rows before they are joined, instead of after.
///
/// A LEFT JOIN's ON condition stays on its join, unless it reads only the
/// right table: dropping left rows would lose the ones that should come out
/// padded with NULLs. For the same reason a WHERE condition that reads a LEFT
/// JOIN's table waits until after that join.
fn join_tree(
    from: Vec<FromItem>,
    binder: &mut Binder,
    predicate: Option<Expr>,
) -> Result<PlanNode> {
    let scope = binder.scope;
    let tables = &scope.tables;
    if from.is_empty() {
        return Ok(with_filter(
            PlanNode::OneRow,
            predicate.into_iter().collect(),
        ));
    }
    if from.len() == 1 {
        let first = from.into_iter().next().expect("checked above");
        return Ok(with_filter(first.node, predicate.into_iter().collect()));
    }

    // Where each table's columns end in a joined row.
    let ends: Vec<usize> = tables.iter().map(|t| t.offset + t.columns.len()).collect();
    let n = from.len();
    // Per table: conditions on its rows alone, conditions on its join, and
    // filters after its join.
    let mut own: Vec<Vec<Expr>> = vec![Vec::new(); n];
    let mut on_join: Vec<Vec<Expr>> = vec![Vec::new(); n];
    let mut after: Vec<Vec<Expr>> = vec![Vec::new(); n];
    let reads_only = |e: &Expr, k: usize| {
        !has_subquery(e)
            && columns_read(e).next().is_some()
            && columns_read(e).all(|c| (tables[k].offset..ends[k]).contains(&c))
    };

    let mut conjuncts = Vec::new();
    for (k, item) in from.iter().enumerate() {
        let Some(on) = item.on else { continue };
        let on = binder.bind(on, Ctx::ON)?;
        match item.kind {
            JoinKind::Left if columns_read(&on).any(|c| c >= ends[k]) => {
                return Err(Error::Parse(
                    "ON clause references tables to its right".into(),
                ));
            }
            JoinKind::Left => {
                for part in split_and(on) {
                    let target = if reads_only(&part, k) {
                        &mut own
                    } else {
                        &mut on_join
                    };
                    target[k].push(part);
                }
            }
            // An inner join's ON condition means the same as one in WHERE.
            JoinKind::Inner => conjuncts.extend(split_and(on)),
        }
    }
    conjuncts.extend(predicate.into_iter().flat_map(split_and));
    for conjunct in conjuncts {
        // A subquery may read any column of the row, so it waits for all.
        let level = if has_subquery(&conjunct) {
            n - 1
        } else {
            columns_read(&conjunct)
                .map(|c| ends.iter().position(|&end| c < end).unwrap_or(n - 1))
                .max()
                .unwrap_or(0)
        };
        let target =
            if level == 0 || reads_only(&conjunct, level) && from[level].kind == JoinKind::Inner {
                &mut own
            } else if from[level].kind == JoinKind::Left {
                &mut after
            } else {
                &mut on_join
            };
        target[level].push(conjunct);
    }

    let mut root = None;
    for (k, item) in from.into_iter().enumerate() {
        // The table's own conditions see only its row, so their columns
        // move to where the table's columns start.
        let mut filters = std::mem::take(&mut own[k]);
        filters
            .iter_mut()
            .for_each(|e| e.shift_columns(tables[k].offset));
        let node = with_filter(item.node, filters);
        let joined = match root {
            None => node,
            Some(left) => PlanNode::NestedLoopJoin {
                left: Box::new(left),
                right: Box::new(PlanNode::Materialize {
                    input: Box::new(node),
                }),
                kind: item.kind,
                condition: and_all(std::mem::take(&mut on_join[k])),
                right_width: tables[k].columns.len(),
                reason: format!(
                    "no index can find the matching rows of {}, so each left row is checked against all of them",
                    item.name
                ),
            },
        };
        root = Some(with_filter(joined, std::mem::take(&mut after[k])));
    }
    Ok(root.expect("FROM has a first table"))
}

fn with_filter(input: PlanNode, conjuncts: Vec<Expr>) -> PlanNode {
    match and_all(conjuncts) {
        Some(predicate) => PlanNode::Filter {
            input: Box::new(input),
            predicate,
        },
        None => input,
    }
}

/// `a AND b AND c` as its parts.
fn split_and(expr: Expr) -> Vec<Expr> {
    match expr {
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => {
            let mut parts = split_and(*left);
            parts.extend(split_and(*right));
            parts
        }
        other => vec![other],
    }
}

fn and_all(conjuncts: Vec<Expr>) -> Option<Expr> {
    conjuncts.into_iter().reduce(|left, right| Expr::Binary {
        op: BinaryOp::And,
        left: Box::new(left),
        right: Box::new(right),
    })
}

/// The row positions an expression reads, not counting outer columns.
fn columns_read(expr: &Expr) -> impl Iterator<Item = usize> {
    let mut columns = Vec::new();
    expr.walk(&mut |e| {
        if let Expr::Column { index, .. } = e {
            columns.push(*index);
        }
    });
    columns.into_iter()
}

fn has_subquery(expr: &Expr) -> bool {
    let mut found = false;
    expr.walk(&mut |e| found |= matches!(e, Expr::Subquery { .. }));
    found
}

/// Whether a plan reads a column of a query outside it. `nesting` counts the
/// subqueries entered on the way down.
fn reads_outside(node: &PlanNode, nesting: usize) -> bool {
    let mut found = false;
    for expr in node.exprs() {
        expr.walk(&mut |e| match e {
            Expr::OuterColumn { depth, .. } => found |= *depth > nesting,
            Expr::Subquery { query, .. } => found |= reads_outside(&query.plan.root, nesting + 1),
            _ => {}
        });
    }
    found
        || node
            .children()
            .iter()
            .any(|child| reads_outside(child, nesting))
}

/// The `LIMIT` and `OFFSET` expressions, if any.
fn limit_clause(query: &ast::Query) -> Result<Option<(&ast::Expr, Option<&ast::Expr>)>> {
    match &query.limit_clause {
        None => Ok(None),
        Some(ast::LimitClause::OffsetCommaLimit { offset, limit }) => {
            Ok(Some((limit, Some(offset))))
        }
        Some(ast::LimitClause::LimitOffset {
            limit,
            offset,
            limit_by,
        }) => {
            if !limit_by.is_empty() {
                return unsupported("LIMIT BY");
            }
            if offset
                .as_ref()
                .is_some_and(|o| o.rows != ast::OffsetRows::None)
            {
                return unsupported("OFFSET ... ROWS");
            }
            match (limit, offset) {
                (Some(limit), offset) => Ok(Some((limit, offset.as_ref().map(|o| &o.value)))),
                (None, None) => Ok(None),
                (None, Some(_)) => Err(Error::Parse("OFFSET needs a LIMIT".into())),
            }
        }
    }
}

/// SQLite names an unaliased result column after its source text.
fn output_name(expr: &ast::Expr) -> String {
    match expr {
        ast::Expr::Identifier(ident) => ident.value.clone(),
        ast::Expr::CompoundIdentifier(parts) => {
            parts.last().map(|p| p.value.clone()).unwrap_or_default()
        }
        other => other.to_string(),
    }
}

/// One result column before binding: an expression from the SELECT list, or
/// a table column that `*` expanded to.
struct Item<'q> {
    name: String,
    /// Whether `name` came from `AS`, so other clauses may refer to it.
    is_alias: bool,
    source: Source<'q>,
}

enum Source<'q> {
    Expr(&'q ast::Expr),
    /// A table column, already bound.
    Column(Expr),
}

fn select_items<'q>(scope: &Scope, projection: &'q [ast::SelectItem]) -> Result<Vec<Item<'q>>> {
    let mut items = Vec::new();
    for item in projection {
        match item {
            ast::SelectItem::UnnamedExpr(e) => items.push(Item {
                name: output_name(e),
                is_alias: false,
                source: Source::Expr(e),
            }),
            ast::SelectItem::ExprWithAlias { expr, alias } => items.push(Item {
                name: alias.value.clone(),
                is_alias: true,
                source: Source::Expr(expr),
            }),
            ast::SelectItem::Wildcard(options) => {
                check_wildcard_options(options)?;
                scope.expand_wildcard(None, &mut items)?
            }
            ast::SelectItem::QualifiedWildcard(
                ast::SelectItemQualifiedWildcardKind::ObjectName(name),
                options,
            ) => {
                check_wildcard_options(options)?;
                scope.expand_wildcard(Some(&object_name(name)?), &mut items)?
            }
            _ => return unsupported("this kind of select item"),
        }
    }
    Ok(items)
}

fn check_wildcard_options(options: &ast::WildcardAdditionalOptions) -> Result<()> {
    if options.opt_ilike.is_some()
        || options.opt_exclude.is_some()
        || options.opt_except.is_some()
        || options.opt_replace.is_some()
        || options.opt_rename.is_some()
    {
        return unsupported("options after *");
    }
    Ok(())
}

/// The names visible to an expression: the tables in FROM, and through
/// `parent`, those of the queries this one is nested in.
struct Scope<'a> {
    tables: Vec<ScopeTable>,
    parent: Option<&'a Scope<'a>>,
}

/// One table, view or subquery in FROM.
struct ScopeTable {
    /// The alias, or else the table or view name. A subquery in FROM without
    /// an alias has none, so no qualified name reaches its columns.
    name: Option<String>,
    columns: Vec<(String, Affinity)>,
    /// Where its columns start in a joined row.
    offset: usize,
}

impl ScopeTable {
    fn matches(&self, qualifier: &str) -> bool {
        self.name
            .as_ref()
            .is_some_and(|n| n.eq_ignore_ascii_case(qualifier))
    }
}

impl<'a> Scope<'a> {
    fn new(parent: Option<&'a Scope<'a>>) -> Self {
        Scope {
            tables: Vec::new(),
            parent,
        }
    }

    /// Columns in one input row.
    fn width(&self) -> usize {
        self.tables.last().map_or(0, |t| t.offset + t.columns.len())
    }

    /// Adds a table under its alias or name, and returns that name.
    fn add_table(&mut self, schema: &TableSchema, alias: Option<&ast::TableAlias>) -> String {
        let name = alias.map_or_else(|| schema.name.clone(), |a| a.name.value.clone());
        self.tables.push(ScopeTable {
            name: Some(name.clone()),
            columns: schema
                .columns
                .iter()
                .map(|c| (c.name.clone(), c.affinity))
                .collect(),
            offset: self.width(),
        });
        name
    }

    /// Adds the result of a view or a subquery. Repeated names get a `:1`,
    /// `:2` suffix as in SQLite, so each column can be named. A column
    /// computed by an expression has no affinity; Fishbowl uses BLOB, which
    /// differs from SQLite only when compared with a TEXT column.
    fn add_query(
        &mut self,
        name: Option<String>,
        names: &[String],
        affinities: &[Option<Affinity>],
    ) {
        let mut columns: Vec<(String, Affinity)> = Vec::new();
        for (column, affinity) in names.iter().zip(affinities) {
            let mut unique = column.clone();
            let mut n = 0;
            while columns.iter().any(|(c, _)| c.eq_ignore_ascii_case(&unique)) {
                n += 1;
                unique = format!("{column}:{n}");
            }
            columns.push((unique, affinity.unwrap_or(Affinity::Blob)));
        }
        self.tables.push(ScopeTable {
            name,
            columns,
            offset: self.width(),
        });
    }

    /// Finds a column among this query's own tables. `Ok(None)` means none
    /// of them has it.
    fn local_column(&self, qualifier: Option<&str>, name: &str) -> Result<Option<Expr>> {
        let qualify = self.tables.len() > 1;
        Ok(self
            .find_column(qualifier, name)?
            .map(|(table, i)| column_expr(table, i, qualify)))
    }

    fn find_column(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Result<Option<(&ScopeTable, usize)>> {
        let mut found = None;
        for table in &self.tables {
            if qualifier.is_some_and(|q| !table.matches(q)) {
                continue;
            }
            if let Some(i) = table
                .columns
                .iter()
                .position(|(c, _)| c.eq_ignore_ascii_case(name))
            {
                if found.is_some() {
                    return Err(Error::Catalog(format!(
                        "ambiguous column name: {}",
                        qualified(qualifier, name)
                    )));
                }
                found = Some((table, i));
            }
        }
        Ok(found)
    }

    fn expand_wildcard(&self, qualifier: Option<&str>, items: &mut Vec<Item>) -> Result<()> {
        if self.tables.is_empty() && qualifier.is_none() {
            return Err(Error::Catalog("no tables specified".into()));
        }
        let mut matched = false;
        for table in &self.tables {
            if qualifier.is_some_and(|q| !table.matches(q)) {
                continue;
            }
            matched = true;
            for (i, (column, _)) in table.columns.iter().enumerate() {
                items.push(Item {
                    name: column.clone(),
                    is_alias: false,
                    source: Source::Column(column_expr(table, i, self.tables.len() > 1)),
                });
            }
        }
        match qualifier {
            Some(q) if !matched => Err(Error::Catalog(format!("no such table: {q}"))),
            _ => Ok(()),
        }
    }
}

/// Column `i` of a table in scope. `qualify` names its table too, for the
/// plan display, when there is more than one it could be from.
fn column_expr(table: &ScopeTable, i: usize, qualify: bool) -> Expr {
    let (column, affinity) = &table.columns[i];
    let name = match &table.name {
        Some(t) if qualify => format!("{t}.{column}"),
        _ => column.clone(),
    };
    Expr::Column {
        index: table.offset + i,
        name,
        affinity: *affinity,
    }
}

fn qualified(qualifier: Option<&str>, name: &str) -> String {
    qualifier.map_or(name.to_string(), |q| format!("{q}.{name}"))
}

/// What an aggregate call means where it appears.
#[derive(Debug, Clone, Copy)]
enum Aggregates {
    /// Allowed: it becomes a reference to the Aggregate step's output.
    Collect,
    /// An error, as in WHERE or inside another aggregate.
    Misuse,
    /// An error with GROUP BY's own message.
    GroupBy,
}

/// The rules for binding one clause.
#[derive(Debug, Clone, Copy)]
struct Ctx {
    aggregates: Aggregates,
    /// Whether an unknown name may refer to a result column's alias. SQLite
    /// allows `SELECT a + 1 AS b FROM t WHERE b > 2`.
    aliases: bool,
}

impl Ctx {
    /// WHERE, and every clause of a non-aggregate query.
    const ROW: Ctx = Ctx {
        aggregates: Aggregates::Misuse,
        aliases: true,
    };
    /// The result columns, HAVING and ORDER BY of an aggregate query.
    const GROUPED: Ctx = Ctx {
        aggregates: Aggregates::Collect,
        aliases: true,
    };
    const GROUP_BY: Ctx = Ctx {
        aggregates: Aggregates::GroupBy,
        aliases: true,
    };
    /// INSERT values, LIMIT and OFFSET.
    const CONSTANT: Ctx = Ctx {
        aggregates: Aggregates::Misuse,
        aliases: false,
    };
    /// The ON condition of a join.
    const ON: Ctx = Ctx::CONSTANT;

    fn without_aliases(self) -> Ctx {
        Ctx {
            aliases: false,
            ..self
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Clause {
    OrderBy,
    GroupBy,
}

/// Resolves names in expressions ("binding") and collects the aggregates
/// they use.
struct Binder<'a> {
    scope: &'a Scope<'a>,
    items: &'a [Item<'a>],
    /// Each distinct aggregate call, in the order SQLite numbers them.
    aggregates: Vec<AggregateCall>,
    /// What subqueries are planned with. Without it they are not allowed.
    cx: Option<&'a Planner<'a>>,
}

impl<'a> Binder<'a> {
    /// A binder with no result columns: INSERT values, LIMIT, and the
    /// expressions of UPDATE and DELETE.
    fn without_items(scope: &'a Scope<'a>) -> Self {
        Binder {
            scope,
            items: &[],
            aggregates: Vec::new(),
            cx: None,
        }
    }

    fn bind_item(&mut self, item: &Item, ctx: Ctx) -> Result<Expr> {
        match &item.source {
            Source::Expr(e) => self.bind(e, ctx),
            Source::Column(column) => Ok(column.clone()),
        }
    }

    fn sort_keys(&mut self, order_by: &ast::OrderBy, ctx: Ctx) -> Result<Vec<SortKey>> {
        let mut keys = Vec::new();
        for (i, term) in order_by_terms(order_by)?.iter().enumerate() {
            let (descending, nulls_first) = sort_direction(term)?;
            keys.push(SortKey {
                expr: self.result_term(&term.expr, i, Clause::OrderBy, ctx)?,
                descending,
                nulls_first,
            });
        }
        Ok(keys)
    }

    /// Resolves an ORDER BY or GROUP BY term. A number picks a result column
    /// by position (`ORDER BY 2`). In ORDER BY a bare name matching an alias
    /// picks that result column, even over a table column of the same name.
    /// Anything else is an ordinary expression.
    fn result_term(
        &mut self,
        term: &ast::Expr,
        position: usize,
        clause: Clause,
        ctx: Ctx,
    ) -> Result<Expr> {
        let items = self.items;
        if let Some(n) = folded_integer(term) {
            let clause_name = match clause {
                Clause::OrderBy => "ORDER BY",
                Clause::GroupBy => "GROUP BY",
            };
            return match usize::try_from(n)
                .ok()
                .and_then(|n| items.get(n.checked_sub(1)?))
            {
                Some(item) => self.bind_item(item, ctx.without_aliases()),
                None => Err(Error::Parse(format!(
                    "{} {clause_name} term out of range - should be between 1 and {}",
                    ordinal(position + 1),
                    items.len()
                ))),
            };
        }
        if let (Clause::OrderBy, ast::Expr::Identifier(ident)) = (clause, term)
            && let Some(item) = find_alias(items, &ident.value)
        {
            return self.bind_item(item, ctx.without_aliases());
        }
        self.bind(term, ctx)
    }

    /// Resolves a name: first this query's tables, then its result column
    /// aliases, then the tables of each enclosing query, innermost first.
    fn identifier(&mut self, qualifier: Option<&str>, name: &str, ctx: Ctx) -> Result<Expr> {
        if let Some(column) = self.scope.local_column(qualifier, name)? {
            return Ok(column);
        }
        if qualifier.is_none()
            && ctx.aliases
            && let Some(item) = find_alias(self.items, name)
        {
            // The alias's own expression cannot refer to aliases, which rules
            // out cycles like `SELECT b AS a, a AS b`.
            return self.bind_item(item, ctx.without_aliases());
        }
        let mut depth = 0;
        let mut scope = self.scope;
        while let Some(parent) = scope.parent {
            depth += 1;
            if let Some((table, i)) = parent.find_column(qualifier, name)? {
                // An outer column always shows its table, to tell it apart.
                let Expr::Column {
                    index,
                    name,
                    affinity,
                } = column_expr(table, i, true)
                else {
                    unreachable!("column_expr makes a column")
                };
                return Ok(Expr::OuterColumn {
                    depth,
                    index,
                    name,
                    affinity,
                });
            }
            scope = parent;
        }
        Err(Error::Catalog(format!(
            "no such column: {}",
            qualified(qualifier, name)
        )))
    }

    /// Plans a subquery inside an expression, with this query's names in
    /// reach.
    fn subquery(&mut self, query: &ast::Query, kind: SubqueryKind) -> Result<Expr> {
        let Some(cx) = self.cx else {
            return unsupported("subquery outside SELECT");
        };
        let plan = plan_query(cx, query, Some(self.scope))?;
        let one_column = matches!(kind, SubqueryKind::Exists { .. }) || plan.columns.len() == 1;
        if !one_column {
            return Err(Error::Parse(format!(
                "sub-select returns {} columns - expected 1",
                plan.columns.len()
            )));
        }
        let correlated = reads_outside(&plan.root, 0);
        Ok(Expr::Subquery {
            kind,
            query: Box::new(Subquery {
                plan,
                correlated,
                sql: query.to_string(),
            }),
        })
    }

    fn bind(&mut self, expr: &ast::Expr, ctx: Ctx) -> Result<Expr> {
        let boxed = |e: Expr| Box::new(e);
        Ok(match expr {
            ast::Expr::Identifier(ident) => self.identifier(None, &ident.value, ctx)?,
            ast::Expr::CompoundIdentifier(parts) => match parts.as_slice() {
                [table, column] => self.identifier(Some(&table.value), &column.value, ctx)?,
                _ => return unsupported("schema-qualified column names"),
            },
            ast::Expr::Value(v) => Expr::Literal(literal(&v.value)?),
            ast::Expr::Nested(inner) => self.bind(inner, ctx)?,
            ast::Expr::IsNull(inner) => Expr::IsNull {
                expr: boxed(self.bind(inner, ctx)?),
                negated: false,
            },
            ast::Expr::IsNotNull(inner) => Expr::IsNull {
                expr: boxed(self.bind(inner, ctx)?),
                negated: true,
            },
            ast::Expr::IsDistinctFrom(left, right) | ast::Expr::IsNotDistinctFrom(left, right) => {
                let distinct = matches!(expr, ast::Expr::IsDistinctFrom(..));
                match truth_keyword(right) {
                    // `x IS NOT DISTINCT FROM TRUE` is the truth test `x IS TRUE`.
                    Some(truth) => Expr::IsTruth {
                        expr: boxed(self.bind(left, ctx)?),
                        truth,
                        negated: distinct,
                    },
                    None => Expr::Is {
                        left: boxed(self.bind(left, ctx)?),
                        right: boxed(self.bind(right, ctx)?),
                        distinct,
                    },
                }
            }
            ast::Expr::IsTrue(inner)
            | ast::Expr::IsNotTrue(inner)
            | ast::Expr::IsFalse(inner)
            | ast::Expr::IsNotFalse(inner) => Expr::IsTruth {
                expr: boxed(self.bind(inner, ctx)?),
                truth: matches!(expr, ast::Expr::IsTrue(_) | ast::Expr::IsNotTrue(_)),
                negated: matches!(expr, ast::Expr::IsNotTrue(_) | ast::Expr::IsNotFalse(_)),
            },
            // 9223372036854775808 alone is too big for an integer, but with a
            // minus sign it is the smallest one, as in SQLite.
            ast::Expr::UnaryOp {
                op: ast::UnaryOperator::Minus,
                expr: inner,
            } if is_i64_min_magnitude(inner) => Expr::Literal(Value::Integer(i64::MIN)),
            ast::Expr::UnaryOp { op, expr } => {
                let op = match op {
                    ast::UnaryOperator::Minus => UnaryOp::Negate,
                    ast::UnaryOperator::Plus => UnaryOp::Plus,
                    ast::UnaryOperator::Not => UnaryOp::Not,
                    other => return unsupported(format!("unary operator {other}")),
                };
                Expr::Unary {
                    op,
                    expr: boxed(self.bind(expr, ctx)?),
                }
            }
            ast::Expr::BinaryOp { left, op, right } => Expr::Binary {
                op: binary_op(op)?,
                left: boxed(self.bind(left, ctx)?),
                right: boxed(self.bind(right, ctx)?),
            },
            ast::Expr::Between {
                expr,
                negated,
                low,
                high,
            } => Expr::Between {
                expr: boxed(self.bind(expr, ctx)?),
                low: boxed(self.bind(low, ctx)?),
                high: boxed(self.bind(high, ctx)?),
                negated: *negated,
            },
            ast::Expr::InList {
                expr,
                list,
                negated,
            } => Expr::InList {
                expr: boxed(self.bind(expr, ctx)?),
                list: list
                    .iter()
                    .map(|e| self.bind(e, ctx))
                    .collect::<Result<_>>()?,
                negated: *negated,
            },
            ast::Expr::Case {
                operand,
                conditions,
                else_result,
                ..
            } => Expr::Case {
                operand: match operand {
                    Some(operand) => Some(boxed(self.bind(operand, ctx)?)),
                    None => None,
                },
                branches: conditions
                    .iter()
                    .map(|w| Ok((self.bind(&w.condition, ctx)?, self.bind(&w.result, ctx)?)))
                    .collect::<Result<_>>()?,
                otherwise: match else_result {
                    Some(e) => Some(boxed(self.bind(e, ctx)?)),
                    None => None,
                },
            },
            ast::Expr::Cast {
                kind: ast::CastKind::Cast,
                expr,
                data_type,
                format: None,
            } => {
                let type_name = data_type.to_string();
                let affinity = Affinity::from_declared_type(&type_name);
                // Blobs are not a value type in Fishbowl yet.
                if affinity == Affinity::Blob {
                    return unsupported("CAST AS BLOB");
                }
                Expr::Cast {
                    expr: boxed(self.bind(expr, ctx)?),
                    type_name,
                    affinity,
                }
            }
            ast::Expr::Function(func) => self.function(func, ctx)?,
            ast::Expr::Subquery(query) => self.subquery(query, SubqueryKind::Scalar)?,
            ast::Expr::Exists { subquery, negated } => {
                self.subquery(subquery, SubqueryKind::Exists { negated: *negated })?
            }
            ast::Expr::InSubquery {
                expr,
                subquery,
                negated,
            } => {
                let kind = SubqueryKind::In {
                    expr: boxed(self.bind(expr, ctx)?),
                    negated: *negated,
                };
                self.subquery(subquery, kind)?
            }
            // `substr(x, 2, 3)` gets its own syntax node in the parser.
            ast::Expr::Substring {
                expr,
                substring_from,
                substring_for,
                special: true,
                ..
            } => {
                let mut args = vec![self.bind(expr, ctx)?];
                for e in [substring_from, substring_for].into_iter().flatten() {
                    args.push(self.bind(e, ctx)?);
                }
                Expr::Function {
                    func: ScalarFunc::Substr,
                    args,
                }
            }
            // So does `trim(x)` and `trim(x, 'chars')`.
            ast::Expr::Trim {
                expr,
                trim_where: None,
                trim_what: None,
                trim_characters,
            } => {
                let mut args = vec![self.bind(expr, ctx)?];
                for e in trim_characters.iter().flatten() {
                    args.push(self.bind(e, ctx)?);
                }
                if !ScalarFunc::Trim.accepts(args.len()) {
                    return Err(wrong_argument_count("trim"));
                }
                Expr::Function {
                    func: ScalarFunc::Trim,
                    args,
                }
            }
            other => return unsupported(expr_kind(other)),
        })
    }

    fn function(&mut self, func: &ast::Function, ctx: Ctx) -> Result<Expr> {
        if func.over.is_some() {
            return unsupported("window functions");
        }
        if func.filter.is_some() {
            return unsupported("FILTER on an aggregate");
        }
        if func.uses_odbc_syntax
            || func.null_treatment.is_some()
            || !func.within_group.is_empty()
            || !matches!(func.parameters, ast::FunctionArguments::None)
        {
            return unsupported("this kind of function call");
        }
        let name = object_name(&func.name)?.to_ascii_lowercase();
        let ast::FunctionArguments::List(list) = &func.args else {
            return unsupported("a function call without an argument list");
        };
        if !list.clauses.is_empty() {
            return unsupported("clauses inside a function's arguments");
        }
        let distinct = matches!(
            list.duplicate_treatment,
            Some(ast::DuplicateTreatment::Distinct)
        );
        let mut star = false;
        let mut args = Vec::new();
        for arg in &list.args {
            match arg {
                ast::FunctionArg::Unnamed(ast::FunctionArgExpr::Expr(e)) => args.push(e),
                ast::FunctionArg::Unnamed(ast::FunctionArgExpr::Wildcard) => star = true,
                _ => return unsupported("this kind of function argument"),
            }
        }
        if star {
            // Only count(*) takes a star, and then as its only argument.
            if name != "count" || distinct || list.args.len() != 1 {
                return Err(Error::Parse(format!("{name}(*) is not valid SQL")));
            }
        }

        if let Some(func) = AggregateFunc::from_name(&name).filter(|f| f.accepts(args.len())) {
            if distinct && args.len() != 1 {
                return Err(Error::Parse(
                    "DISTINCT aggregates must have exactly one argument".into(),
                ));
            }
            match ctx.aggregates {
                Aggregates::Collect => {}
                Aggregates::Misuse => {
                    return Err(Error::Parse(format!(
                        "misuse of aggregate function {name}()"
                    )));
                }
                Aggregates::GroupBy => {
                    return Err(Error::Parse(
                        "aggregate functions are not allowed in the GROUP BY clause".into(),
                    ));
                }
            }
            // An aggregate's arguments are per-row expressions.
            let inner = Ctx {
                aggregates: Aggregates::Misuse,
                ..ctx
            };
            let call = AggregateCall {
                func,
                distinct,
                args: args
                    .iter()
                    .map(|e| self.bind(e, inner))
                    .collect::<Result<_>>()?,
            };
            // SQLite computes `max(t.a)` in the query that owns `t` when the
            // arguments read only an enclosing query's columns.
            let (mut own, mut outer) = (false, false);
            for arg in &call.args {
                arg.walk(&mut |e| match e {
                    Expr::Column { .. } => own = true,
                    Expr::OuterColumn { .. } => outer = true,
                    _ => {}
                });
            }
            if outer && !own {
                return unsupported("an aggregate of an enclosing query's columns");
            }
            // The same call twice is computed once, as in SQLite.
            let index = match self.aggregates.iter().position(|a| *a == call) {
                Some(index) => index,
                None => {
                    self.aggregates.push(call.clone());
                    self.aggregates.len() - 1
                }
            };
            return Ok(Expr::Column {
                index: self.scope.width() + index,
                name: call.to_string(),
                affinity: Affinity::Blob,
            });
        }

        if let Some(func) = ScalarFunc::from_name(&name).filter(|f| f.accepts(args.len())) {
            if distinct {
                return Err(Error::Parse(format!(
                    "DISTINCT is only allowed in an aggregate, not {name}()"
                )));
            }
            return Ok(Expr::Function {
                func,
                args: args
                    .iter()
                    .map(|e| self.bind(e, ctx))
                    .collect::<Result<_>>()?,
            });
        }
        if AggregateFunc::from_name(&name).is_some() || ScalarFunc::from_name(&name).is_some() {
            return Err(wrong_argument_count(&name));
        }
        if is_sqlite_function(&name) {
            return unsupported(format!("function {name}()"));
        }
        Err(Error::Catalog(format!("no such function: {name}")))
    }
}

fn order_by_terms(order_by: &ast::OrderBy) -> Result<&[ast::OrderByExpr]> {
    if order_by.interpolate.is_some() {
        return unsupported("ORDER BY ... INTERPOLATE");
    }
    match &order_by.kind {
        ast::OrderByKind::Expressions(terms) => Ok(terms),
        ast::OrderByKind::All(_) => unsupported("ORDER BY ALL"),
    }
}

/// Whether an ORDER BY term is descending, and whether NULLs come first.
fn sort_direction(term: &ast::OrderByExpr) -> Result<(bool, bool)> {
    if term.with_fill.is_some() {
        return unsupported("ORDER BY ... WITH FILL");
    }
    let descending = match &term.options.sort {
        None | Some(ast::OrderBySort::Asc) => false,
        Some(ast::OrderBySort::Desc) => true,
        Some(ast::OrderBySort::Using(_)) => return unsupported("ORDER BY ... USING"),
    };
    // NULL is the smallest value: first ascending, last descending.
    Ok((descending, term.options.nulls_first.unwrap_or(!descending)))
}

fn find_alias<'i>(items: &'i [Item<'i>], name: &str) -> Option<&'i Item<'i>> {
    items
        .iter()
        .find(|item| item.is_alias && item.name.eq_ignore_ascii_case(name))
}

fn wrong_argument_count(name: &str) -> Error {
    Error::Parse(format!("wrong number of arguments to function {name}()"))
}

fn is_i64_min_magnitude(expr: &ast::Expr) -> bool {
    match expr {
        ast::Expr::Nested(inner) => is_i64_min_magnitude(inner),
        ast::Expr::Value(v) => {
            matches!(&v.value, ast::Value::Number(text, _) if text == "9223372036854775808")
        }
        _ => false,
    }
}

/// Whether an expression is the keyword TRUE or FALSE as SQLite sees it.
/// SQLite's parser turns `x IN ()` into FALSE and `x NOT IN ()` into TRUE,
/// unless `x` calls a function.
fn truth_keyword(expr: &ast::Expr) -> Option<bool> {
    match expr {
        ast::Expr::Value(v) => match v.value {
            ast::Value::Boolean(b) => Some(b),
            _ => None,
        },
        ast::Expr::InList {
            expr,
            list,
            negated,
        } if list.is_empty() && !has_function(expr) => Some(*negated),
        ast::Expr::Nested(inner) => truth_keyword(inner),
        _ => None,
    }
}

/// An integer that ORDER BY and GROUP BY read as a result column position,
/// like `2`, `-1` or `(+3)`.
///
/// SQLite's parser simplifies a few constant forms to integers first, so
/// they count too: `'x' IS NULL` is 0 and `'x' IS NOT NULL` is 1 (a literal
/// is never NULL), and `0 AND x` is 0 unless a function is involved. That
/// makes `ORDER BY (0 AND a)` an error, as in SQLite.
fn folded_integer(expr: &ast::Expr) -> Option<i64> {
    match expr {
        // Only integers that fit in 32 bits are positions.
        ast::Expr::Value(v) => match literal(&v.value).ok()? {
            Value::Integer(i) if i32::try_from(i).is_ok() => Some(i),
            _ => None,
        },
        ast::Expr::Nested(inner) => folded_integer(inner),
        ast::Expr::UnaryOp {
            op: ast::UnaryOperator::Plus,
            expr,
        } => folded_integer(expr),
        ast::Expr::UnaryOp {
            op: ast::UnaryOperator::Minus,
            expr,
        } => folded_integer(expr)?.checked_neg(),
        ast::Expr::IsNull(inner) if never_null(inner) => Some(0),
        ast::Expr::IsNotNull(inner) if never_null(inner) => Some(1),
        ast::Expr::BinaryOp {
            left,
            op: ast::BinaryOperator::And,
            right,
        } if (always_false(left) || always_false(right))
            && !has_function(left)
            && !has_function(right) =>
        {
            Some(0)
        }
        _ => None,
    }
}

/// The integer 0, as written or as simplified by SQLite's parser. Spellings
/// like `-0`, `0.0` and FALSE do not count.
fn always_false(expr: &ast::Expr) -> bool {
    match expr {
        ast::Expr::Nested(inner) => always_false(inner),
        ast::Expr::Value(_)
        | ast::Expr::IsNull(_)
        | ast::Expr::BinaryOp {
            op: ast::BinaryOperator::And,
            ..
        } => folded_integer(expr) == Some(0),
        _ => false,
    }
}

/// A literal number or string, or something the parser folds into an integer.
fn never_null(expr: &ast::Expr) -> bool {
    match expr {
        ast::Expr::Nested(inner)
        | ast::Expr::UnaryOp {
            op: ast::UnaryOperator::Plus | ast::UnaryOperator::Minus,
            expr: inner,
        } => never_null(inner),
        ast::Expr::Value(v) => matches!(
            v.value,
            ast::Value::Number(..) | ast::Value::SingleQuotedString(_)
        ),
        ast::Expr::IsNull(_) | ast::Expr::IsNotNull(_) | ast::Expr::BinaryOp { .. } => {
            folded_integer(expr).is_some()
        }
        _ => false,
    }
}

/// Whether an expression calls a function anywhere inside it. SQLite's
/// parser leaves such expressions alone rather than simplifying them.
fn has_function(expr: &ast::Expr) -> bool {
    use ast::Expr as E;
    match expr {
        E::Function(_) | E::Substring { .. } | E::Trim { .. } => true,
        E::Nested(e)
        | E::UnaryOp { expr: e, .. }
        | E::IsNull(e)
        | E::IsNotNull(e)
        | E::IsTrue(e)
        | E::IsNotTrue(e)
        | E::IsFalse(e)
        | E::IsNotFalse(e)
        | E::Cast { expr: e, .. } => has_function(e),
        E::BinaryOp { left, right, .. }
        | E::IsDistinctFrom(left, right)
        | E::IsNotDistinctFrom(left, right) => has_function(left) || has_function(right),
        E::Between {
            expr, low, high, ..
        } => has_function(expr) || has_function(low) || has_function(high),
        E::InList { expr, list, .. } => has_function(expr) || list.iter().any(has_function),
        E::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            operand.as_deref().is_some_and(has_function)
                || conditions
                    .iter()
                    .any(|w| has_function(&w.condition) || has_function(&w.result))
                || else_result.as_deref().is_some_and(has_function)
        }
        _ => false,
    }
}

/// "1st", "2nd", "3rd", "4th", ... "11th", "21st".
fn ordinal(n: usize) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

fn binary_op(op: &ast::BinaryOperator) -> Result<BinaryOp> {
    use ast::BinaryOperator as B;
    Ok(match op {
        B::Plus => BinaryOp::Add,
        B::Minus => BinaryOp::Subtract,
        B::Multiply => BinaryOp::Multiply,
        B::Divide => BinaryOp::Divide,
        B::Modulo => BinaryOp::Remainder,
        B::StringConcat => BinaryOp::Concat,
        B::Eq => BinaryOp::Eq,
        B::NotEq => BinaryOp::NotEq,
        B::Lt => BinaryOp::Lt,
        B::LtEq => BinaryOp::LtEq,
        B::Gt => BinaryOp::Gt,
        B::GtEq => BinaryOp::GtEq,
        B::And => BinaryOp::And,
        B::Or => BinaryOp::Or,
        other => return unsupported(format!("operator {other}")),
    })
}

fn literal(value: &ast::Value) -> Result<Value> {
    Ok(match value {
        ast::Value::Null => Value::Null,
        ast::Value::Boolean(b) => Value::Integer(i64::from(*b)),
        ast::Value::SingleQuotedString(s) => Value::Text(s.clone()),
        ast::Value::Number(text, _) => {
            let is_integer = text.bytes().all(|b| b.is_ascii_digit());
            match text.parse::<i64>() {
                Ok(i) if is_integer => Value::Integer(i),
                // Integers too large for 64 bits become reals, as in SQLite.
                _ => Value::Real(
                    text.parse()
                        .map_err(|_| Error::Parse(format!("bad number {text}")))?,
                ),
            }
        }
        other => return unsupported(format!("literal {other}")),
    })
}

fn object_name(name: &ast::ObjectName) -> Result<String> {
    match name.0.as_slice() {
        [ast::ObjectNamePart::Identifier(ident)] => Ok(ident.value.clone()),
        _ => unsupported(format!("qualified name {name}")),
    }
}

/// A short, stable label for an unsupported expression, such as "CASE" or
/// "function call". The sqllogictest report groups failures by this label.
fn expr_kind(expr: &ast::Expr) -> String {
    match expr {
        ast::Expr::Cast { .. } => "this kind of CAST".into(),
        ast::Expr::Substring { .. } | ast::Expr::Trim { .. } => {
            "SUBSTRING or TRIM with FROM".into()
        }
        ast::Expr::Like { .. } | ast::Expr::ILike { .. } => "LIKE".into(),
        ast::Expr::Collate { .. } => "COLLATE".into(),
        ast::Expr::IsTrue(_)
        | ast::Expr::IsFalse(_)
        | ast::Expr::IsNotTrue(_)
        | ast::Expr::IsNotFalse(_) => "IS TRUE / IS FALSE".into(),
        _ => "this kind of expression".into(),
    }
}

fn statement_kind(stmt: &ast::Statement) -> String {
    let text = stmt.to_string();
    let words: Vec<&str> = text.split_whitespace().take(2).collect();
    match words.as_slice() {
        [first, second] if ["CREATE", "DROP", "ALTER"].contains(first) => {
            format!("{first} {second}")
        }
        [first, ..] => first.to_string(),
        [] => "empty statement".into(),
    }
}
