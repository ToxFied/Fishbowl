//! Writing rows: rowids, index entries, constraint checks, and undoing a
//! statement that fails halfway.
//!
//! Every change to a row is also a change to each of the table's indexes:
//! inserting a row adds one entry per index, deleting removes them, and an
//! update removes the old entries and adds new ones. [`TableWriter`] does
//! both together, so an index never disagrees with its table.
//!
//! Constraints are checked one row at a time, against the table as it is at
//! that moment, the way SQLite does. If row 3 of an INSERT breaks a UNIQUE
//! rule, rows 1 and 2 are already stored. So the writer keeps an undo log
//! and, when the statement fails, reverses each change, newest first. The
//! statement then leaves no trace: all or nothing.
//!
//! `INSERT OR <rule>` changes what a broken constraint does; see
//! [`OnConflict`].

use crate::catalog::{IndexSchema, TableSchema};
use crate::error::{Error, Result};
use crate::storage::index::{IndexKey, MAX_KEY_SIZE};
use crate::storage::page::Key;
use crate::storage::{IndexCursor, Row, Storage, TableScan};
use crate::value::Value;

/// What an INSERT does when a row breaks a constraint, as in SQLite's
/// `INSERT OR <rule>`. Without transactions (M6), `OR ROLLBACK` is `Abort`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnConflict {
    /// Fail the statement and undo all of its changes. The default.
    #[default]
    Abort,
    /// Fail the statement but keep the rows it already wrote. Only a broken
    /// constraint fails this way; other errors undo the statement.
    Fail,
    /// Skip the row that breaks the rule and carry on.
    Ignore,
    /// Delete the rows whose keys the new row clashes with, then store it.
    Replace,
}

/// One change, with what is needed to reverse it.
enum Undo {
    Inserted(i64, Row),
    Deleted(i64, Row),
    Updated { old: (i64, Row), new: (i64, Row) },
}

pub struct TableWriter<'a> {
    schema: &'a TableSchema,
    storage: &'a mut Storage,
    on_conflict: OnConflict,
    undo: Vec<Undo>,
}

impl<'a> TableWriter<'a> {
    pub fn new(schema: &'a TableSchema, storage: &'a mut Storage, on_conflict: OnConflict) -> Self {
        TableWriter {
            schema,
            storage,
            on_conflict,
            undo: Vec::new(),
        }
    }

    /// Ends the statement: keeps every change if it succeeded, reverses
    /// every change if it failed. Under `OR FAIL`, a broken constraint keeps
    /// the earlier changes, but any other error still reverses them.
    pub fn finish<T>(mut self, result: Result<T>) -> Result<T> {
        let keep =
            self.on_conflict == OnConflict::Fail && matches!(result, Err(Error::Constraint(_)));
        if result.is_err() && !keep {
            while let Some(change) = self.undo.pop() {
                match change {
                    Undo::Inserted(rowid, row) => self.remove(rowid, &row)?,
                    Undo::Deleted(rowid, row) => self.put(rowid, &row)?,
                    Undo::Updated { old, new } => {
                        self.remove(new.0, &new.1)?;
                        self.put(old.0, &old.1)?;
                    }
                }
            }
        }
        result
    }

    /// Stores a new row. Its values already have the columns' affinity.
    /// Returns whether it was stored: `OR IGNORE` can skip it.
    pub fn insert(&mut self, mut row: Row) -> Result<bool> {
        let rowid = match self.schema.rowid_alias {
            // An INTEGER PRIMARY KEY left NULL gets the next rowid.
            Some(pk) if row[pk].is_null() => self.storage.next_rowid(&self.schema.name)?,
            Some(pk) => rowid_value(&row[pk])?,
            None => self.storage.next_rowid(&self.schema.name)?,
        };
        if let Some(pk) = self.schema.rowid_alias {
            row[pk] = Value::Integer(rowid);
        }
        if self.on_conflict == OnConflict::Replace {
            for (victim, _) in self.clashes(rowid, &row, None)? {
                if let Some(old) = self.storage.get(&self.schema.name, victim)? {
                    self.delete(victim, old)?;
                }
            }
        }
        match self.check(rowid, &row, None) {
            Err(Error::Constraint(_)) if self.on_conflict == OnConflict::Ignore => {
                return Ok(false);
            }
            checked => checked?,
        }
        self.put(rowid, &row)?;
        self.undo.push(Undo::Inserted(rowid, row));
        Ok(true)
    }

    /// Replaces the row at `rowid`. Changing the INTEGER PRIMARY KEY moves
    /// the row to a new rowid.
    pub fn update(&mut self, rowid: i64, old: Row, new: Row) -> Result<()> {
        let new_rowid = match self.schema.rowid_alias {
            Some(pk) => rowid_value(&new[pk])?,
            None => rowid,
        };
        self.check(new_rowid, &new, Some(rowid))?;
        self.remove(rowid, &old)?;
        self.put(new_rowid, &new)?;
        self.undo.push(Undo::Updated {
            old: (rowid, old),
            new: (new_rowid, new),
        });
        Ok(())
    }

