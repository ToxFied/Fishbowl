//! Turns a parsed SQL statement into a [`Plan`].
//!
//! This is also where names are resolved ("binding"): `WHERE a > 1` becomes
//! "column 0 > 1", checked against the catalog. Anything Fishbowl does not
//! implement yet is rejected here with [`Error::Unsupported`], never ignored.

use sqlparser::ast;

use crate::catalog::{Catalog, Column, TableSchema};
use crate::error::{Error, Result, unsupported};
use crate::expr::{
    AggregateCall, AggregateFunc, BinaryOp, Expr, ScalarFunc, UnaryOp, is_sqlite_function,
};
use crate::plan::{Plan, PlanNode, QueryPlan, SortKey};
use crate::value::{Affinity, Value};

pub fn plan_statement(catalog: &Catalog, stmt: &ast::Statement) -> Result<Plan> {
    match stmt {
        ast::Statement::CreateTable(create) => plan_create_table(create),
        ast::Statement::Insert(insert) => plan_insert(catalog, insert),
        ast::Statement::Query(query) => plan_query(catalog, query).map(Plan::Query),
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
    let table = match catalog.table(&name) {
        Ok(schema) => schema.name.clone(),
        Err(_) if if_exists => name,
        Err(e) => return Err(e),
    };
    Ok(Plan::DropTable { table, if_exists })
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
    let schema = catalog.table(&object_name(name)?)?;
    let scope = Scope {
        table: Some(schema),
        alias: alias.as_ref().map(|a| a.name.value.clone()),
    };
    Ok((schema, scope))
}

fn plan_insert(catalog: &Catalog, insert: &ast::Insert) -> Result<Plan> {
    if insert.or.is_some() || insert.on.is_some() || insert.returning.is_some() {
        return unsupported("INSERT OR / ON CONFLICT / RETURNING");
    }
    let ast::TableObject::TableName(table_name) = &insert.table else {
        return unsupported("INSERT INTO a table function");
    };
    let schema = catalog.table(&object_name(table_name)?)?;
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
    let scope = Scope::empty();
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

fn plan_query(catalog: &Catalog, query: &ast::Query) -> Result<QueryPlan> {
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
    let select = match query.body.as_ref() {
        ast::SetExpr::Select(select) => select,
        ast::SetExpr::SetOperation { .. } => return unsupported("compound SELECT (UNION etc.)"),
        ast::SetExpr::Query(_) => return unsupported("parenthesized SELECT"),
        ast::SetExpr::Values(_) => return unsupported("VALUES as a query"),
        _ => return unsupported("this kind of query"),
    };
    let distinct = select_is_distinct(select)?;
    let group_by_terms = match &select.group_by {
        ast::GroupByExpr::Expressions(terms, modifiers) if modifiers.is_empty() => terms,
        _ => return unsupported("GROUP BY ALL or GROUP BY modifiers"),
    };

    let (scope, mut root) = plan_from(catalog, &select.from)?;
    let items = select_items(&scope, &select.projection)?;
    let mut binder = Binder {
        scope: &scope,
        items: &items,
        aggregates: Vec::new(),
    };

    if let Some(predicate) = &select.selection {
        root = PlanNode::Filter {
            input: Box::new(root),
            predicate: binder.bind(predicate, Ctx::ROW)?,
        };
    }

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
    let sort_keys = match &query.order_by {
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
    root = PlanNode::Project {
        input: Box::new(root),
        exprs,
    };
    if distinct {
        root = PlanNode::Distinct {
            input: Box::new(root),
        };
    }
    if let Some((limit, offset)) = limit_clause(query)? {
        // LIMIT and OFFSET are constants: no table is in scope.
        let scope = Scope::empty();
        let mut constants = Binder::without_items(&scope);
        root = PlanNode::Limit {
            input: Box::new(root),
            limit: constants.bind(limit, Ctx::CONSTANT)?,
            offset: match offset {
                Some(offset) => Some(constants.bind(offset, Ctx::CONSTANT)?),
                None => None,
            },
        };
    }

    Ok(QueryPlan {
        columns: items.into_iter().map(|item| item.name).collect(),
        root,
    })
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

fn plan_from<'c>(
    catalog: &'c Catalog,
    from: &[ast::TableWithJoins],
) -> Result<(Scope<'c>, PlanNode)> {
    match from {
        [] => Ok((Scope::empty(), PlanNode::OneRow)),
        [from] => {
            if !from.joins.is_empty() {
                return unsupported("JOIN");
            }
            let ast::TableFactor::Table {
                name,
                alias,
                args: None,
                ..
            } = &from.relation
            else {
                return unsupported("subquery or table function in FROM");
            };
            let schema = catalog.table(&object_name(name)?)?;
            let scope = Scope {
                table: Some(schema),
                alias: alias.as_ref().map(|a| a.name.value.clone()),
            };
            let scan = PlanNode::SeqScan {
                table: schema.name.clone(),
                reason: format!("{} has no index, so every row is read", schema.name),
            };
            Ok((scope, scan))
        }
        _ => unsupported("JOIN (more than one table in FROM)"),
    }
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
    Column(usize),
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

/// The names visible to an expression: at most one table for now.
struct Scope<'a> {
    table: Option<&'a TableSchema>,
    alias: Option<String>,
}

impl<'a> Scope<'a> {
    fn empty() -> Self {
        Scope {
            table: None,
            alias: None,
        }
    }

    /// Columns in one input row.
    fn width(&self) -> usize {
        self.table.map_or(0, |t| t.columns.len())
    }

    fn matches_qualifier(&self, qualifier: &str) -> bool {
        match (self.table, &self.alias) {
            (_, Some(alias)) => alias.eq_ignore_ascii_case(qualifier),
            (Some(table), None) => table.name.eq_ignore_ascii_case(qualifier),
            (None, None) => false,
        }
    }

    fn column_expr(table: &TableSchema, index: usize) -> Expr {
        let column = &table.columns[index];
        Expr::Column {
            index,
            name: column.name.clone(),
            affinity: column.affinity,
        }
    }

    fn column(&self, qualifier: Option<&str>, name: &str) -> Result<Expr> {
        let found = self
            .table
            .filter(|_| qualifier.is_none_or(|q| self.matches_qualifier(q)));
        match found.and_then(|t| Some((t, t.column_index(name)?))) {
            Some((table, index)) => Ok(Self::column_expr(table, index)),
            None => {
                let full = qualifier.map_or(name.to_string(), |q| format!("{q}.{name}"));
                Err(Error::Catalog(format!("no such column: {full}")))
            }
        }
    }

    fn expand_wildcard(&self, qualifier: Option<&str>, items: &mut Vec<Item>) -> Result<()> {
        let table = match (self.table, qualifier) {
            (Some(t), None) => t,
            (Some(t), Some(q)) if self.matches_qualifier(q) => t,
            (_, Some(q)) => return Err(Error::Catalog(format!("no such table: {q}"))),
            (None, None) => return Err(Error::Catalog("no tables specified".into())),
        };
        for (index, column) in table.columns.iter().enumerate() {
            items.push(Item {
                name: column.name.clone(),
                is_alias: false,
                source: Source::Column(index),
            });
        }
        Ok(())
    }
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
}

impl<'a> Binder<'a> {
    /// A binder with no result columns: INSERT values, LIMIT, and the
    /// expressions of UPDATE and DELETE.
    fn without_items(scope: &'a Scope<'a>) -> Self {
        Binder {
            scope,
            items: &[],
            aggregates: Vec::new(),
        }
    }

    fn bind_item(&mut self, item: &Item, ctx: Ctx) -> Result<Expr> {
        match item.source {
            Source::Expr(e) => self.bind(e, ctx),
            Source::Column(index) => {
                Ok(self
                    .scope
                    .column(None, &item.name)
                    .unwrap_or_else(|_| Expr::Column {
                        index,
                        name: item.name.clone(),
                        affinity: Affinity::Blob,
                    }))
            }
        }
    }

    fn sort_keys(&mut self, order_by: &ast::OrderBy, ctx: Ctx) -> Result<Vec<SortKey>> {
        if order_by.interpolate.is_some() {
            return unsupported("ORDER BY ... INTERPOLATE");
        }
        let ast::OrderByKind::Expressions(terms) = &order_by.kind else {
            return unsupported("ORDER BY ALL");
        };
        let mut keys = Vec::new();
        for (i, term) in terms.iter().enumerate() {
            if term.with_fill.is_some() {
                return unsupported("ORDER BY ... WITH FILL");
            }
            let descending = match &term.options.sort {
                None | Some(ast::OrderBySort::Asc) => false,
                Some(ast::OrderBySort::Desc) => true,
                Some(ast::OrderBySort::Using(_)) => return unsupported("ORDER BY ... USING"),
            };
            keys.push(SortKey {
                expr: self.result_term(&term.expr, i, Clause::OrderBy, ctx)?,
                descending,
                // NULL is the smallest value: first ascending, last descending.
                nulls_first: term.options.nulls_first.unwrap_or(!descending),
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

    fn identifier(&mut self, qualifier: Option<&str>, name: &str, ctx: Ctx) -> Result<Expr> {
        let column = self.scope.column(qualifier, name);
        if column.is_err()
            && qualifier.is_none()
            && ctx.aliases
            && let Some(item) = find_alias(self.items, name)
        {
            // The alias's own expression cannot refer to aliases, which rules
            // out cycles like `SELECT b AS a, a AS b`.
            return self.bind_item(item, ctx.without_aliases());
        }
        column
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
        ast::Expr::InSubquery { .. } => "IN (subquery)".into(),
        ast::Expr::Subquery(_) => "scalar subquery".into(),
        ast::Expr::Exists { .. } => "EXISTS".into(),
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
