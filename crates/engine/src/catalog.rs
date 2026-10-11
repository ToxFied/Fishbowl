//! The catalog: what tables and views exist and what columns they have.

use std::collections::BTreeMap;

use serde::Serialize;
use sqlparser::ast;

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

/// A view: a named SELECT, planned again each time a query uses it.
#[derive(Debug, Clone)]
pub struct View {
    pub name: String,
    /// Column names from `CREATE VIEW v(a, b)`. Empty means the SELECT's own.
    pub columns: Vec<String>,
    pub query: Box<ast::Query>,
}

/// Tables and views keyed by lowercase name, so lookups ignore case like
/// SQLite does. A table and a view cannot share a name.
#[derive(Debug, Default)]
pub struct Catalog {
    tables: BTreeMap<String, TableSchema>,
    views: BTreeMap<String, View>,
}

impl Catalog {
    pub fn add_table(&mut self, schema: TableSchema) -> Result<()> {
        self.check_name_free(&schema.name)?;
        self.tables.insert(schema.name.to_ascii_lowercase(), schema);
        Ok(())
    }

    pub fn add_view(&mut self, view: View) -> Result<()> {
        self.check_name_free(&view.name)?;
        self.views.insert(view.name.to_ascii_lowercase(), view);
        Ok(())
    }

    fn check_name_free(&self, name: &str) -> Result<()> {
        let key = name.to_ascii_lowercase();
        let kind = if self.tables.contains_key(&key) {
            "table"
        } else if self.views.contains_key(&key) {
            "view"
        } else {
            return Ok(());
        };
        Err(Error::Catalog(format!("{kind} {name} already exists")))
    }

    pub fn remove_view(&mut self, name: &str) -> Option<View> {
        self.views.remove(&name.to_ascii_lowercase())
    }

    pub fn view(&self, name: &str) -> Option<&View> {
        self.views.get(&name.to_ascii_lowercase())
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