    pub fn delete(&mut self, rowid: i64, old: Row) -> Result<()> {
        self.remove(rowid, &old)?;
        self.undo.push(Undo::Deleted(rowid, old));
        Ok(())
    }

    /// Checks every rule `row` must keep to be stored at `rowid`. `replacing`
    /// is the row's current rowid during an UPDATE, so the row does not
    /// clash with itself. Nothing is written until every check passes.
    fn check(&self, rowid: i64, row: &[Value], replacing: Option<i64>) -> Result<()> {
        let schema = self.schema;
        for (column, value) in schema.columns.iter().zip(row) {
            if column.not_null && value.is_null() {
                return Err(Error::Constraint(format!(
                    "NOT NULL constraint failed: {}.{}",
                    schema.name, column.name
                )));
            }
        }
        for index in &schema.indexes {
            if index_key(index, rowid, row).encoded_size() > MAX_KEY_SIZE {
                return Err(Error::Unsupported(format!(
                    "index entries larger than {MAX_KEY_SIZE} bytes"
                )));
            }
        }
        match self.clashes(rowid, row, replacing)?.first() {
            Some((_, columns)) => Err(unique_failed(schema, columns)),
            None => Ok(()),
        }
    }

    /// The other rows that already hold this row's rowid or one of its
    /// unique keys, with the columns of the key they share. NULL is unlike
    /// every value, even NULL, so a key with a NULL never clashes.
    fn clashes(
        &self,
        rowid: i64,
        row: &[Value],
        replacing: Option<i64>,
    ) -> Result<Vec<(i64, Vec<usize>)>> {
        let schema = self.schema;
        let mut found = Vec::new();
        if let Some(pk) = schema.rowid_alias
            && replacing != Some(rowid)
            && self.storage.get(&schema.name, rowid)?.is_some()
        {
            found.push((rowid, vec![pk]));
        }
        for index in schema.indexes.iter().filter(|i| i.unique) {
            let values = index_key(index, rowid, row).values;
            if values.iter().any(Value::is_null) {
                continue;
            }
            let mut cursor = IndexCursor::new(&index.name, Some((values.clone(), true)));
            while let Some(entry) = cursor.next(self.storage)? {
                if entry.compare_prefix(&values).is_ne() {
                    break;
                }
                if Some(entry.rowid) != replacing {
                    found.push((
                        entry.rowid,
                        index.columns.iter().map(|c| c.position).collect(),
                    ));
                }
            }
        }
        Ok(found)
    }

    /// Stores a row and its index entries.
    fn put(&mut self, rowid: i64, row: &[Value]) -> Result<()> {
        self.storage.put(&self.schema.name, rowid, row)?;
        for index in &self.schema.indexes {
            self.storage
                .index_insert(&index.name, index_key(index, rowid, row))?;
        }
        Ok(())
    }

    /// Removes a row and its index entries.
    fn remove(&mut self, rowid: i64, row: &[Value]) -> Result<()> {
        self.storage.delete(&self.schema.name, rowid)?;
        for index in &self.schema.indexes {
            self.storage
                .index_delete(&index.name, index_key(index, rowid, row))?;
        }
        Ok(())
    }
}

/// Fills a new index from the rows its table already holds. A unique index
/// fails if two rows already share a key.
pub fn build_index(storage: &mut Storage, schema: &TableSchema, index: &IndexSchema) -> Result<()> {
    let mut keys = Vec::new();
    let mut scan = TableScan::new(&schema.name);
    while let Some((rowid, row)) = scan.next_entry(storage)? {
        keys.push(index_key(index, rowid, &row));
    }
    keys.sort();
    if index.unique {
        let clash = keys.windows(2).any(|pair| {
            !pair[0].values.iter().any(Value::is_null)
                && pair[0].compare_prefix(&pair[1].values).is_eq()
        });
        if clash {
            let columns: Vec<usize> = index.columns.iter().map(|c| c.position).collect();
            return Err(unique_failed(schema, &columns));
        }
    }
    for key in keys {
        storage.index_insert(&index.name, key)?;
    }
    Ok(())
}

/// A row's entry in an index: its values in the indexed columns, then its
/// rowid.
pub fn index_key(index: &IndexSchema, rowid: i64, row: &[Value]) -> IndexKey {
    IndexKey {
        values: index
            .columns
            .iter()
            .map(|c| row[c.position].clone())
            .collect(),
        rowid,
    }
}

/// An INTEGER PRIMARY KEY holds the rowid, so it must be an integer. Text
/// like `'5'` was already converted by the column's affinity.
fn rowid_value(value: &Value) -> Result<i64> {
    match value {
        Value::Integer(i) => Ok(*i),
        _ => Err(Error::Runtime("datatype mismatch".into())),
    }
}

fn unique_failed(schema: &TableSchema, columns: &[usize]) -> Error {
    Error::Constraint(format!(
        "UNIQUE constraint failed: {}",
        schema.qualified_columns(columns.iter().copied())
    ))
}
