//! Fishbowl: a SQL database written from scratch, built to be watched.
//!
//! A statement goes through four stages:
//!
//! 1. **Parse** SQL text into a syntax tree (the `sqlparser` crate).
//! 2. **Plan**: resolve names against the [`catalog`] and pick the steps to run
//!    ([`planner`]).
//! 3. **Execute** the plan, pulling rows through each step ([`exec`]).
//! 4. **Store** rows in B+trees on 4 KiB pages ([`storage`]), in a file or in
//!    memory.

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
use storage::{PageStore, Row, Storage};

/// What a statement produced.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StatementResult {
    CreateTable { table: String },
    Insert { table: String, rows: u64 },
    Update { table: String, rows: u64 },
    Delete { table: String, rows: u64 },
    DropTable { table: String },
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

pub struct Database {
    catalog: Catalog,
    storage: Storage,
    row_budget: Option<u64>,
}

impl Default for Database {
    fn default() -> Self {
        Self::new()
    }
}

impl Database {
    /// A new, empty database held in memory.
    pub fn new() -> Self {
        Database {
            catalog: Catalog::default(),
            storage: Storage::memory(),
            row_budget: None,
        }
    }

    /// Opens a database file, creating it if it does not exist. Every
    /// statement that changes data is written to the file before it returns.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let store = storage::store::FileStore::open(path.as_ref())?;
        Self::with_store(Box::new(store))
    }

    /// Opens a database on any page store, rebuilding the catalog from the
    /// `CREATE TABLE` statements stored in it.
    pub fn with_store(store: Box<dyn PageStore>) -> Result<Self> {
        let (storage, statements) = Storage::open(store)?;
        let mut catalog = Catalog::default();
        for sql in statements {
            for stmt in parse(&sql)? {
                match planner::plan_statement(&catalog, &stmt)? {
                    Plan::CreateTable { schema, .. } => catalog.add_table(schema)?,
                    _ => return Err(Error::Storage(format!("bad schema entry: {sql}"))),
                }
            }
        }
        Ok(Database {
            catalog,
            storage,
            row_budget: None,
        })
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
        let result = match planner::plan_statement(&self.catalog, stmt)? {
            Plan::CreateTable {
                schema,
                if_not_exists,
            } => {
                let table = schema.name.clone();
                if if_not_exists && self.catalog.table(&table).is_ok() {
                    return Ok(StatementResult::CreateTable { table });
                }
                self.catalog.add_table(schema)?;
                if let Err(e) = self.storage.create_table(&table, &stmt.to_string()) {
                    self.catalog.remove_table(&table);
                    return Err(e);
                }
                StatementResult::CreateTable { table }
            }
            Plan::Insert {
                table,
                targets,
                rows,
            } => self.insert(&table, &targets, &rows)?,
            Plan::Update {
                table,
                assignments,
                predicate,
            } => self.update(&table, &assignments, predicate.as_ref())?,
            Plan::Delete { table, predicate } => self.delete(&table, predicate.as_ref())?,
            Plan::DropTable { table, if_exists } => {
                if if_exists && self.catalog.table(&table).is_err() {
                    return Ok(StatementResult::DropTable { table });
                }
                self.storage.drop_table(&table)?;
                self.catalog.remove_table(&table);
                StatementResult::DropTable { table }
            }
            Plan::Query(query) => {
                let mut ctx = ExecContext::new(&self.storage, self.row_budget);
                let (rows, plan) = exec::run_query(&mut ctx, &query.root)?;
                return Ok(StatementResult::Query(QueryResult {
                    columns: query.columns,
                    rows,
                    plan,
                }));
            }
        };
        // Every change reaches the page store before the statement returns.
        self.storage.flush()?;
        Ok(result)
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
                    .eval(&[])?
                    .apply_affinity(schema.columns[target].affinity);
            }
            check_not_null(schema, &row)?;
            built.push(row);
        }
        for row in &built {
            self.storage.insert(table, row)?;
        }
        Ok(StatementResult::Insert {
            table: table.to_string(),
            rows: built.len() as u64,
        })
    }

    /// Computes every new row from the old one before writing any, so a
    /// failing UPDATE leaves the table unchanged, and a row is never updated
    /// twice by meeting it again later in the scan.
    fn update(
        &mut self,
        table: &str,
        assignments: &[(usize, expr::Expr)],
        predicate: Option<&expr::Expr>,
    ) -> Result<StatementResult> {
        let schema = self.catalog.table(table)?;
        let mut updated = Vec::new();
        for (rowid, old) in self.matching_rows(table, predicate)? {
            let mut row = old.clone();
            for (index, expr) in assignments {
                row[*index] = expr
                    .eval(&old)?
                    .apply_affinity(schema.columns[*index].affinity);
            }
            check_not_null(schema, &row)?;
            updated.push((rowid, row));
        }
        for (rowid, row) in &updated {
            self.storage.update(table, *rowid, row)?;
        }
        Ok(StatementResult::Update {
            table: table.to_string(),
            rows: updated.len() as u64,
        })
    }

    fn delete(&mut self, table: &str, predicate: Option<&expr::Expr>) -> Result<StatementResult> {
        let doomed = self.matching_rows(table, predicate)?;
        for (rowid, _) in &doomed {
            self.storage.delete(table, *rowid)?;
        }
        Ok(StatementResult::Delete {
            table: table.to_string(),
            rows: doomed.len() as u64,
        })
    }

    /// The rows an UPDATE or DELETE applies to, with their rowids. Reading
    /// them all before changing any keeps the scan away from rows it is
    /// changing underneath itself.
    fn matching_rows(
        &self,
        table: &str,
        predicate: Option<&expr::Expr>,
    ) -> Result<Vec<(i64, Row)>> {
        let mut scan = storage::TableScan::new(table);
        let mut matches = Vec::new();
        let mut processed = 0u64;
        while let Some((rowid, row)) = scan.next_entry(&self.storage)? {
            processed += 1;
            if let Some(limit) = self.row_budget.filter(|&limit| processed > limit) {
                return Err(Error::BudgetExceeded(limit));
            }
            let keep = match predicate {
                Some(p) => p.eval(&row)?.truthiness() == Some(true),
                None => true,
            };
            if keep {
                matches.push((rowid, row));
            }
        }
        Ok(matches)
    }
}

fn check_not_null(schema: &TableSchema, row: &[Value]) -> Result<()> {
    for (column, value) in schema.columns.iter().zip(row) {
        if column.not_null && value.is_null() {
            return Err(Error::Constraint(format!(
                "NOT NULL constraint failed: {}.{}",
                schema.name, column.name
            )));
        }
    }
    Ok(())
}

fn parse(sql: &str) -> Result<Vec<sqlparser::ast::Statement>> {
    Parser::parse_sql(&SQLiteDialect {}, sql).map_err(|e| Error::Parse(e.to_string()))
}
