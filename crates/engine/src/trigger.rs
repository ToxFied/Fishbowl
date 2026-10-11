//! Triggers: statements that run for each row an INSERT, UPDATE or DELETE
//! changes, as in SQLite's `CREATE TRIGGER`.
//!
//! A trigger keeps its WHEN clause and body as SQL text. Each time it fires,
//! every `NEW.column` and `OLD.column` in that text becomes the row's value,
//! written as a literal, and the statement is parsed and planned afresh. That
//! matches SQLite, where these references carry no column affinity: with an
//! INTEGER column `x`, `NEW.x = '5'` is false even when `x` is 5.
//!
//! Everything a statement and its triggers write goes into one undo log, so
//! a failure anywhere, including `RAISE(ABORT, ...)`, undoes all of it. A
//! trigger does not fire while it is already running, which is SQLite's
//! default (`PRAGMA recursive_triggers` off).
//!
//! `INSTEAD OF` triggers make a view writable: the statement changes no
//! rows itself, and the triggers fire once for each row of the view it
//! names.

use sqlparser::ast;
use sqlparser::keywords::Keyword;
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::{Token, Tokenizer};

use crate::catalog::{Catalog, Column, TableSchema};
use crate::dialect::FishbowlDialect;
use crate::error::{Error, Result, unsupported};
use crate::exec::{self, ExecContext};
use crate::plan::Plan;
use crate::storage::Row;
use crate::value::Value;
use crate::write::{self, OnConflict, TableWriter, UndoLog};
use crate::{Database, StatementResult, parse, planner};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timing {
    Before,
    After,
    InsteadOf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Insert,
    Update,
    Delete,
}

#[derive(Debug, Clone)]
pub struct Trigger {
    pub name: String,
    /// The table or view it is on.
    pub table: String,
    pub timing: Timing,
    pub event: Event,
    /// `UPDATE OF a, b`: fire only for an UPDATE that sets one of these
    /// columns. Empty means any UPDATE.
    pub columns: Vec<String>,
    /// The WHEN condition, as SQL.
    pub when: Option<String>,
    /// The body's statements, as SQL.
    pub body: Vec<String>,
}

impl Trigger {
    /// Whether an UPDATE that sets `columns` fires it. `UPDATE OF` needs
    /// one of its columns among them; other triggers always fire.
    fn fires_for(&self, schema: &TableSchema, columns: &[usize]) -> bool {
        self.columns.is_empty()
            || self.columns.iter().any(|name| {
                schema
                    .column_index(name)
                    .is_some_and(|c| columns.contains(&c))
            })
    }
}

/// Parses `CREATE [TEMP] TRIGGER IF NOT EXISTS`, which the parser does not
/// know. The result has `or_alter` set to stand for IF NOT EXISTS; SQLite
/// has no `OR ALTER`, so a statement that spells it is refused here.
pub(crate) fn parse_create(parser: &mut Parser) -> Option<Result<ast::Statement, ParserError>> {
    let is = |n: usize, keyword: Keyword| match parser.peek_nth_token(n).token {
        Token::Word(w) => w.keyword == keyword,
        _ => false,
    };
    if !is(0, Keyword::CREATE) {
        return None;
    }
    if is(1, Keyword::OR) && is(2, Keyword::ALTER) {
        return Some(Err(ParserError::ParserError(
            "near \"ALTER\": syntax error".into(),
        )));
    }
    let temporary = is(1, Keyword::TEMP) || is(1, Keyword::TEMPORARY);
    let at = usize::from(temporary) + 1;
    let wanted = [Keyword::TRIGGER, Keyword::IF, Keyword::NOT, Keyword::EXISTS];
    if !wanted.iter().enumerate().all(|(i, &k)| is(at + i, k)) {
        return None;
    }
    for _ in 0..at + wanted.len() {
        parser.next_token();
    }
    Some(
        parser
            .parse_create_trigger(temporary, true, false, false)
            .map(ast::Statement::CreateTrigger),
    )
}

