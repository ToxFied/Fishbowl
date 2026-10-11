//! Planning for keys and indexes: `PRIMARY KEY` and `UNIQUE` in
//! `CREATE TABLE`, `CREATE INDEX` and `DROP INDEX`.
//!
//! Each key becomes something storage can check quickly:
//!
//! - An `INTEGER PRIMARY KEY` column becomes the rowid, as in SQLite. The
//!   table's own B-tree is sorted by it, so it needs no index of its own.
//! - Any other `PRIMARY KEY`, and every `UNIQUE`, becomes a unique index
//!   named `sqlite_autoindex_<table>_<n>`, also as in SQLite. Checking the
//!   key is then a lookup in that index.

use sqlparser::ast;
use sqlparser::keywords::Keyword;
use sqlparser::tokenizer::Token;

use super::object_name;
use crate::catalog::{Catalog, Column, IndexColumn, IndexSchema, TableSchema};
use crate::error::{Error, Result, unsupported};
use crate::plan::Plan;
use crate::write::OnConflict;

/// A `PRIMARY KEY` or `UNIQUE` constraint, in the order it was written.
struct Key {
    columns: Vec<IndexColumn>,
    primary: bool,
    /// Written after a column (`a INTEGER PRIMARY KEY`) rather than as a
    /// table constraint (`PRIMARY KEY (a)`).
    on_column: bool,
}

/// Collects the keys of a `CREATE TABLE` as its columns are planned.
#[derive(Default)]
pub(super) struct Keys {
    keys: Vec<Key>,
}

impl Keys {
    /// Handles a column option other than NULL and NOT NULL. `position` is
    /// the column's position and `name` its name.
    pub(super) fn column_option(
        &mut self,
        position: usize,
        name: &str,
        option: &ast::ColumnOption,
    ) -> Result<()> {
        let column = || IndexColumn {
            position,
            name: name.to_string(),
            descending: false,
        };
        match option {
            ast::ColumnOption::PrimaryKey(key)
                if key.columns.is_empty() && key.characteristics.is_none() =>
            {
                self.keys.push(Key {
                    columns: vec![column()],
                    primary: true,
                    on_column: true,
                });
            }
            ast::ColumnOption::Unique(key)
                if key.columns.is_empty() && key.characteristics.is_none() =>
            {
                self.keys.push(Key {
                    columns: vec![column()],
                    primary: false,
                    on_column: true,
                });
            }
            // The parser leaves `PRIMARY KEY DESC` and `AUTOINCREMENT` as
            // loose keywords after the key.
            ast::ColumnOption::DialectSpecific(tokens) => match tokens.as_slice() {
                [Token::Word(w)] if matches!(w.keyword, Keyword::ASC | Keyword::DESC) => {
                    match self.keys.last_mut() {
                        Some(key) if key.on_column && key.columns[0].position == position => {
                            key.columns[0].descending = w.keyword == Keyword::DESC;
                        }
                        _ => return unsupported(format!("column constraint {option}")),
                    }
                }
                [Token::Word(w)] if w.keyword == Keyword::AUTOINCREMENT => {
                    return unsupported("AUTOINCREMENT");
                }
                _ => return unsupported(format!("column constraint {option}")),
            },
            other => return unsupported(format!("column constraint {other}")),
        }
        Ok(())
    }

    /// Adds the table constraints, marks the key columns, and returns the
    /// rowid column and the indexes the keys need.
    pub(super) fn finish(
        mut self,
        table: &str,
        columns: &mut [Column],
        constraints: &[ast::TableConstraint],
    ) -> Result<(Option<usize>, Vec<IndexSchema>)> {
        let schema = TableSchema {
            name: table.to_string(),
            columns: columns.to_vec(),
            rowid_alias: None,
            indexes: Vec::new(),
        };
        for constraint in constraints {
            let (columns, characteristics, primary) = match constraint {
                ast::TableConstraint::PrimaryKey(key) => (&key.columns, &key.characteristics, true),
                ast::TableConstraint::Unique(key) => (&key.columns, &key.characteristics, false),
                _ => return unsupported("table constraints"),
            };
            if characteristics.is_some() {
                return unsupported("table constraints");
            }
            self.keys.push(Key {
                columns: index_columns(&schema, columns)?,
                primary,
                on_column: false,
            });
        }

        let mut primary = self.keys.iter().filter(|k| k.primary);
        let primary_key = primary.next();
        if primary.next().is_some() {
            return Err(Error::Catalog(format!(
                "table \"{table}\" has more than one primary key"
            )));
        }
        // SQLite's rule, quirks included: a lone column declared exactly
        // INTEGER is the rowid, unless it says `PRIMARY KEY DESC` itself.
        let rowid_alias = match primary_key.map(|k| (k, k.columns.as_slice())) {
            Some((key, [column]))
                if columns[column.position]
                    .declared_type
                    .eq_ignore_ascii_case("INTEGER")
                    && !(key.on_column && column.descending) =>
            {
                Some(column.position)
            }
            _ => None,
        };
        for column in primary_key.iter().flat_map(|k| &k.columns) {
            columns[column.position].primary_key = true;
        }

        let mut indexes: Vec<IndexSchema> = Vec::new();
        for key in self.keys {
            let positions: Vec<usize> = key.columns.iter().map(|c| c.position).collect();
            let is_rowid = key.primary && rowid_alias.is_some();
            // A second key on the same columns would only repeat the first.
            let repeated = indexes.iter().any(|i| {
                i.columns
                    .iter()
                    .map(|c| c.position)
                    .eq(positions.iter().copied())
            });
            if is_rowid || repeated {
                continue;
            }
            indexes.push(IndexSchema {
                name: format!("sqlite_autoindex_{table}_{}", indexes.len() + 1),
                columns: key.columns,
                unique: true,
                automatic: true,
            });
        }
        Ok((rowid_alias, indexes))
    }
}

