//! Fishbowl: a SQL database written from scratch, built to be watched.
//!
//! A statement goes through four stages:
//!
//! 1. **Parse** SQL text into a syntax tree (the `sqlparser` crate).
//! 2. **Plan**: resolve names against the [`catalog`] and pick the steps to run
//!    ([`planner`]).
//! 3. **Execute** the plan, pulling rows through each step ([`exec`]).
//! 4. **Store** rows in B+trees on 4 KiB pages ([`storage`]), in a file or in
//!    memory, keeping each table's indexes in step ([`write`]).

pub mod access;
pub mod catalog;
pub mod error;
pub mod exec;
pub mod expr;
pub mod plan;
pub mod planner;
pub mod storage;
pub mod value;
pub mod write;

use serde::Serialize;
use sqlparser::dialect::SQLiteDialect;
use sqlparser::parser::Parser;

pub use error::{Error, Result};
pub use plan::PlanProfile;
pub use value::Value;

use catalog::{Catalog, TableSchema};
use exec::ExecContext;
use plan::Plan;
use storage::index::IndexKey;
use storage::{IndexCursor, PageStore, Row, Storage, TableScan};
use write::{OnConflict, TableWriter};

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
    CreateIndex { index: String, table: String },
    DropIndex { index: String },
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
    /// `CREATE TABLE` and `CREATE INDEX` statements stored in it.
    pub fn with_store(store: Box<dyn PageStore>) -> Result<Self> {
        let (storage, statements) = Storage::open(store)?;
        let mut catalog = Catalog::default();
        for sql in statements {
            for stmt in parse(&sql)? {
                match planner::plan_statement(&catalog, &stmt)? {
                    Plan::CreateTable { schema, .. } => catalog.add_table(schema)?,
                    Plan::CreateIndex { table, index, .. } => catalog.add_index(&table, index)?,
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
                // Indexes for PRIMARY KEY and UNIQUE come with the table.
                let indexes: Vec<String> = schema.indexes.iter().map(|i| i.name.clone()).collect();
                self.catalog.add_table(schema)?;
                let created = self
                    .storage
                    .create_table(&table, &stmt.to_string())
                    .and_then(|()| {
                        indexes
                            .iter()
                            .try_for_each(|index| self.storage.create_index(index, &table, None))
                    });
                if let Err(e) = created {
                    self.catalog.remove_table(&table);
                    return Err(e);
                }
                StatementResult::CreateTable { table }
            }
            Plan::CreateIndex {
                table,
                index,
                if_not_exists,
            } => {
                let name = index.name.clone();
                if if_not_exists && self.catalog.index(&name).is_some() {
                    return Ok(StatementResult::CreateIndex { index: name, table });
                }
                self.catalog.add_index(&table, index.clone())?;
                let created = self
                    .storage
                    .create_index(&name, &table, Some(&stmt.to_string()));
                if let Err(e) = created {
                    self.catalog.remove_index(&name);
                    return Err(e);
                }
                let schema = self.catalog.table(&table)?;
                if let Err(e) = write::build_index(&mut self.storage, schema, &index) {
                    self.storage.drop_index(&name)?;
                    self.catalog.remove_index(&name);
                    return Err(e);
                }
                StatementResult::CreateIndex { index: name, table }
            }
            Plan::DropIndex { name, if_exists } => {
                match self.catalog.index(&name) {
                    None if if_exists => return Ok(StatementResult::DropIndex { index: name }),
                    None => return Err(Error::Catalog(format!("no such index: {name}"))),
                    Some((_, index)) if index.automatic => {
                        return Err(Error::Catalog(
                            "index associated with UNIQUE or PRIMARY KEY constraint cannot be \
                             dropped"
                                .into(),
                        ));
                    }
                    Some(_) => {}
                }
                self.storage.drop_index(&name)?;
                self.catalog.remove_index(&name);
                StatementResult::DropIndex { index: name }
            }
            Plan::Insert {
                table,
                targets,
                rows,
                on_conflict,
            } => {
                let rows = rows
                    .iter()
                    .map(|exprs| exprs.iter().map(|e| e.eval(&[])).collect())
                    .collect::<Result<Vec<_>>>()?;
                self.insert(&table, &targets, rows, on_conflict)?
            }
            Plan::InsertSelect {
                table,
                targets,
                query,
                on_conflict,
            } => {
                let mut ctx = ExecContext::new(&self.storage, self.row_budget);
                let (rows, _) = exec::run_query(&mut ctx, &query.root)?;
                self.insert(&table, &targets, rows, on_conflict)?
            }
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

    /// Stores rows of values, one per VALUES row or SELECT result row, each
    /// value for the column in `targets` at its position. If any row fails a
    /// constraint, the rows stored before it are taken out again, unless
    /// `on_conflict` says otherwise.
    fn insert(
        &mut self,
        table: &str,
        targets: &[usize],
        rows: Vec<Vec<Value>>,
        on_conflict: OnConflict,
    ) -> Result<StatementResult> {
        let schema = self.catalog.table(table)?;
        let mut count = 0;
        let mut writer = TableWriter::new(schema, &mut self.storage, on_conflict);
        let result = rows.into_iter().try_for_each(|values| {
            let mut row = vec![Value::Null; schema.columns.len()];
            for (&target, value) in targets.iter().zip(values) {
                row[target] = value.apply_affinity(schema.columns[target].affinity);
            }
            count += u64::from(writer.insert(row)?);
            Ok(())
        });
        writer.finish(result)?;
        Ok(StatementResult::Insert {
            table: table.to_string(),
            rows: count,
        })
    }

    /// Computes each new row from the old one. The matching rows are found
    /// before any is changed, so a row is never updated twice by meeting it
    /// again later in the scan. A failure undoes the rows already updated.
    fn update(
        &mut self,
        table: &str,
        assignments: &[(usize, expr::Expr)],
        predicate: Option<&expr::Expr>,
    ) -> Result<StatementResult> {
        let schema = self.catalog.table(table)?;
        let access = access::for_write(schema, predicate);
        let mut matches = self.matching_rows(table, &access, predicate)?;
        // Rows change in the order the scan found them, and each change is
        // checked against the rows not yet changed, so the order decides
        // which UPDATEs fail. Like SQLite, change rows in rowid order instead
        // when the UPDATE moves them within the tree that was scanned.
        let scanned = schema
            .indexes
            .iter()
            .find(|i| access.index.as_ref() == Some(&i.name));
        let moves = assignments.iter().any(|(column, _)| {
            Some(*column) == schema.rowid_alias
                || scanned.is_some_and(|i| i.columns.iter().any(|c| c.position == *column))
        });
        if moves {
            matches.sort_by_key(|(rowid, _)| *rowid);
        }
        let count = matches.len() as u64;
        let mut writer = TableWriter::new(schema, &mut self.storage, OnConflict::Abort);
        let result = matches.into_iter().try_for_each(|(rowid, old)| {
            let mut row = old.clone();
            for (index, expr) in assignments {
                row[*index] = expr
                    .eval(&old)?
                    .apply_affinity(schema.columns[*index].affinity);
            }
            writer.update(rowid, old, row)
        });
        writer.finish(result)?;
        Ok(StatementResult::Update {
            table: table.to_string(),
            rows: count,
        })
    }

    fn delete(&mut self, table: &str, predicate: Option<&expr::Expr>) -> Result<StatementResult> {
        let schema = self.catalog.table(table)?;
        let access = access::for_write(schema, predicate);
        let doomed = self.matching_rows(table, &access, predicate)?;
        let count = doomed.len() as u64;
        let mut writer = TableWriter::new(schema, &mut self.storage, OnConflict::Abort);
        let result = doomed
            .into_iter()
            .try_for_each(|(rowid, row)| writer.delete(rowid, row));
        writer.finish(result)?;
        Ok(StatementResult::Delete {
            table: table.to_string(),
            rows: count,
        })
    }

    /// Checks that every B-tree keeps its rules and that every index holds
    /// exactly one entry per row of its table, with that row's values, like
    /// SQLite's `PRAGMA integrity_check`. Also checks that no two rows
    /// share a key in a unique index.
    pub fn integrity_check(&self) -> Result<()> {
        let broken = |what: String| Err(Error::Storage(format!("integrity check: {what}")));
        for schema in self.catalog.tables() {
            self.storage.check_trees(&schema.name)?;
            let mut rows = Vec::new();
            let mut scan = TableScan::new(&schema.name);
            while let Some(entry) = scan.next_entry(&self.storage)? {
                rows.push(entry);
            }
            if let Some(pk) = schema.rowid_alias
                && let Some((rowid, _)) = rows
                    .iter()
                    .find(|(rowid, row)| row[pk] != Value::Integer(*rowid))
            {
                return broken(format!("row {rowid} of {} has the wrong key", schema.name));
            }
            for index in &schema.indexes {
                let mut expected: Vec<IndexKey> = rows
                    .iter()
                    .map(|(rowid, row)| write::index_key(index, *rowid, row))
                    .collect();
                expected.sort();
                let mut actual = Vec::new();
                let mut cursor = IndexCursor::new(&index.name, None);
                while let Some(key) = cursor.next(&self.storage)? {
                    actual.push(key);
                }
                let same = |a: &IndexKey, b: &IndexKey| a.rowid == b.rowid && a.values == b.values;
                if expected.len() != actual.len()
                    || !expected.iter().zip(&actual).all(|(a, b)| same(a, b))
                {
                    return broken(format!(
                        "index {} does not match table {}",
                        index.name, schema.name
                    ));
                }
                let repeats = actual.windows(2).any(|pair| {
                    !pair[0].values.iter().any(Value::is_null)
                        && pair[0].compare_prefix(&pair[1].values).is_eq()
                });
                if index.unique && repeats {
                    return broken(format!("unique index {} has a repeated key", index.name));
                }
            }
        }
        Ok(())
    }

    /// The rows an UPDATE or DELETE applies to, with their rowids, read
    /// through `access`. Reading them all before changing any keeps the scan
    /// away from rows it is changing underneath itself.
    fn matching_rows(
        &self,
        table: &str,
        access: &access::IndexAccess,
        predicate: Option<&expr::Expr>,
    ) -> Result<Vec<(i64, Row)>> {
        let mut scan = access::Scan::new(table, access);
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

fn parse(sql: &str) -> Result<Vec<sqlparser::ast::Statement>> {
    Parser::parse_sql(&SQLiteDialect {}, sql).map_err(|e| Error::Parse(e.to_string()))
}
