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
//!
//! Every statement runs inside a transaction. `BEGIN` opens one that lasts
//! until `COMMIT` or `ROLLBACK`; outside one, each statement is its own
//! transaction and commits as it ends, as in SQLite. A commit is durable
//! once the write-ahead log is synced ([`storage::wal`]).

pub mod access;
pub mod catalog;
pub mod dialect;
pub mod error;
pub mod exec;
pub mod expr;
pub mod plan;
pub mod planner;
pub mod storage;
pub mod trace;
pub mod value;
pub mod write;

use serde::Serialize;
use sqlparser::ast;
use sqlparser::parser::Parser;

pub use error::{Error, Result};
pub use plan::PlanProfile;
pub use storage::{Options, Recovery};
pub use trace::Trace;
pub use value::Value;

use catalog::{Catalog, TableSchema};
use dialect::FishbowlDialect;
use exec::ExecContext;
use plan::Plan;
use storage::index::IndexKey;
use storage::{IndexCursor, LogStore, PageStore, Row, Storage, TableScan};
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
    CreateView { view: String },
    DropView { view: String },
    Query(QueryResult),
    CreateIndex { index: String, table: String },
    DropIndex { index: String },
    Begin,
    Commit,
    Rollback,
}

#[derive(Debug, Clone, Serialize)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Row>,
    pub plan: PlanProfile,
    /// What each step did with each row, when tracing is on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace: Option<Trace>,
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
    /// Between `BEGIN` and `COMMIT` or `ROLLBACK`. Otherwise each statement
    /// is its own transaction.
    in_transaction: bool,
    /// The catalog as of the last commit, saved before the first statement
    /// since then that might change it, for rollback.
    saved_catalog: Option<Catalog>,
    /// Trace every query, recording at most this many events.
    trace_limit: Option<usize>,
}

impl Default for Database {
    fn default() -> Self {
        Self::new()
    }
}

impl Database {
    /// A new, empty database held in memory. Its write-ahead log is in
    /// memory too.
    pub fn new() -> Self {
        Database {
            catalog: Catalog::default(),
            storage: Storage::memory(),
            row_budget: None,
            in_transaction: false,
            saved_catalog: None,
            trace_limit: None,
        }
    }

