//! Storage: where rows live between statements.
//!
//! From the bottom up:
//!
//! - [`store`]: a [`PageStore`] reads and writes numbered 4 KiB pages, in a
//!   file on native builds or in memory in the browser.
//! - [`pager`]: the [`Pager`] caches pages in a small buffer pool and hands
//!   out free pages.
//! - [`page`]: the byte layout of every kind of page.
//! - [`btree`]: each table is a B+tree of rows keyed by rowid.
//! - [`record`]: how one row of values becomes bytes.
//!
//! This module ties them together. Like SQLite's `sqlite_schema`, a schema
//! table at page 1 lists every table with its root page and the
//! `CREATE TABLE` statement that made it, so a database file can be reopened.

pub mod btree;
pub mod page;
pub mod pager;
pub mod record;
pub mod store;

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;

use crate::error::{Error, Result};
use crate::value::Value;
use btree::{BTree, Cursor};
use page::PageId;
use pager::{DEFAULT_POOL_PAGES, Pager};
pub use store::{MemoryStore, PageStore};

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
}

struct Table {
    tree: BTree,
    /// The table's row in the schema table.
    schema_rowid: i64,
    rows: usize,
}

impl Storage {
    /// A new database held in memory.
    pub fn memory() -> Storage {
        let (storage, _) = Storage::open(Box::new(MemoryStore::default()))
            .expect("creating an empty in-memory database touches no I/O");
        storage
    }

    /// Opens a store, creating a new database if it is empty. Also returns
    /// the `CREATE TABLE` statements of the tables it already holds, oldest
    /// first, so the caller can rebuild its catalog.
    pub fn open(store: Box<dyn PageStore>) -> Result<(Storage, Vec<String>)> {
        let mut pager = Pager::open(store, DEFAULT_POOL_PAGES)?;
        let schema = if pager.page_count() == 1 {
            let schema = BTree::create(&mut pager)?;
            debug_assert_eq!(schema.root(), SCHEMA_ROOT);
            pager.flush()?;
            schema
        } else {
            BTree::open(SCHEMA_ROOT)
        };

        let mut tables = HashMap::new();
        let mut statements = Vec::new();
        let mut cursor = Cursor::new(&mut pager, schema)?;
        while let Some((schema_rowid, bytes)) = cursor.next(&mut pager)? {
            let row = record::decode(&bytes)?;
            let [Value::Text(name), Value::Integer(root), Value::Text(sql)] = row.as_slice() else {
                return Err(Error::Storage(format!(
                    "schema row {schema_rowid} is damaged"
                )));
            };
            let root = PageId::try_from(*root)
                .map_err(|_| Error::Storage(format!("table {name} has a bad root page")))?;
            let tree = BTree::open(root);
            let rows = tree.count(&mut pager)?;
            tables.insert(
                name.to_ascii_lowercase(),
                Table {
                    tree,
                    schema_rowid,
                    rows,
                },
            );
            statements.push(sql.clone());
        }
        let storage = Storage {
            pager: RefCell::new(pager),
            schema,
            tables,
        };
        Ok((storage, statements))
    }

    /// Creates an empty table. `sql` is recorded so the table can be rebuilt
    /// when the file is reopened.
    pub fn create_table(&mut self, name: &str, sql: &str) -> Result<()> {
        let pager = self.pager.get_mut();
        let tree = BTree::create(pager)?;
        let schema_rowid = next_rowid(pager, self.schema)?;
        let row = [
            Value::Text(name.to_string()),
            Value::Integer(tree.root().into()),
            Value::Text(sql.to_string()),
        ];
        self.schema
            .insert(pager, schema_rowid, &record::encode(&row))?;
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

    /// Deletes a table and returns all of its pages to the free list.
    pub fn drop_table(&mut self, name: &str) -> Result<()> {
        let table = self
            .tables
            .remove(&name.to_ascii_lowercase())
            .ok_or_else(|| no_such_table(name))?;
        let pager = self.pager.get_mut();
        table.tree.destroy(pager)?;
        self.schema.delete(pager, table.schema_rowid)?;
        Ok(())
    }

    /// Adds a row with the next free rowid, one past the largest, and
    /// returns that rowid.
    pub fn insert(&mut self, table: &str, row: &[Value]) -> Result<i64> {
        let pager = self.pager.get_mut();
        let table = table_mut(&mut self.tables, table)?;
        let rowid = next_rowid(pager, table.tree)?;
        table.tree.insert(pager, rowid, &record::encode(row))?;
        table.rows += 1;
        Ok(rowid)
    }

    /// Replaces the row stored under `rowid`.
    pub fn update(&mut self, table: &str, rowid: i64, row: &[Value]) -> Result<()> {
        let pager = self.pager.get_mut();
        let table = table_mut(&mut self.tables, table)?;
        if table.tree.insert(pager, rowid, &record::encode(row))? {
            table.rows += 1;
        }
        Ok(())
    }

    /// Removes the row stored under `rowid`. Returns whether it existed.
    pub fn delete(&mut self, table: &str, rowid: i64) -> Result<bool> {
        let pager = self.pager.get_mut();
        let table = table_mut(&mut self.tables, table)?;
        let found = table.tree.delete(pager, rowid)?;
        if found {
            table.rows -= 1;
        }
        Ok(found)
    }

    pub fn row_count(&self, table: &str) -> usize {
        self.tables
            .get(&table.to_ascii_lowercase())
            .map_or(0, |t| t.rows)
    }

    /// Writes every changed page to the store.
    pub fn flush(&mut self) -> Result<()> {
        self.pager.get_mut().flush()
    }

    fn tree(&self, table: &str) -> Result<BTree> {
        self.tables
            .get(&table.to_ascii_lowercase())
            .map(|t| t.tree)
            .ok_or_else(|| no_such_table(table))
    }
}

/// Reads a table's rows in rowid order. It holds no borrow of the storage
/// between rows, so a plan can keep one per scan step.
pub struct TableScan {
    table: String,
    cursor: Option<Cursor>,
}

impl TableScan {
    pub fn new(table: &str) -> TableScan {
        TableScan {
            table: table.to_string(),
            cursor: None,
        }
    }

    pub fn next(&mut self, storage: &Storage) -> Result<Option<Row>> {
        Ok(self.next_entry(storage)?.map(|(_, row)| row))
    }

    /// The next row and its rowid.
    pub fn next_entry(&mut self, storage: &Storage) -> Result<Option<(i64, Row)>> {
        let pager = &mut *storage.pager.borrow_mut();
        if self.cursor.is_none() {
            self.cursor = Some(Cursor::new(pager, storage.tree(&self.table)?)?);
        }
        let cursor = self.cursor.as_mut().expect("cursor was just set");
        match cursor.next(pager)? {
            Some((rowid, bytes)) => Ok(Some((rowid, record::decode(&bytes)?))),
            None => Ok(None),
        }
    }
}

fn table_mut<'a>(tables: &'a mut HashMap<String, Table>, name: &str) -> Result<&'a mut Table> {
    tables
        .get_mut(&name.to_ascii_lowercase())
        .ok_or_else(|| no_such_table(name))
}

fn no_such_table(name: &str) -> Error {
    Error::Catalog(format!("no such table: {name}"))
}

/// One past the largest rowid in use, or 1 for an empty tree, as in SQLite.
fn next_rowid(pager: &mut Pager, tree: BTree) -> Result<i64> {
    match tree.last_key(pager)? {
        None => Ok(1),
        Some(i64::MAX) => Err(Error::Storage("no rowids left in this table".into())),
        Some(last) => Ok(last + 1),
    }
}
