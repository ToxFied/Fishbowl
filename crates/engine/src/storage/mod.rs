//! Storage: where rows live between statements.
//!
//! From the bottom up:
//!
//! - [`store`]: a [`PageStore`] reads and writes numbered 4 KiB pages, and a
//!   [`LogStore`] holds the write-ahead log, in files on native builds or in
//!   memory in the browser. [`fault`] has stores that lose power on purpose.
//! - [`wal`]: the write-ahead log. A commit appends the changed pages to it,
//!   and a checkpoint copies them into the database file.
//! - [`pager`]: the [`Pager`] caches pages in a small buffer pool, keeps a
//!   transaction's changes in memory until it commits, and hands out free
//!   pages.
//! - [`page`]: the byte layout of every kind of page.
//! - [`btree`]: each table is a B+tree of rows keyed by rowid, and each index
//!   a B+tree of (values, rowid) keys.
//! - [`record`]: how one row of values becomes bytes.
//! - [`index`]: index keys and their order.
//! - [`inspect`]: read-only views of a tree's pages, for the page viewer.
//!
//! This module ties them together. Like SQLite's `sqlite_schema`, a schema
//! table at page 1 lists every table, index and view with its root page and
//! the statement that made it, so a database file can be reopened. A view
//! has no rows, so its root page is 0.
//!
//! Storage knows nothing about columns or constraints. The caller decides
//! what goes into an index and checks the rules; see `write.rs`.
//!
//! Every change waits in memory until [`Storage::commit`] makes it durable
//! or [`Storage::rollback`] forgets it.

pub mod btree;
pub mod fault;
pub mod index;
pub mod inspect;
pub mod page;
pub mod pager;
pub mod record;
pub mod store;
pub mod wal;

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;

use crate::error::{Error, Result};
use crate::value::Value;
use btree::{BTree, Cursor};
use index::{IndexKey, MAX_KEY_SIZE};
use page::{Key, PageId};
pub use pager::Options;
use pager::Pager;
pub use store::{LogStore, MemoryLog, MemoryStore, PageStore};
pub use wal::Recovery;

pub type Row = Vec<Value>;

/// The schema table's root, the first page after the header.
const SCHEMA_ROOT: PageId = 1;

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Storage(e.to_string())
    }
}

pub struct Storage {
    /// Reading through the buffer pool updates the cache, so even a read
    /// needs to change the pager. `RefCell` lets queries read through a
    /// shared `&Storage` while writes go through `&mut`.
    pager: RefCell<Pager>,
    schema: BTree,
    /// Keyed by lowercase name, like the catalog.
    tables: HashMap<String, Table>,
    indexes: HashMap<String, Index>,
    /// Each view's row in the schema table.
    views: HashMap<String, i64>,
    /// Each trigger's row in the schema table.
    triggers: HashMap<String, i64>,
    /// The four maps above as they were at the last commit, saved before
    /// the first change after it, for rollback.
    saved: Option<Saved>,
}

#[derive(Clone)]
struct Saved {
    tables: HashMap<String, Table>,
    indexes: HashMap<String, Index>,
    views: HashMap<String, i64>,
    triggers: HashMap<String, i64>,
}

#[derive(Clone)]
struct Table {
    tree: BTree,
    /// The table's row in the schema table.
    schema_rowid: i64,
    rows: usize,
}

#[derive(Clone)]
struct Index {
    tree: BTree<IndexKey>,
    /// The table it indexes, lowercase.
    table: String,
    schema_rowid: i64,
}

/// One row of the schema table, laid out like SQLite's `sqlite_schema`:
/// `(type, name, tbl_name, rootpage, sql)`. An index made for a `UNIQUE` or
/// `PRIMARY KEY` constraint has no SQL of its own.
fn schema_row(kind: &str, name: &str, table: &str, root: PageId, sql: Option<&str>) -> Row {
    vec![
        Value::Text(kind.into()),
        Value::Text(name.into()),
        Value::Text(table.into()),
        Value::Integer(root.into()),
        sql.map_or(Value::Null, |sql| Value::Text(sql.into())),
    ]
}

impl Storage {
    /// A new database held in memory, log included.
    pub fn memory() -> Storage {
        let (storage, _) = Storage::open(
            Box::new(MemoryStore::default()),
            Box::new(MemoryLog::default()),
            Options::default(),
        )
        .expect("creating an empty in-memory database cannot fail");
        storage
    }

