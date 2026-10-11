//! The catalog: what tables exist, what columns they have, and their indexes.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::error::{Error, Result};
use crate::value::Affinity;

#[derive(Debug, Clone, Serialize)]
pub struct Column {
    pub name: String,
    pub declared_type: String,
    pub affinity: Affinity,
    pub not_null: bool,
    /// Part of the table's PRIMARY KEY.
    pub primary_key: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct TableSchema {
    pub name: String,
    pub columns: Vec<Column>,
    /// The `INTEGER PRIMARY KEY` column, if there is one. Its value is the
    /// row's rowid, the key of the table's own B-tree, so it needs no index.
    #[serde(skip)]
    pub rowid_alias: Option<usize>,
    /// In the order they were made, which is the order they are checked.
    pub indexes: Vec<IndexSchema>,
}

/// An index: a B-tree of (column values, rowid) kept in step with a table.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IndexSchema {
    pub name: String,
    pub columns: Vec<IndexColumn>,
    /// No two rows may have the same values in these columns, unless one of
    /// them is NULL.
    pub unique: bool,
    /// Made for a `PRIMARY KEY` or `UNIQUE` constraint rather than by
    /// `CREATE INDEX`. Such an index goes away only with its table.
    pub automatic: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IndexColumn {
    /// The column's position in the table's rows.
    #[serde(skip)]
    pub position: usize,
    pub name: String,
    /// Recorded for display. Entries are always stored in ascending order,
    /// and a scan can walk them either way.
    pub descending: bool,
}

impl TableSchema {
    /// Finds a column by name. SQL identifiers are case-insensitive.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
    }

    /// "t.a, t.b": how SQLite names columns in a constraint error.
    pub fn qualified_columns(&self, columns: impl IntoIterator<Item = usize>) -> String {
        columns
            .into_iter()
            .map(|i| format!("{}.{}", self.name, self.columns[i].name))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Tables keyed by lowercase name, so lookups ignore case like SQLite does.
#[derive(Debug, Default)]
pub struct Catalog {
    tables: BTreeMap<String, TableSchema>,
}

impl Catalog {
    pub fn add_table(&mut self, schema: TableSchema) -> Result<()> {
        let key = schema.name.to_ascii_lowercase();
        if self.tables.contains_key(&key) {
            return Err(Error::Catalog(format!(
                "table {} already exists",
                schema.name
            )));
        }
        // Tables and indexes share one set of names, as in SQLite.
        if self.index(&schema.name).is_some() {
            return Err(Error::Catalog(format!(
                "there is already an index named {}",
                schema.name
            )));
        }
        self.tables.insert(key, schema);
        Ok(())
    }

    pub fn remove_table(&mut self, name: &str) -> Option<TableSchema> {
        self.tables.remove(&name.to_ascii_lowercase())
    }

    pub fn table(&self, name: &str) -> Result<&TableSchema> {
        self.tables
            .get(&name.to_ascii_lowercase())
            .ok_or_else(|| Error::Catalog(format!("no such table: {name}")))
    }

    pub fn tables(&self) -> impl Iterator<Item = &TableSchema> {
        self.tables.values()
    }

    /// Adds an index to `table`, checking that its name is free.
    pub fn add_index(&mut self, table: &str, index: IndexSchema) -> Result<()> {
        if self.index(&index.name).is_some() {
            return Err(Error::Catalog(format!(
                "index {} already exists",
                index.name
            )));
        }
        if self.table(&index.name).is_ok() {
            return Err(Error::Catalog(format!(
                "there is already a table named {}",
                index.name
            )));
        }
        let schema = self
            .tables
            .get_mut(&table.to_ascii_lowercase())
            .ok_or_else(|| Error::Catalog(format!("no such table: {table}")))?;
        schema.indexes.push(index);
        Ok(())
    }

    /// Removes an index and returns it with the name of its table.
    pub fn remove_index(&mut self, name: &str) -> Option<(String, IndexSchema)> {
        self.tables.values_mut().find_map(|table| {
            let i = table
                .indexes
                .iter()
                .position(|index| index.name.eq_ignore_ascii_case(name))?;
            Some((table.name.clone(), table.indexes.remove(i)))
        })
    }

    /// Finds an index by name, with its table.
    pub fn index(&self, name: &str) -> Option<(&TableSchema, &IndexSchema)> {
        self.tables.values().find_map(|table| {
            let index = table
                .indexes
                .iter()
                .find(|index| index.name.eq_ignore_ascii_case(name))?;
            Some((table, index))
        })
    }
}
