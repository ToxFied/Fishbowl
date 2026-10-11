//! Row storage. For now every table is a vector of rows in memory.
//!
//! This is a placeholder with a deliberately narrow interface (create, append,
//! read by position) so the pager and B-tree milestones can replace it without
//! touching the planner or executor.

use std::collections::HashMap;

use crate::value::Value;

pub type Row = Vec<Value>;

#[derive(Debug, Default)]
pub struct MemoryStorage {
    tables: HashMap<String, Vec<Row>>,
}

impl MemoryStorage {
    pub fn create_table(&mut self, name: &str) {
        self.tables.entry(name.to_ascii_lowercase()).or_default();
    }

    pub fn append(&mut self, table: &str, row: Row) {
        self.tables
            .entry(table.to_ascii_lowercase())
            .or_default()
            .push(row);
    }

    /// The row at `position`, or `None` past the end of the table.
    pub fn row(&self, table: &str, position: usize) -> Option<&Row> {
        self.tables
            .get(&table.to_ascii_lowercase())
            .and_then(|rows| rows.get(position))
    }

    pub fn row_count(&self, table: &str) -> usize {
        self.tables
            .get(&table.to_ascii_lowercase())
            .map_or(0, Vec::len)
    }
}
