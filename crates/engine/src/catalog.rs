//! The catalog: what tables exist and what columns they have.

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
}

#[derive(Debug, Clone, Serialize)]
pub struct TableSchema {
    pub name: String,
    pub columns: Vec<Column>,
}

impl TableSchema {
    /// Finds a column by name. SQL identifiers are case-insensitive.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
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
}