/// Checks a `CREATE TRIGGER` against the catalog and SQLite's grammar.
pub(crate) fn plan_create(catalog: &Catalog, create: &ast::CreateTrigger) -> Result<Trigger> {
    let syntax = |near: &str| Err(Error::Parse(format!("near \"{near}\": syntax error")));
    if create.or_replace {
        return syntax("REPLACE");
    }
    if create.is_constraint
        || create.referenced_table_name.is_some()
        || !create.referencing.is_empty()
        || create.characteristics.is_some()
        || create.exec_body.is_some()
        || create.statements_as
        || !create.period_before_table
    {
        return unsupported("this CREATE TRIGGER option");
    }
    match create.trigger_object {
        None | Some(ast::TriggerObjectKind::ForEach(ast::TriggerObject::Row)) => {}
        Some(ast::TriggerObjectKind::ForEach(ast::TriggerObject::Statement)) => {
            return syntax("STATEMENT");
        }
        Some(ast::TriggerObjectKind::For(_)) => return syntax("ROW"),
    }
    let timing = match create.period {
        None | Some(ast::TriggerPeriod::Before) => Timing::Before,
        Some(ast::TriggerPeriod::After) => Timing::After,
        Some(ast::TriggerPeriod::InsteadOf) => Timing::InsteadOf,
        Some(ast::TriggerPeriod::For) => return syntax("FOR"),
    };
    let (event, columns) = match create.events.as_slice() {
        [ast::TriggerEvent::Insert] => (Event::Insert, Vec::new()),
        [ast::TriggerEvent::Delete] => (Event::Delete, Vec::new()),
        [ast::TriggerEvent::Update(columns)] => (
            Event::Update,
            columns.iter().map(|c| c.value.clone()).collect(),
        ),
        [ast::TriggerEvent::Truncate] => return syntax("TRUNCATE"),
        _ => return syntax("OR"),
    };
    let Some(ast::ConditionalStatements::BeginEnd(block)) = &create.statements else {
        return syntax("END");
    };
    if block.statements.is_empty() {
        return syntax("END");
    }
    let mut body = Vec::new();
    for stmt in &block.statements {
        match stmt {
            ast::Statement::Insert(_)
            | ast::Statement::Update(_)
            | ast::Statement::Delete(_)
            | ast::Statement::Query(_) => body.push(stmt.to_string()),
            other => {
                let text = other.to_string();
                return syntax(text.split_whitespace().next().unwrap_or_default());
            }
        }
    }

    let name = single_name(&create.name)?;
    let table = single_name(&create.table_name)?;
    let table = if let Ok(schema) = catalog.table(&table) {
        if timing == Timing::InsteadOf {
            return Err(Error::Catalog(format!(
                "cannot create INSTEAD OF trigger on table: {table}"
            )));
        }
        schema.name.clone()
    } else if let Some(view) = catalog.view(&table) {
        if timing != Timing::InsteadOf {
            let when = if timing == Timing::Before {
                "BEFORE"
            } else {
                "AFTER"
            };
            return Err(Error::Catalog(format!(
                "cannot create {when} trigger on view: {table}"
            )));
        }
        view.name.clone()
    } else {
        return Err(Error::Catalog(format!("no such table: main.{table}")));
    };
    Ok(Trigger {
        name,
        table,
        timing,
        event,
        columns,
        when: create.condition.as_ref().map(ToString::to_string),
        body,
    })
}

fn single_name(name: &ast::ObjectName) -> Result<String> {
    match name.0.as_slice() {
        [ast::ObjectNamePart::Identifier(ident)] => Ok(ident.value.clone()),
        _ => unsupported(format!("qualified name {name}")),
    }
}

/// What the rest of a row's work does after its triggers ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Go,
    /// `RAISE(IGNORE)`: skip the row and the triggers still to fire for it.
    Ignore,
}

/// The rows an INSERT, UPDATE or DELETE changes, found before any is
/// written.
pub(crate) enum Change {
    Insert {
        /// For each value in a row, the column it fills.
        targets: Vec<usize>,
        rows: Vec<Vec<Value>>,
        on_conflict: OnConflict,
    },
    Update {
        assignments: Vec<(usize, crate::expr::Expr)>,
        rows: Vec<(i64, Row)>,
    },
    Delete {
        rows: Vec<(i64, Row)>,
    },
}

/// The state of one statement that fires triggers.
struct Firing {
    /// The triggers running now. One of them does not fire again.
    active: Vec<String>,
    /// Every change the statement and its triggers made, by table, oldest
    /// first.
    log: Vec<(String, UndoLog)>,
    /// The statement's `OR` clause, which overrides those in its triggers.
    on_conflict: Option<OnConflict>,
    /// The `RAISE()` that failed the statement. Its own rule, not the
    /// statement's `OR` clause, decides what is undone.
    raised: Option<Raise>,
}

impl Firing {
    fn new(on_conflict: Option<OnConflict>) -> Firing {
        Firing {
            active: Vec::new(),
            log: Vec::new(),
            on_conflict,
            raised: None,
        }
    }
}

