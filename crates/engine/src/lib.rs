//! Fishbowl: a SQL database written from scratch, built to be watched.
//!
//! A statement goes through four stages:
//!
//! 1. **Parse** SQL text into a syntax tree (the `sqlparser` crate).
//! 2. **Plan**: resolve names against the [`catalog`] and pick the steps to run
//!    ([`planner`]).
//! 3. **Execute** the plan, pulling rows through each step ([`exec`]).
//! 4. **Store** rows ([`storage`], in memory for now).

pub mod catalog;
pub mod error;
pub mod exec;
pub mod expr;
pub mod plan;
pub mod planner;
pub mod storage;
pub mod value;

use serde::Serialize;
use sqlparser::dialect::SQLiteDialect;
use sqlparser::parser::Parser;

pub use error::{Error, Result};
pub use plan::PlanProfile;
pub use value::Value;

use catalog::{Catalog, TableSchema};
use exec::ExecContext;
use plan::Plan;
use storage::{MemoryStorage, Row};

/// What a statement produced.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StatementResult {
    CreateTable { table: String },
    Insert { table: String, rows: u64 },
    Query(QueryResult),
}

#[derive(Debug, Clone, Serialize)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Row>,
    pub plan: PlanProfile,
}

/// One statement from a script and its outcome.
#[derive(Debug, Clone)]
pub struct ScriptEntry {
    pub sql: String,
    pub result: Result<StatementResult>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TableInfo {
    #[serde(flatten)]
    pub schema: TableSchema,
    pub row_count: usize,
}

#[derive(Debug, Default)]
pub struct Database {
    catalog: Catalog,
    storage: MemoryStorage,
    row_budget: Option<u64>,
}

impl Database {
    pub fn new() -> Self {
        Self::default()
    }

    /// Caps how many rows one statement may process. `None` means no cap.
    pub fn set_row_budget(&mut self, budget: Option<u64>) {
        self.row_budget = budget;
    }

    /// Runs exactly one SQL statement.
    pub fn execute(&mut self, sql: &str) -> Result<StatementResult> {
        match parse(sql)?.as_slice() {
            [stmt] => self.execute_statement(stmt),
            [] => Err(Error::Parse("no statement to run".into())),
            _ => Err(Error::Parse("expected one statement".into())),
        }
    }

    /// Runs every statement in `sql`, stopping at the first error. A parse
    /// error anywhere means nothing runs.
    pub fn run_script(&mut self, sql: &str) -> Vec<ScriptEntry> {
        let statements = match parse(sql) {
            Ok(statements) => statements,
            Err(e) => {
                return vec![ScriptEntry {
                    sql: sql.trim().to_string(),
                    result: Err(e),
                }];
            }
        };
        let mut entries = Vec::new();
        for stmt in &statements {
            let result = self.execute_statement(stmt);
            let failed = result.is_err();
            entries.push(ScriptEntry {
                sql: stmt.to_string(),
                result,
            });
            if failed {
                break;
            }
        }
        entries
    }

    pub fn tables(&self) -> Vec<TableInfo> {
        self.catalog
            .tables()
            .map(|schema| TableInfo {
                schema: schema.clone(),
                row_count: self.storage.row_count(&schema.name),
            })
            .collect()
    }

    fn execute_statement(&mut self, stmt: &sqlparser::ast::Statement) -> Result<StatementResult> {
        match planner::plan_statement(&self.catalog, stmt)? {
            Plan::CreateTable {
                schema,
                if_not_exists,
            } => {
                let table = schema.name.clone();
                if if_not_exists && self.catalog.table(&table).is_ok() {
                    return Ok(StatementResult::CreateTable { table });
                }
                self.catalog.add_table(schema)?;
                self.storage.create_table(&table);
                Ok(StatementResult::CreateTable { table })
            }
            Plan::Insert {
                table,
                targets,
                rows,
            } => self.insert(&table, &targets, &rows),
            Plan::Query(query) => {
                let mut ctx = ExecContext::new(&self.storage, self.row_budget);
                let (rows, plan) = exec::run_query(&mut ctx, &query.root)?;
                Ok(StatementResult::Query(QueryResult {
                    columns: query.columns,
                    rows,
                    plan,
                }))
            }
        }
    }

    /// Builds and checks every row before storing any, so a failing INSERT
    /// leaves the table unchanged.
    fn insert(
        &mut self,
        table: &str,
        targets: &[usize],
        rows: &[Vec<expr::Expr>],
    ) -> Result<StatementResult> {
        let schema = self.catalog.table(table)?;
        let mut built = Vec::with_capacity(rows.len());
        for exprs in rows {
            let mut row = vec![Value::Null; schema.columns.len()];
            for (&target, expr) in targets.iter().zip(exprs) {
                row[target] = expr
                    .eval(&[])
                    .apply_affinity(schema.columns[target].affinity);
            }
            for (column, value) in schema.columns.iter().zip(&row) {
                if column.not_null && value.is_null() {
                    return Err(Error::Constraint(format!(
                        "NOT NULL constraint failed: {}.{}",
                        schema.name, column.name
                    )));
                }
            }
            built.push(row);
        }
        let count = built.len() as u64;
        for row in built {
            self.storage.append(table, row);
        }
        Ok(StatementResult::Insert {
            table: table.to_string(),
            rows: count,
        })
    }
}

fn parse(sql: &str) -> Result<Vec<sqlparser::ast::Statement>> {
    Parser::parse_sql(&SQLiteDialect {}, sql).map_err(|e| Error::Parse(e.to_string()))
}