/// Resolves the columns of an index or key: plain column names, each
/// optionally ASC or DESC.
fn index_columns(table: &TableSchema, columns: &[ast::IndexColumn]) -> Result<Vec<IndexColumn>> {
    columns
        .iter()
        .map(|c| {
            let ast::Expr::Identifier(ident) = &c.column.expr else {
                return unsupported("indexes on expressions");
            };
            if c.operator_class.is_some()
                || c.column.with_fill.is_some()
                || c.column.options.nulls_first.is_some()
                || matches!(c.column.options.sort, Some(ast::OrderBySort::Using(_)))
            {
                return unsupported("this index column option");
            }
            let position = table
                .column_index(&ident.value)
                .ok_or_else(|| Error::Catalog(format!("no such column: {}", ident.value)))?;
            Ok(IndexColumn {
                position,
                name: table.columns[position].name.clone(),
                descending: matches!(c.column.options.sort, Some(ast::OrderBySort::Desc)),
            })
        })
        .collect()
}

pub(super) fn plan_create_index(catalog: &Catalog, create: &ast::CreateIndex) -> Result<Plan> {
    if create.using.is_some()
        || create.concurrently
        || create.r#async
        || !create.include.is_empty()
        || create.nulls_distinct.is_some()
        || !create.with.is_empty()
        || !create.index_options.is_empty()
        || !create.alter_options.is_empty()
    {
        return unsupported("this CREATE INDEX option");
    }
    if create.predicate.is_some() {
        return unsupported("partial indexes (CREATE INDEX ... WHERE)");
    }
    let Some(name) = &create.name else {
        return Err(Error::Parse("CREATE INDEX needs a name".into()));
    };
    let name = object_name(name)?;
    if name.to_ascii_lowercase().starts_with("sqlite_") {
        return Err(Error::Catalog(format!(
            "object name reserved for internal use: {name}"
        )));
    }
    let schema = catalog.table(&object_name(&create.table_name)?)?;
    Ok(Plan::CreateIndex {
        table: schema.name.clone(),
        index: IndexSchema {
            name,
            columns: index_columns(schema, &create.columns)?,
            unique: create.unique,
            automatic: false,
        },
        if_not_exists: create.if_not_exists,
    })
}

pub(super) fn plan_drop_index(if_exists: bool, names: &[ast::ObjectName]) -> Result<Plan> {
    let [name] = names else {
        return unsupported("DROP INDEX of several indexes");
    };
    Ok(Plan::DropIndex {
        name: object_name(name)?,
        if_exists,
    })
}

/// The rule after `INSERT OR`. `REPLACE INTO` is `INSERT OR REPLACE INTO`.
/// `OR ROLLBACK` undoes the statement like `ABORT`, and then `Database`
/// rolls back the rest of the transaction too.
pub(super) fn on_conflict(or: Option<ast::SqliteOnConflict>) -> OnConflict {
    match or {
        None | Some(ast::SqliteOnConflict::Abort | ast::SqliteOnConflict::Rollback) => {
            OnConflict::Abort
        }
        Some(ast::SqliteOnConflict::Fail) => OnConflict::Fail,
        Some(ast::SqliteOnConflict::Ignore) => OnConflict::Ignore,
        Some(ast::SqliteOnConflict::Replace) => OnConflict::Replace,
    }
}