/// The rows a firing trigger can name: `OLD` and `NEW`, with their rowids.
#[derive(Clone, Copy)]
struct Rows<'r> {
    schema: &'r TableSchema,
    old: Option<(i64, &'r [Value])>,
    new: Option<(i64, &'r [Value])>,
}

impl Database {
    /// Runs `CREATE TRIGGER` or `DROP TRIGGER`. Anything else is `None`.
    pub(crate) fn trigger_statement(
        &mut self,
        stmt: &ast::Statement,
    ) -> Option<Result<StatementResult>> {
        match stmt {
            ast::Statement::CreateTrigger(create) => Some(self.create_trigger(create)),
            ast::Statement::DropTrigger(drop) => Some(self.drop_trigger(drop)),
            _ => None,
        }
    }

    fn create_trigger(&mut self, create: &ast::CreateTrigger) -> Result<StatementResult> {
        let trigger = plan_create(&self.catalog, create)?;
        let name = trigger.name.clone();
        // `or_alter` stands for IF NOT EXISTS; see `parse_create`.
        if create.or_alter && self.catalog.trigger(&name).is_some() {
            return Ok(StatementResult::CreateTrigger { trigger: name });
        }
        let table = trigger.table.clone();
        self.catalog.add_trigger(trigger)?;
        if !create.temporary {
            let sql = ast::CreateTrigger {
                or_alter: false,
                ..create.clone()
            };
            let stored = self.storage.create_trigger(
                &name,
                &table,
                &ast::Statement::CreateTrigger(sql).to_string(),
            );
            if let Err(e) = stored {
                self.catalog.remove_trigger(&name);
                return Err(e);
            }
        }
        Ok(StatementResult::CreateTrigger { trigger: name })
    }

    fn drop_trigger(&mut self, drop: &ast::DropTrigger) -> Result<StatementResult> {
        if drop.table_name.is_some() || drop.option.is_some() {
            return unsupported("this DROP TRIGGER option");
        }
        let name = single_name(&drop.trigger_name)?;
        match self.catalog.remove_trigger(&name) {
            Some(trigger) => self.storage.drop_trigger(&trigger.name)?,
            None if drop.if_exists => {}
            None => return Err(Error::Catalog(format!("no such trigger: {name}"))),
        }
        Ok(StatementResult::DropTrigger { trigger: name })
    }

    /// Forgets the stored triggers on a table or view that is being dropped.
    /// The catalog drops them with the table.
    pub(crate) fn drop_triggers_on(&mut self, table: &str) -> Result<()> {
        let names: Vec<String> = self
            .catalog
            .triggers_on(table)
            .map(|t| t.name.clone())
            .collect();
        names
            .iter()
            .try_for_each(|name| self.storage.drop_trigger(name))
    }

    /// Writes a statement's rows to a table that has triggers, firing them.
    /// Returns how many rows the statement itself changed. If anything
    /// fails, every change is undone, unless `OR FAIL` or `RAISE(FAIL)`
    /// keeps the ones made so far.
    pub(crate) fn write_with_triggers(&mut self, table: &str, change: Change) -> Result<u64> {
        let on_conflict = match &change {
            Change::Insert { on_conflict, .. } => *on_conflict,
            _ => OnConflict::Abort,
        };
        let mut firing = Firing::new((on_conflict != OnConflict::Abort).then_some(on_conflict));
        let result = self.apply(&mut firing, table, change);
        let fail = on_conflict == OnConflict::Fail;
        self.settle(firing, fail, result)
    }

    /// Ends a statement that fired triggers: if it failed, undoes its
    /// changes, unless `OR FAIL` (`fail`) or `RAISE(FAIL)` keeps them.
    fn settle<T>(&mut self, firing: Firing, fail: bool, result: Result<T>) -> Result<T> {
        let Err(e) = &result else { return result };
        let keep = match &firing.raised {
            Some(raise) => {
                self.end_transaction = Some(matches!(raise, Raise::Rollback(_)));
                matches!(raise, Raise::Fail(_))
            }
            None => fail && matches!(e, Error::Constraint(_)),
        };
        if !keep {
            for (table, undo) in firing.log.into_iter().rev() {
                let schema = self.catalog.table(&table)?;
                TableWriter::new(schema, &mut self.storage, OnConflict::Abort).revert(undo)?;
            }
        }
        result
    }

    /// Writes rows one at a time, each between its BEFORE and AFTER
    /// triggers.
    fn apply(&mut self, firing: &mut Firing, table: &str, change: Change) -> Result<u64> {
        let schema = self.catalog.table(table)?.clone();
        let mut count = 0;
        match change {
            Change::Insert {
                targets,
                rows,
                on_conflict,
            } => {
                let on_conflict = firing.on_conflict.unwrap_or(on_conflict);
                for values in rows {
                    let mut row = vec![Value::Null; schema.columns.len()];
                    for (&target, value) in targets.iter().zip(values) {
                        row[target] = value.apply_affinity(schema.columns[target].affinity);
                    }
                    // Before the row is stored, an unassigned rowid reads as
                    // -1, and so does an INTEGER PRIMARY KEY left NULL. One
                    // that is set must be an integer before triggers run.
                    let mut early = row.clone();
                    let rowid = match schema.rowid_alias {
                        Some(pk) if row[pk].is_null() => {
                            early[pk] = Value::Integer(-1);
                            -1
                        }
                        Some(pk) => write::rowid_value(&row[pk])?,
                        None => -1,
                    };
                    let before = Rows {
                        schema: &schema,
                        old: None,
                        new: Some((rowid, &early)),
                    };
                    if self.fire(firing, Timing::Before, Event::Insert, before, &[])?
                        == Flow::Ignore
                    {
                        continue;
                    }
                    let mut writer = TableWriter::new(&schema, &mut self.storage, on_conflict);
                    let stored = writer.insert(row.clone());
                    firing.log.push((schema.name.clone(), writer.into_undo()));
                    let Some(rowid) = stored? else { continue };
                    if let Some(pk) = schema.rowid_alias {
                        row[pk] = Value::Integer(rowid);
                    }
                    count += 1;
                    let after = Rows {
                        schema: &schema,
                        old: None,
                        new: Some((rowid, &row)),
                    };
                    self.fire(firing, Timing::After, Event::Insert, after, &[])?;
                }
            }
            Change::Update {
                assignments,
                mut rows,
            } => {
                let columns: Vec<usize> = assignments.iter().map(|(c, _)| *c).collect();
                self.visit_in_rowid_order(&schema, Event::Update, &columns, &mut rows);
                for (rowid, _) in rows {
                    // An earlier row's triggers may have changed or deleted it.
                    let Some(old) = self.storage.get(&schema.name, rowid)? else {
                        continue;
                    };
                    let mut new = old.clone();
                    for (index, expr) in &assignments {
                        new[*index] = expr
                            .eval(&old)?
                            .apply_affinity(schema.columns[*index].affinity);
                    }
                    let new_rowid = match schema.rowid_alias.map(|pk| &new[pk]) {
                        Some(Value::Integer(id)) => *id,
                        _ => rowid,
                    };
                    let rows = Rows {
                        schema: &schema,
                        old: Some((rowid, &old)),
                        new: Some((new_rowid, &new)),
                    };
                    if self.fire(firing, Timing::Before, Event::Update, rows, &columns)?
                        == Flow::Ignore
                    {
                        continue;
                    }
                    // Like SQLite, take the columns the UPDATE does not set
                    // from the row as the BEFORE triggers left it.
                    let Some(old) = self.storage.get(&schema.name, rowid)? else {
                        continue;
                    };
                    let mut changed = old.clone();
                    for &c in &columns {
                        changed[c] = new[c].clone();
                    }
                    let mut writer =
                        TableWriter::new(&schema, &mut self.storage, OnConflict::Abort);
                    let written = writer.update(rowid, old.clone(), changed.clone());
                    firing.log.push((schema.name.clone(), writer.into_undo()));
                    written?;
                    count += 1;
                    let rows = Rows {
                        schema: &schema,
                        old: Some((rowid, &old)),
                        new: Some((new_rowid, &changed)),
                    };
                    self.fire(firing, Timing::After, Event::Update, rows, &columns)?;
                }
            }
            Change::Delete { mut rows } => {
                self.visit_in_rowid_order(&schema, Event::Delete, &[], &mut rows);
                for (rowid, _) in rows {
                    let Some(old) = self.storage.get(&schema.name, rowid)? else {
                        continue;
                    };
                    let rows = Rows {
                        schema: &schema,
                        old: Some((rowid, &old)),
                        new: None,
                    };
                    if self.fire(firing, Timing::Before, Event::Delete, rows, &[])? == Flow::Ignore
                    {
                        continue;
                    }
                    // A BEFORE trigger may have deleted the row already.
                    let Some(old) = self.storage.get(&schema.name, rowid)? else {
                        continue;
                    };
                    let mut writer =
                        TableWriter::new(&schema, &mut self.storage, OnConflict::Abort);
                    let deleted = writer.delete(rowid, old.clone());
                    firing.log.push((schema.name.clone(), writer.into_undo()));
                    deleted?;
                    count += 1;
                    let rows = Rows {
                        old: Some((rowid, &old)),
                        ..rows
                    };
                    self.fire(firing, Timing::After, Event::Delete, rows, &[])?;
                }
            }
        }
        Ok(count)
    }

    /// When an UPDATE or DELETE fires triggers, SQLite collects the rowids
    /// it matched and visits them in order, rather than in the order its
    /// scan found them. That order shows in the triggers' effects.
    fn visit_in_rowid_order(
        &self,
        schema: &TableSchema,
        event: Event,
        columns: &[usize],
        rows: &mut [(i64, Row)],
    ) {
        if self
            .catalog
            .triggers_on(&schema.name)
            .any(|t| t.event == event && t.fires_for(schema, columns))
        {
            rows.sort_by_key(|(rowid, _)| *rowid);
        }
    }

    /// Runs the triggers for one row and one moment, newest first.
    /// `columns` are the columns an UPDATE sets.
    fn fire(
        &mut self,
        firing: &mut Firing,
        timing: Timing,
        event: Event,
        rows: Rows,
        columns: &[usize],
    ) -> Result<Flow> {
        let schema = rows.schema;
        let triggers: Vec<Trigger> = self
            .catalog
            .triggers_on(&schema.name)
            .filter(|t| t.timing == timing && t.event == event)
            .filter(|t| {
                !firing
                    .active
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case(&t.name))
            })
            .filter(|t| t.fires_for(schema, columns))
            .cloned()
            .collect();
        for trigger in triggers {
            if let Some(when) = &trigger.when {
                let query = query(&format!("SELECT {}", substitute(when, rows)?))?;
                let row = self.select(query)?.into_iter().next();
                let holds = row.and_then(|r| r.into_iter().next());
                if holds.and_then(|v| v.truthiness()) != Some(true) {
                    continue;
                }
            }
            firing.active.push(trigger.name.clone());
            let mut flow = Ok(Flow::Go);
            for sql in &trigger.body {
                flow = substitute(sql, rows).and_then(|sql| self.run_body(firing, &sql));
                if flow != Ok(Flow::Go) {
                    break;
                }
            }
            firing.active.pop();
            if flow? == Flow::Ignore {
                return Ok(Flow::Ignore);
            }
        }
        Ok(Flow::Go)
    }

    /// Runs one statement of a trigger's body, its row values already in
    /// place.
    fn run_body(&mut self, firing: &mut Firing, sql: &str) -> Result<Flow> {
        let [stmt] = parse(sql)?
            .try_into()
            .map_err(|_| Error::Parse(format!("expected one statement in a trigger: {sql}")))?;
        if let ast::Statement::Query(mut query) = stmt {
            let raises = take_raises(&mut query)?;
            let rows = self.select(query)?;
            let raised = rows.iter().flatten().find_map(|value| match value {
                Value::Text(text) => raise_index(text).and_then(|i| raises.get(i)),
                _ => None,
            });
            return match raised {
                None => Ok(Flow::Go),
                Some(Raise::Ignore) => Ok(Flow::Ignore),
                Some(
                    raise @ (Raise::Abort(message)
                    | Raise::Fail(message)
                    | Raise::Rollback(message)),
                ) => {
                    let error = Error::Constraint(message.clone());
                    firing.raised = Some(raise.clone());
                    Err(error)
                }
            };
        }
        if has_raise(sql)? {
            return unsupported("RAISE() outside a trigger's SELECT");
        }
        let rollback = matches!(
            &stmt,
            ast::Statement::Insert(insert) if insert.or == Some(ast::SqliteOnConflict::Rollback)
        );
        let (table, change) = match planner::plan_statement(&self.catalog, &stmt)? {
            plan @ (Plan::Insert { .. }
            | Plan::InsertSelect { .. }
            | Plan::Update { .. }
            | Plan::Delete { .. }) => self.change_for(plan)?,
            _ => return unsupported("this statement in a trigger"),
        };
        let result = self.apply(firing, &table, change);
        if rollback && matches!(result, Err(Error::Constraint(_))) {
            self.end_transaction = Some(true);
        }
        result.map(|_| Flow::Go)
    }

    /// Runs a query and returns its rows.
    fn select(&self, query: Box<ast::Query>) -> Result<Vec<Row>> {
        let Plan::Query(plan) =
            planner::plan_statement(&self.catalog, &ast::Statement::Query(query))?
        else {
            return Err(Error::Parse("expected a query".into()));
        };
        let mut ctx = ExecContext::new(&self.storage, self.row_budget);
        Ok(exec::run_query(&mut ctx, &plan.root)?.0)
    }

    /// Runs an INSERT, UPDATE or DELETE on a view through its `INSTEAD OF`
    /// triggers. `None` if the statement is not one, or the view has no
    /// such trigger, so the statement takes its usual path.
    pub(crate) fn instead_of(&mut self, stmt: &ast::Statement) -> Option<Result<StatementResult>> {
        let (view, event) = match stmt {
            ast::Statement::Insert(insert) => match &insert.table {
                ast::TableObject::TableName(name) => (name, Event::Insert),
                _ => return None,
            },
            ast::Statement::Update(update) => match &update.table.relation {
                ast::TableFactor::Table { name, .. } => (name, Event::Update),
                _ => return None,
            },
            ast::Statement::Delete(delete) => {
                let (ast::FromTable::WithFromKeyword(from) | ast::FromTable::WithoutKeyword(from)) =
                    &delete.from;
                match from.as_slice() {
                    [
                        ast::TableWithJoins {
                            relation: ast::TableFactor::Table { name, .. },
                            ..
                        },
                    ] => (name, Event::Delete),
                    _ => return None,
                }
            }
            _ => return None,
        };
        let view = single_name(view).ok()?;
        let view = self.catalog.view(&view)?.name.clone();
        let fires = self
            .catalog
            .triggers_on(&view)
            .any(|t| t.timing == Timing::InsteadOf && t.event == event);
        fires.then(|| self.write_view(stmt, &view, event))
    }

    fn write_view(
        &mut self,
        stmt: &ast::Statement,
        view: &str,
        event: Event,
    ) -> Result<StatementResult> {
        // The view's columns, named and typed like a table's, so `NEW` and
        // `OLD` can name them.
        let all = query(&format!("SELECT * FROM {}", quoted(view)))?;
        let Plan::Query(plan) =
            planner::plan_statement(&self.catalog, &ast::Statement::Query(all))?
        else {
            return Err(Error::Parse("expected a query".into()));
        };
        let schema = TableSchema {
            name: view.to_string(),
            columns: plan
                .columns
                .iter()
                .map(|name| Column {
                    name: name.clone(),
                    declared_type: String::new(),
                    affinity: crate::value::Affinity::Blob,
                    not_null: false,
                    primary_key: false,
                })
                .collect(),
            rowid_alias: None,
            indexes: Vec::new(),
        };
        // Each row: (OLD, NEW).
        let mut changes: Vec<(Option<Row>, Option<Row>)> = Vec::new();
        let mut columns = Vec::new();
        match stmt {
            ast::Statement::Insert(insert) => {
                if insert.or.is_some() || insert.on.is_some() || insert.returning.is_some() {
                    return unsupported("INSERT OR / ON CONFLICT / RETURNING on a view");
                }
                let targets = if insert.columns.is_empty() {
                    (0..schema.columns.len()).collect()
                } else {
                    let mut targets = Vec::new();
                    for column in &insert.columns {
                        let name = single_name(column)?;
                        targets.push(schema.column_index(&name).ok_or_else(|| {
                            Error::Catalog(format!("table {view} has no column named {name}"))
                        })?);
                    }
                    targets
                };
                let Some(source) = &insert.source else {
                    return unsupported("INSERT without VALUES");
                };
                let rows = match source.body.as_ref() {
                    ast::SetExpr::Values(values) => {
                        let mut rows = Vec::new();
                        for row in &values.rows {
                            let q = query(&format!("SELECT {}", comma(&row.content)))?;
                            rows.extend(self.select(q)?);
                        }
                        rows
                    }
                    _ => self.select(source.clone())?,
                };
                for values in rows {
                    if values.len() != targets.len() {
                        return Err(Error::Catalog(format!(
                            "{} values for {} columns",
                            values.len(),
                            targets.len()
                        )));
                    }
                    let mut row = vec![Value::Null; schema.columns.len()];
                    for (&target, value) in targets.iter().zip(values) {
                        row[target] = value;
                    }
                    changes.push((None, Some(row)));
                }
            }
            ast::Statement::Update(update) => {
                if update.or.is_some()
                    || update.from.is_some()
                    || update.returning.is_some()
                    || !update.order_by.is_empty()
                    || update.limit.is_some()
                    || !update.table.joins.is_empty()
                {
                    return unsupported("UPDATE OR / FROM / RETURNING / LIMIT");
                }
                let mut exprs = Vec::new();
                for assignment in &update.assignments {
                    let ast::AssignmentTarget::ColumnName(column) = &assignment.target else {
                        return unsupported("UPDATE of a column tuple");
                    };
                    let name = single_name(column)?;
                    let index = schema
                        .column_index(&name)
                        .ok_or_else(|| Error::Catalog(format!("no such column: {name}")))?;
                    columns.push(index);
                    exprs.push(assignment.value.to_string());
                }
                let mut sql = format!("SELECT *, {} FROM {}", exprs.join(", "), update.table);
                if let Some(selection) = &update.selection {
                    sql += &format!(" WHERE {selection}");
                }
                for mut old in self.select(query(&sql)?)? {
                    let values = old.split_off(schema.columns.len());
                    let mut new = old.clone();
                    for (&c, value) in columns.iter().zip(values) {
                        new[c] = value;
                    }
                    changes.push((Some(old), Some(new)));
                }
            }
            ast::Statement::Delete(delete) => {
                if delete.using.is_some()
                    || delete.returning.is_some()
                    || !delete.order_by.is_empty()
                    || delete.limit.is_some()
                {
                    return unsupported("DELETE USING / RETURNING / LIMIT");
                }
                let (ast::FromTable::WithFromKeyword(from) | ast::FromTable::WithoutKeyword(from)) =
                    &delete.from;
                let mut sql = format!("SELECT * FROM {}", comma(from));
                if let Some(selection) = &delete.selection {
                    sql += &format!(" WHERE {selection}");
                }
                for old in self.select(query(&sql)?)? {
                    changes.push((Some(old), None));
                }
            }
            _ => return unsupported("this statement on a view"),
        }

        let mut firing = Firing::new(None);
        let mut result = Ok(());
        for (old, new) in &changes {
            // A view's rows have no rowid of their own.
            let rows = Rows {
                schema: &schema,
                old: old.as_deref().map(|r| (-1, r)),
                new: new.as_deref().map(|r| (-1, r)),
            };
            result = self
                .fire(&mut firing, Timing::InsteadOf, event, rows, &columns)
                .map(drop);
            if result.is_err() {
                break;
            }
        }
        self.settle(firing, false, result)?;
        // As in SQLite, rows changed through a view's triggers do not count.
        let table = view.to_string();
        Ok(match event {
            Event::Insert => StatementResult::Insert { table, rows: 0 },
            Event::Update => StatementResult::Update { table, rows: 0 },
            Event::Delete => StatementResult::Delete { table, rows: 0 },
        })
    }
}