    /// Opens a database file and its log, recovering any transactions the
    /// log holds, or creates a new database if both are empty. Also returns
    /// the `CREATE TABLE`, `CREATE INDEX`, `CREATE VIEW` and `CREATE TRIGGER`
    /// statements it holds, oldest first, so the caller can rebuild its
    /// catalog.
    pub fn open(
        store: Box<dyn PageStore>,
        log: Box<dyn LogStore>,
        options: Options,
    ) -> Result<(Storage, Vec<String>)> {
        let mut pager = Pager::open(store, log, options)?;
        let schema = if pager.page_count() == 1 {
            let schema = BTree::create(&mut pager)?;
            debug_assert_eq!(schema.root(), SCHEMA_ROOT);
            pager.commit()?;
            schema
        } else {
            BTree::open(SCHEMA_ROOT)
        };

        let mut tables = HashMap::new();
        let mut indexes = HashMap::new();
        let mut views = HashMap::new();
        let mut triggers = HashMap::new();
        let mut statements = Vec::new();
        let mut cursor = Cursor::new(&mut pager, schema)?;
        while let Some((schema_rowid, bytes)) = cursor.next(&mut pager)? {
            let row = record::decode(&bytes)?;
            let damaged = || Error::Storage(format!("schema row {schema_rowid} is damaged"));
            let [
                Value::Text(kind),
                Value::Text(name),
                Value::Text(table),
                Value::Integer(root),
                sql,
            ] = row.as_slice()
            else {
                return Err(damaged());
            };
            let root = PageId::try_from(*root).map_err(|_| damaged())?;
            match (kind.as_str(), sql) {
                ("table", Value::Text(sql)) => {
                    let tree = BTree::open(root);
                    let rows = tree.count(&mut pager)?;
                    let table = Table {
                        tree,
                        schema_rowid,
                        rows,
                    };
                    tables.insert(name.to_ascii_lowercase(), table);
                    statements.push(sql.clone());
                }
                ("index", sql) => {
                    let index = Index {
                        tree: BTree::open(root),
                        table: table.to_ascii_lowercase(),
                        schema_rowid,
                    };
                    indexes.insert(name.to_ascii_lowercase(), index);
                    if let Value::Text(sql) = sql {
                        statements.push(sql.clone());
                    }
                }
                ("view", Value::Text(sql)) => {
                    views.insert(name.to_ascii_lowercase(), schema_rowid);
                    statements.push(sql.clone());
                }
                ("trigger", Value::Text(sql)) => {
                    triggers.insert(name.to_ascii_lowercase(), schema_rowid);
                    statements.push(sql.clone());
                }
                _ => return Err(damaged()),
            }
        }
        let storage = Storage {
            pager: RefCell::new(pager),
            schema,
            tables,
            indexes,
            views,
            triggers,
            saved: None,
        };
        Ok((storage, statements))
    }

    /// Makes every change since the last commit durable. On an error the
    /// outcome is unknown until the database is reopened, and every later
    /// call fails.
    pub fn commit(&mut self) -> Result<()> {
        self.pager.get_mut().commit()?;
        self.saved = None;
        Ok(())
    }

    /// Forgets every change since the last commit.
    pub fn rollback(&mut self) {
        self.pager.get_mut().rollback();
        if let Some(saved) = self.saved.take() {
            self.tables = saved.tables;
            self.indexes = saved.indexes;
            self.views = saved.views;
            self.triggers = saved.triggers;
        }
    }

    /// Copies the write-ahead log into the database file and empties it.
    pub fn checkpoint(&mut self) -> Result<()> {
        self.pager.get_mut().checkpoint()
    }

    /// What recovery found when the database was opened.
    pub fn recovery(&self) -> Recovery {
        self.pager.borrow().recovery()
    }

    /// Frames in the log waiting for a checkpoint.
    pub fn log_frames(&self) -> u64 {
        self.pager.borrow().log_frames()
    }

    /// Saves the maps before the first change since the last commit.
    fn save(&mut self) {
        if self.saved.is_none() {
            self.saved = Some(Saved {
                tables: self.tables.clone(),
                indexes: self.indexes.clone(),
                views: self.views.clone(),
                triggers: self.triggers.clone(),
            });
        }
    }