    /// Opens a database file, creating it if it does not exist. Its
    /// write-ahead log is the file of the same name with `-wal` added.
    /// Opening recovers every committed transaction the log holds. A
    /// transaction is durable once its `COMMIT` (or, outside `BEGIN`, its
    /// statement) returns `Ok`.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let path = path.as_ref();
        let mut log_path = path.as_os_str().to_owned();
        log_path.push("-wal");
        let store = storage::store::FileStore::open(path)?;
        let log = storage::store::FileLog::open(std::path::Path::new(&log_path))?;
        Self::with_stores(Box::new(store), Box::new(log), Options::default())
    }

    /// Opens a database on any page store and log, recovering what the log
    /// holds, then rebuilding the catalog from the `CREATE TABLE`, `CREATE
    /// INDEX` and `CREATE VIEW` statements stored in the database.
    pub fn with_stores(
        store: Box<dyn PageStore>,
        log: Box<dyn LogStore>,
        options: Options,
    ) -> Result<Self> {
        let (storage, statements) = Storage::open(store, log, options)?;
        let mut catalog = Catalog::default();
        for sql in statements {
            for stmt in parse(&sql)? {
                match planner::plan_statement(&catalog, &stmt)? {
                    Plan::CreateTable { schema, .. } => catalog.add_table(schema)?,
                    Plan::CreateIndex { table, index, .. } => catalog.add_index(&table, index)?,
                    Plan::CreateView { view, .. } => catalog.add_view(view)?,
                    _ => return Err(Error::Storage(format!("bad schema entry: {sql}"))),
                }
            }
        }
        Ok(Database {
            catalog,
            storage,
            row_budget: None,
            in_transaction: false,
            saved_catalog: None,
            trace_limit: None,
        })
    }

    /// Whether a `BEGIN` is waiting for its `COMMIT` or `ROLLBACK`.
    pub fn in_transaction(&self) -> bool {
        self.in_transaction
    }

    /// What recovery found in the log when the database was opened.
    pub fn recovery(&self) -> Recovery {
        self.storage.recovery()
    }

    /// Copies the write-ahead log into the database file and empties it.
    /// This also happens on its own once the log holds 1000 pages.
    pub fn checkpoint(&mut self) -> Result<()> {
        self.storage.checkpoint()
    }

    /// Caps how many rows one statement may process. `None` means no cap.
    pub fn set_row_budget(&mut self, budget: Option<u64>) {
        self.row_budget = budget;
    }

    /// Records a row-level [`Trace`] of every query, up to `limit` events
    /// each, or stops tracing with `None`. Off by default.
    pub fn set_trace(&mut self, limit: Option<usize>) {
        self.trace_limit = limit;
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

    /// Every page of the B-tree behind a table or an index, for the page
    /// viewer. Reads only.
    pub fn inspect_tree(&self, name: &str) -> Result<storage::inspect::TreeInfo> {
        self.storage.inspect_tree(name)
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

    /// Runs one statement inside a transaction: the one `BEGIN` opened, or
    /// else one of its own that commits as the statement ends.
    fn execute_statement(&mut self, stmt: &ast::Statement) -> Result<StatementResult> {
        let active = self.in_transaction;
        match stmt {
            ast::Statement::StartTransaction {
                modes,
                statements,
                exception: None,
                has_end_keyword: false,
                ..
            } if modes.is_empty() && statements.is_empty() => {
                if active {
                    return Err(Error::Runtime(
                        "cannot start a transaction within a transaction".into(),
                    ));
                }
                self.in_transaction = true;
                return Ok(StatementResult::Begin);
            }
            // `END` is another name for `COMMIT`.
            ast::Statement::Commit { chain: false, .. } => {
                if !active {
                    return Err(Error::Runtime(
                        "cannot commit - no transaction is active".into(),
                    ));
                }
                self.commit()?;
                return Ok(StatementResult::Commit);
            }
            ast::Statement::Rollback {
                chain: false,
                savepoint: None,
            } => {
                if !active {
                    return Err(Error::Runtime(
                        "cannot rollback - no transaction is active".into(),
                    ));
                }
                self.rollback();
                return Ok(StatementResult::Rollback);
            }
            _ => {}
        }
        if !matches!(stmt, ast::Statement::Query(_)) && self.saved_catalog.is_none() {
            self.saved_catalog = Some(self.catalog.clone());
        }
        // A failed statement has already undone itself, except for what
        // `OR FAIL` keeps, so the transaction carries on. Outside `BEGIN`
        // it then commits, like SQLite's autocommit.
        let result = self.run_statement(stmt);
        match &result {
            Err(e) if ends_transaction(stmt, e) => self.rollback(),
            _ if !active => self.commit()?,
            _ => {}
        }
        result
    }

    /// Ends the transaction, keeping its changes. If the commit fails, the
    /// changes are dropped from memory; whether they reached the disk is
    /// unknown until the database is reopened.
    fn commit(&mut self) -> Result<()> {
        self.in_transaction = false;
        match self.storage.commit() {
            Ok(()) => {
                self.saved_catalog = None;
                Ok(())
            }
            Err(e) => {
                self.rollback();
                Err(e)
            }
        }
    }

    /// Ends the transaction, forgetting its changes.
    fn rollback(&mut self) {
        self.in_transaction = false;
        self.storage.rollback();
        if let Some(catalog) = self.saved_catalog.take() {
            self.catalog = catalog;
        }
    }

    fn run_statement(&mut self, stmt: &ast::Statement) -> Result<StatementResult> {
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
            // A view holds no rows: the catalog keeps its SELECT, and the
            // schema table keeps its statement for when the file is reopened.
            Plan::CreateView {
                view,
                if_not_exists,
                temporary,
            } => {
                let name = view.name.clone();
                if if_not_exists && self.catalog.view(&name).is_some() {
                    return Ok(StatementResult::CreateView { view: name });
                }
                self.catalog.add_view(view)?;
                if !temporary && let Err(e) = self.storage.create_view(&name, &stmt.to_string()) {
                    self.catalog.remove_view(&name);
                    return Err(e);
                }
                StatementResult::CreateView { view: name }
            }
            Plan::DropView { view, .. } => {
                if self.catalog.remove_view(&view).is_some() {
                    self.storage.drop_view(&view)?;
                }
                StatementResult::DropView { view }
            }
            Plan::Query(query) => {
                let mut ctx = ExecContext::new(&self.storage, self.row_budget);
                if let Some(limit) = self.trace_limit {
                    ctx.start_trace(limit);
                }
                let (rows, plan) = exec::run_query(&mut ctx, &query.root)?;
                return Ok(StatementResult::Query(QueryResult {
                    columns: query.columns,
                    rows,
                    plan,
                    trace: ctx.take_trace(),
                }));
            }
        };
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
        self.storage.check_pages()?;
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

/// Errors that end the whole transaction rather than just the statement, as
/// in SQLite: a broken constraint under `INSERT OR ROLLBACK`, and a storage
/// error, after which the statement's own undo cannot be trusted.
fn ends_transaction(stmt: &ast::Statement, error: &Error) -> bool {
    match error {
        Error::Storage(_) => true,
        Error::Constraint(_) => matches!(
            stmt,
            ast::Statement::Insert(insert) if insert.or == Some(ast::SqliteOnConflict::Rollback)
        ),
        _ => false,
    }
}

fn parse(sql: &str) -> Result<Vec<ast::Statement>> {
    Parser::parse_sql(&FishbowlDialect::default(), sql).map_err(|e| Error::Parse(e.to_string()))
}