/// Parses SQL this module wrote, which holds one query.
fn query(sql: &str) -> Result<Box<ast::Query>> {
    match parse(sql)?.pop() {
        Some(ast::Statement::Query(query)) => Ok(query),
        _ => Err(Error::Parse(format!("expected a query: {sql}"))),
    }
}

fn comma<T: std::fmt::Display>(items: &[T]) -> String {
    items
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Replaces each `NEW.column` and `OLD.column` in `sql` with the row's
/// value. `NEW.rowid` (or `oid`, `_rowid_`) is the row's rowid unless a
/// column has that name. A reference to a row the event lacks, such as
/// `OLD` in an INSERT trigger, or to a column the table lacks, is left for
/// the planner to report as "no such column".
fn substitute(sql: &str, rows: Rows) -> Result<String> {
    let tokens = Tokenizer::new(&FishbowlDialect::default(), sql)
        .tokenize()
        .map_err(|e| Error::Parse(e.to_string()))?;
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    while i < tokens.len() {
        if let (Some(Token::Word(row)), Some(Token::Period), Some(Token::Word(column))) =
            (tokens.get(i), tokens.get(i + 1), tokens.get(i + 2))
            && i.checked_sub(1).and_then(|p| tokens.get(p)) != Some(&Token::Period)
        {
            let found = if row.value.eq_ignore_ascii_case("new") {
                rows.new
            } else if row.value.eq_ignore_ascii_case("old") {
                rows.old
            } else {
                None
            };
            let value =
                found.and_then(
                    |(rowid, values)| match rows.schema.column_index(&column.value) {
                        Some(c) => Some(values[c].clone()),
                        None => ["rowid", "oid", "_rowid_"]
                            .iter()
                            .any(|r| column.value.eq_ignore_ascii_case(r))
                            .then_some(Value::Integer(rowid)),
                    },
                );
            if let Some(value) = value {
                out += &literal(&value);
                i += 3;
                continue;
            }
        }
        write_token(&mut out, &tokens[i]);
        i += 1;
    }
    Ok(out)
}

/// A value as SQL that reads back as exactly that value.
fn literal(value: &Value) -> String {
    match value {
        Value::Null => "NULL".into(),
        Value::Integer(i) if *i < 0 => format!("({i})"),
        Value::Integer(i) => i.to_string(),
        Value::Real(f) if f.is_infinite() => {
            if *f > 0.0 {
                "1e999".into()
            } else {
                "(-1e999)".into()
            }
        }
        // `{:?}` keeps a decimal point or exponent, so the planner reads a
        // real, and it round-trips exactly.
        Value::Real(f) if f.is_sign_negative() => format!("(-{:?})", -f),
        Value::Real(f) => format!("{f:?}"),
        Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
        Value::Blob(b) => format!(
            "X'{}'",
            b.iter().map(|x| format!("{x:02X}")).collect::<String>()
        ),
    }
}

/// Writes a token back as SQL. The tokenizer's own `Display` does not
/// double the quotes inside a quoted string or name.
fn write_token(out: &mut String, token: &Token) {
    match token {
        Token::SingleQuotedString(s) => *out += &format!("'{}'", s.replace('\'', "''")),
        Token::Word(w) => match w.quote_style {
            Some(q) => {
                let close = match q {
                    '[' => ']',
                    '`' => '`',
                    _ => q,
                };
                let doubled = format!("{close}{close}");
                *out += &format!("{q}{}{close}", w.value.replace(close, &doubled));
            }
            None => *out += &w.value,
        },
        other => *out += &other.to_string(),
    }
}

/// What a `RAISE()` call does.
#[derive(Debug, Clone)]
enum Raise {
    Ignore,
    Abort(String),
    Fail(String),
    Rollback(String),
}

/// Marks where a `RAISE()` was. No real text holds a NUL followed by this.
const RAISE_MARK: &str = "\0fishbowl raise ";

fn raise_index(text: &str) -> Option<usize> {
    text.strip_prefix(RAISE_MARK)?.parse().ok()
}

/// Takes each `RAISE()` out of a trigger's SELECT, leaving a marker string
/// in its place. If the marker appears in the result, that RAISE ran.
/// RAISE may be a result column or a CASE result inside one, the forms
/// triggers use; anywhere else it is not supported yet.
fn take_raises(query: &mut ast::Query) -> Result<Vec<Raise>> {
    let mut raises = Vec::new();
    if let ast::SetExpr::Select(select) = query.body.as_mut() {
        for item in &mut select.projection {
            if let ast::SelectItem::UnnamedExpr(expr)
            | ast::SelectItem::ExprWithAlias { expr, .. } = item
            {
                take_raise(expr, &mut raises)?;
            }
        }
    }
    if has_raise(&query.to_string())? {
        return unsupported("RAISE() here");
    }
    Ok(raises)
}

fn take_raise(expr: &mut ast::Expr, raises: &mut Vec<Raise>) -> Result<()> {
    match expr {
        ast::Expr::Function(function)
            if function.name.to_string().eq_ignore_ascii_case("raise") =>
        {
            let raise = parse_raise(function)?;
            *expr = ast::Expr::value(ast::Value::SingleQuotedString(format!(
                "{RAISE_MARK}{}",
                raises.len()
            )));
            raises.push(raise);
        }
        ast::Expr::Case {
            conditions,
            else_result,
            ..
        } => {
            for when in conditions {
                take_raise(&mut when.result, raises)?;
            }
            if let Some(e) = else_result {
                take_raise(e, raises)?;
            }
        }
        ast::Expr::Nested(inner) => take_raise(inner, raises)?,
        _ => {}
    }
    Ok(())
}

fn parse_raise(function: &ast::Function) -> Result<Raise> {
    let ast::FunctionArguments::List(list) = &function.args else {
        return Err(Error::Parse("near \")\": syntax error".into()));
    };
    let args: Vec<&ast::Expr> = list
        .args
        .iter()
        .filter_map(|arg| match arg {
            ast::FunctionArg::Unnamed(ast::FunctionArgExpr::Expr(e)) => Some(e),
            _ => None,
        })
        .collect();
    let kind = match args.first() {
        Some(ast::Expr::Identifier(ident)) => ident.value.to_ascii_uppercase(),
        _ => return Err(Error::Parse("near \"RAISE\": syntax error".into())),
    };
    let message = match args.get(1) {
        Some(ast::Expr::Value(v)) => match &v.value {
            ast::Value::SingleQuotedString(s) => Some(s.clone()),
            _ => return unsupported("a RAISE() message that is not a string"),
        },
        Some(_) => return unsupported("a RAISE() message that is not a string"),
        None => None,
    };
    let syntax = || Err(Error::Parse("near \")\": syntax error".into()));
    match (kind.as_str(), message, args.len()) {
        ("IGNORE", None, 1) => Ok(Raise::Ignore),
        ("ABORT", Some(m), 2) => Ok(Raise::Abort(m)),
        ("FAIL", Some(m), 2) => Ok(Raise::Fail(m)),
        ("ROLLBACK", Some(m), 2) => Ok(Raise::Rollback(m)),
        _ => syntax(),
    }
}

/// Whether `sql` calls `RAISE()`.
fn has_raise(sql: &str) -> Result<bool> {
    let tokens = Tokenizer::new(&FishbowlDialect::default(), sql)
        .tokenize()
        .map_err(|e| Error::Parse(e.to_string()))?;
    let significant: Vec<&Token> = tokens
        .iter()
        .filter(|t| !matches!(t, Token::Whitespace(_)))
        .collect();
    Ok(significant.windows(2).any(|pair| {
        matches!(pair[0], Token::Word(w) if w.quote_style.is_none() && w.value.eq_ignore_ascii_case("raise"))
            && *pair[1] == Token::LParen
    }))
}