    /// Creates an empty table. `sql` is recorded so the table can be rebuilt
    /// when the file is reopened.
    pub fn create_table(&mut self, name: &str, sql: &str) -> Result<()> {
        self.save();
        let pager = self.pager.get_mut();
        let tree = BTree::create(pager)?;
        let schema_rowid = add_schema_row(
            pager,
            self.schema,
            &schema_row("table", name, name, tree.root(), Some(sql)),
        )?;
        self.tables.insert(
            name.to_ascii_lowercase(),
            Table {
                tree,
                schema_rowid,
                rows: 0,
            },
        );
        Ok(())
    }

    /// Creates an empty index on `table`. `sql` is the `CREATE INDEX`
    /// statement, or `None` for an index a constraint made, which the
    /// `CREATE TABLE` statement brings back.
    pub fn create_index(&mut self, name: &str, table: &str, sql: Option<&str>) -> Result<()> {
        self.save();
        let table_name = table.to_ascii_lowercase();
        if !self.tables.contains_key(&table_name) {
            return Err(no_such_table(table));
        }
        let pager = self.pager.get_mut();
        let tree = BTree::create(pager)?;
        let schema_rowid = add_schema_row(
            pager,
            self.schema,
            &schema_row("index", name, table, tree.root(), sql),
        )?;
        let index = Index {
            tree,
            table: table_name,
            schema_rowid,
        };
        self.indexes.insert(name.to_ascii_lowercase(), index);
        Ok(())
    }

    /// Deletes a table and its indexes, and returns all of their pages to
    /// the free list.
    pub fn drop_table(&mut self, name: &str) -> Result<()> {
        self.save();
        let key = name.to_ascii_lowercase();
        let table = self
            .tables
            .remove(&key)
            .ok_or_else(|| no_such_table(name))?;
        let doomed: Vec<String> = self
            .indexes
            .iter()
            .filter(|(_, index)| index.table == key)
            .map(|(name, _)| name.clone())
            .collect();
        for index in doomed {
            self.drop_index(&index)?;
        }
        let pager = self.pager.get_mut();
        table.tree.destroy(pager)?;
        self.schema.delete(pager, table.schema_rowid)?;
        Ok(())
    }

    /// Records a view's `CREATE VIEW` statement. A view holds no rows, so
    /// that is all there is to store.
    pub fn create_view(&mut self, name: &str, sql: &str) -> Result<()> {
        self.save();
        let pager = self.pager.get_mut();
        let row = schema_row("view", name, name, 0, Some(sql));
        let schema_rowid = add_schema_row(pager, self.schema, &row)?;
        self.views.insert(name.to_ascii_lowercase(), schema_rowid);
        Ok(())
    }

    /// Forgets a view. A view that was never stored, like a TEMP view, is
    /// not an error.
    pub fn drop_view(&mut self, name: &str) -> Result<()> {
        self.save();
        if let Some(schema_rowid) = self.views.remove(&name.to_ascii_lowercase()) {
            self.schema.delete(self.pager.get_mut(), schema_rowid)?;
        }
        Ok(())
    }

    /// Records a trigger's `CREATE TRIGGER` statement. Like a view, a
    /// trigger is only its statement.
    pub fn create_trigger(&mut self, name: &str, table: &str, sql: &str) -> Result<()> {
        self.save();
        let pager = self.pager.get_mut();
        let row = schema_row("trigger", name, table, 0, Some(sql));
        let schema_rowid = add_schema_row(pager, self.schema, &row)?;
        self.triggers
            .insert(name.to_ascii_lowercase(), schema_rowid);
        Ok(())
    }

    /// Forgets a trigger. One that was never stored, like a TEMP trigger,
    /// is not an error.
    pub fn drop_trigger(&mut self, name: &str) -> Result<()> {
        self.save();
        if let Some(schema_rowid) = self.triggers.remove(&name.to_ascii_lowercase()) {
            self.schema.delete(self.pager.get_mut(), schema_rowid)?;
        }
        Ok(())
    }

    pub fn drop_index(&mut self, name: &str) -> Result<()> {
        self.save();
        let index = self
            .indexes
            .remove(&name.to_ascii_lowercase())
            .ok_or_else(|| no_such_index(name))?;
        let pager = self.pager.get_mut();
        index.tree.destroy(pager)?;
        self.schema.delete(pager, index.schema_rowid)?;
        Ok(())
    }

    /// Adds a row with the next free rowid, one past the largest, and
    /// returns that rowid.
    pub fn insert(&mut self, table: &str, row: &[Value]) -> Result<i64> {
        let rowid = self.next_rowid(table)?;
        self.put(table, rowid, row)?;
        Ok(rowid)
    }

    /// Stores `row` under `rowid`, replacing any row already there.
    pub fn put(&mut self, table: &str, rowid: i64, row: &[Value]) -> Result<()> {
        self.save();
        let pager = self.pager.get_mut();
        let table = table_mut(&mut self.tables, table)?;
        if table.tree.insert(pager, rowid, &record::encode(row))? {
            table.rows += 1;
        }
        Ok(())
    }

    /// Removes the row stored under `rowid`. Returns whether it existed.
    pub fn delete(&mut self, table: &str, rowid: i64) -> Result<bool> {
        self.save();
        let pager = self.pager.get_mut();
        let table = table_mut(&mut self.tables, table)?;
        let found = table.tree.delete(pager, rowid)?;
        if found {
            table.rows -= 1;
        }
        Ok(found)
    }

    /// The row stored under `rowid`, found by walking down the table's tree.
    pub fn get(&self, table: &str, rowid: i64) -> Result<Option<Row>> {
        let tree = self.tree(table)?;
        match tree.get(&mut self.pager.borrow_mut(), rowid)? {
            Some(bytes) => record::decode(&bytes).map(Some),
            None => Ok(None),
        }
    }

    /// One past the largest rowid in use, or 1 for an empty table, as in
    /// SQLite.
    pub fn next_rowid(&self, table: &str) -> Result<i64> {
        let tree = self.tree(table)?;
        match tree.last_key(&mut self.pager.borrow_mut())? {
            None => Ok(1),
            Some(i64::MAX) => Err(Error::Storage("no rowids left in this table".into())),
            Some(last) => Ok(last + 1),
        }
    }

    /// Adds an entry to an index.
    pub fn index_insert(&mut self, index: &str, key: IndexKey) -> Result<()> {
        if key.encoded_size() > MAX_KEY_SIZE {
            return Err(Error::Unsupported(format!(
                "index entries larger than {MAX_KEY_SIZE} bytes"
            )));
        }
        let tree = self.index_tree(index)?;
        tree.insert(self.pager.get_mut(), key, &[])?;
        Ok(())
    }

    /// Removes an entry from an index. Returns whether it was there.
    pub fn index_delete(&mut self, index: &str, key: IndexKey) -> Result<bool> {
        let tree = self.index_tree(index)?;
        tree.delete(self.pager.get_mut(), key)
    }

    pub fn row_count(&self, table: &str) -> usize {
        self.tables
            .get(&table.to_ascii_lowercase())
            .map_or(0, |t| t.rows)
    }

    /// Checks the B-tree rules of a table and all of its indexes.
    pub fn check_trees(&self, table: &str) -> Result<()> {
        let pager = &mut *self.pager.borrow_mut();
        self.tree(table)?.check(pager)?;
        let key = table.to_ascii_lowercase();
        for index in self.indexes.values().filter(|i| i.table == key) {
            index.tree.check(pager)?;
        }
        Ok(())
    }

    /// Checks that every page has exactly one use: the header, a node or
    /// overflow page of one tree, or a place on the free list. A page with
    /// two uses would be corrupted by one of them; a page with none has
    /// leaked.
    pub fn check_pages(&self) -> Result<()> {
        let pager = &mut *self.pager.borrow_mut();
        let mut used = vec![false; pager.page_count() as usize];
        used[0] = true;
        let mut pages = self.schema.check(pager)?.pages;
        for table in self.tables.values() {
            pages.extend(table.tree.check(pager)?.pages);
        }
        for index in self.indexes.values() {
            pages.extend(index.tree.check(pager)?.pages);
        }
        let free = pager.free_pages()?;
        if free.len() != pager.free_count() as usize {
            return Err(Error::Storage(format!(
                "integrity check: the header counts {} free pages, the list has {}",
                pager.free_count(),
                free.len()
            )));
        }
        for page in pages.iter().chain(&free) {
            match used.get_mut(*page as usize) {
                Some(seen) if !*seen => *seen = true,
                Some(_) => {
                    return Err(Error::Storage(format!(
                        "integrity check: page {page} is used twice"
                    )));
                }
                None => {
                    return Err(Error::Storage(format!(
                        "integrity check: page {page} is past the end"
                    )));
                }
            }
        }
        match used.iter().position(|seen| !seen) {
            Some(page) => Err(Error::Storage(format!(
                "integrity check: page {page} is not used by anything"
            ))),
            None => Ok(()),
        }
    }

    fn tree(&self, table: &str) -> Result<BTree> {
        self.tables
            .get(&table.to_ascii_lowercase())
            .map(|t| t.tree)
            .ok_or_else(|| no_such_table(table))
    }

    fn index_tree(&self, index: &str) -> Result<BTree<IndexKey>> {
        self.indexes
            .get(&index.to_ascii_lowercase())
            .map(|i| i.tree)
            .ok_or_else(|| no_such_index(index))
    }
}

/// Reads a table's rows in rowid order. It holds no borrow of the storage
/// between rows, so a plan can keep one per scan step.
pub struct TableScan {
    table: String,
    /// Where to start: the first rowid at or above this value (inclusive)
    /// or above it.
    from: Option<(Value, bool)>,
    cursor: Option<Cursor>,
}

impl TableScan {
    pub fn new(table: &str) -> TableScan {
        TableScan {
            table: table.to_string(),
            from: None,
            cursor: None,
        }
    }

    /// A scan that starts at the first rowid `>= from` (inclusive) or
    /// `> from`, found by walking down the tree rather than along every leaf
    /// before it. `from` can be any value: rowid 3 is above 2.5 and below
    /// any text.
    pub fn from_value(table: &str, from: Option<(Value, bool)>) -> TableScan {
        TableScan {
            from,
            ..TableScan::new(table)
        }
    }

    pub fn next(&mut self, storage: &Storage) -> Result<Option<Row>> {
        Ok(self.next_entry(storage)?.map(|(_, row)| row))
    }

    /// The next row and its rowid.
    pub fn next_entry(&mut self, storage: &Storage) -> Result<Option<(i64, Row)>> {
        let pager = &mut *storage.pager.borrow_mut();
        if self.cursor.is_none() {
            let tree = storage.tree(&self.table)?;
            let before = |&rowid: &i64| match &self.from {
                None => false,
                Some((from, true)) => Value::Integer(rowid).compare(from).is_lt(),
                Some((from, false)) => Value::Integer(rowid).compare(from).is_le(),
            };
            self.cursor = Some(Cursor::seek(pager, tree, before)?);
        }
        let cursor = self.cursor.as_mut().expect("cursor was just set");
        match cursor.next(pager)? {
            Some((rowid, bytes)) => Ok(Some((rowid, record::decode(&bytes)?))),
            None => Ok(None),
        }
    }
}

/// Reads an index's entries in order, starting at a lower bound.
pub struct IndexCursor {
    index: String,
    /// Skip entries whose first values are below these, or equal to them
    /// when the bound is exclusive.
    from: Option<(Vec<Value>, bool)>,
    cursor: Option<Cursor<IndexKey>>,
}

impl IndexCursor {
    /// A cursor at the first entry whose leading values are `>= from`
    /// (inclusive) or `> from`, or at the first entry when `from` is `None`.
    pub fn new(index: &str, from: Option<(Vec<Value>, bool)>) -> IndexCursor {
        IndexCursor {
            index: index.to_string(),
            from,
            cursor: None,
        }
    }

    pub fn next(&mut self, storage: &Storage) -> Result<Option<IndexKey>> {
        let pager = &mut *storage.pager.borrow_mut();
        if self.cursor.is_none() {
            let tree = storage.index_tree(&self.index)?;
            let from = &self.from;
            let before = |key: &IndexKey| match from {
                None => false,
                Some((values, true)) => key.compare_prefix(values).is_lt(),
                Some((values, false)) => key.compare_prefix(values).is_le(),
            };
            self.cursor = Some(Cursor::seek(pager, tree, before)?);
        }
        let cursor = self.cursor.as_mut().expect("cursor was just set");
        Ok(cursor.next(pager)?.map(|(key, _)| key))
    }
}

fn add_schema_row(pager: &mut Pager, schema: BTree, row: &[Value]) -> Result<i64> {
    let rowid = match schema.last_key(pager)? {
        None => 1,
        Some(last) => last + 1,
    };
    schema.insert(pager, rowid, &record::encode(row))?;
    Ok(rowid)
}

fn table_mut<'a>(tables: &'a mut HashMap<String, Table>, name: &str) -> Result<&'a mut Table> {
    tables
        .get_mut(&name.to_ascii_lowercase())
        .ok_or_else(|| no_such_table(name))
}

fn no_such_table(name: &str) -> Error {
    Error::Catalog(format!("no such table: {name}"))
}

fn no_such_index(name: &str) -> Error {
    Error::Catalog(format!("no such index: {name}"))
}
