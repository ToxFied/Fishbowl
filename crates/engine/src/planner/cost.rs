//! The cost model: how many rows a step will produce, and how much work it
//! takes, in units of "rows read".
//!
//! The numbers are SQLite's, from its query planner (`where.c`), simplified
//! to a few terms. Copying them keeps Fishbowl choosing the plans SQLite
//! chooses, which matters for more than speed: when `2` and `2.0` fall in
//! the same group, the one printed is the one read first, so the order an
//! index returns rows in can change an answer.
//!
//! Without statistics, the guesses are SQLite's too: every table has about a
//! million rows and an equality on an index matches about 10 of them. They
//! do not depend on what the tables hold, so a query's plan depends only on
//! its SQL and the schema. `ANALYZE` replaces them with counts.

use crate::catalog::{IndexSchema, TableSchema, TableStats};

/// Rows assumed in a table that `ANALYZE` has not counted (SQLite: 2^20).
pub const DEFAULT_ROWS: f64 = 1_048_576.0;

/// Reading one row of a table, in a full scan or by its rowid. SQLite
/// charges about 3 so that a scan has to be clearly cheaper than an index
/// before it wins: an index's worst case is better when guesses are wrong.
pub const TABLE_ROW: f64 = 3.0;

/// Handing out a row that a Materialize step already holds in memory.
pub const REPLAY_ROW: f64 = 1.0;

/// SQLite adds about a quarter to the cost of any plan that has to sort,
/// to prefer one whose rows already come out in order.
pub const SORT_PENALTY: f64 = 1.23;

/// Rows sharing one value of an index's first k columns, before `ANALYZE`:
/// 10 for one column, then 9, 8, 7, 6, and 5 from the sixth on.
fn default_rows_per_key(k: usize) -> f64 {
    [10.0, 9.0, 8.0, 7.0, 6.0]
        .get(k - 1)
        .copied()
        .unwrap_or(5.0)
}

/// Finding a key in a B-tree of `rows` entries: about log2(rows) steps.
pub fn seek(rows: f64) -> f64 {
    rows.log2().max(1.0)
}

/// Sorting `rows` rows of `columns` result columns: about 2 n log2(n), more
/// for very wide rows.
pub fn sort(rows: f64, columns: usize) -> f64 {
    let width = ((columns + 59) / 30) as f64;
    rows.max(1.0) * width * seek(rows)
}

/// How a table's rows are counted: by `ANALYZE`, or guessed, and how wide
/// its rows and index entries are.
#[derive(Clone, Copy)]
pub struct Sizes<'a> {
    pub table: &'a TableSchema,
    pub stats: Option<&'a TableStats>,
    widths: Widths,
}

/// Row widths on SQLite's log scale: the table's, and each index's (up to
/// the first 16; any more are measured when asked).
#[derive(Clone, Copy)]
struct Widths {
    table: i16,
    indexes: [i16; 16],
}

impl<'a> Sizes<'a> {
    pub fn new(table: &'a TableSchema, stats: Option<&'a TableStats>) -> Sizes<'a> {
        let mut indexes = [0; 16];
        for (w, index) in indexes.iter_mut().zip(&table.indexes) {
            *w = index_width(table, index);
        }
        Sizes {
            table,
            stats,
            widths: Widths {
                table: table_width(table),
                indexes,
            },
        }
    }

    pub fn rows(&self) -> f64 {
        self.stats.map_or(DEFAULT_ROWS, |s| s.rows as f64)
    }

    /// Rows that share one set of values in the index's first `k` columns.
    pub fn rows_per_key(&self, index: &IndexSchema, k: usize) -> f64 {
        if index.unique && k == index.columns.len() {
            return 1.0;
        }
        match self.stats {
            Some(stats) => stats
                .indexes
                .get(&index.name.to_ascii_lowercase())
                .and_then(|per_key| per_key.get(k - 1).copied())
                .unwrap_or_else(|| self.rows()),
            None => default_rows_per_key(k),
        }
    }

    /// Rows that share one value of `column`, for a condition no index
    /// serves: from `ANALYZE`'s count of distinct values, or the index guess.
    pub fn rows_per_value(&self, column: usize) -> f64 {
        if self.table.rowid_alias == Some(column) {
            return 1.0;
        }
        if let Some(index) = self
            .table
            .indexes
            .iter()
            .find(|i| i.unique && i.columns.len() == 1 && i.columns[0].position == column)
        {
            return self.rows_per_key(index, 1);
        }
        match self.stats {
            Some(stats) => {
                let distinct = stats.distinct.get(column).copied().unwrap_or(1).max(1);
                self.rows() / distinct as f64
            }
            None => default_rows_per_key(1),
        }
    }

    /// Reading one entry of `index`: between about 1 and 3, against 3 for
    /// a table row. A narrow entry packs more to a page, so it is cheaper.
    /// This is SQLite's formula, with row widths guessed from the declared
    /// column types on SQLite's log scale.
    pub fn entry(&self, index: &IndexSchema) -> f64 {
        let (index, table) = (self.index_width(index), self.widths.table);
        2f64.powf((1 + 15 * i32::from(index) / i32::from(table)) as f64 / 10.0)
    }

    /// Whether an index entry is narrower than a table row, on SQLite's
    /// scale. Only then does reading the whole index beat reading the table.
    pub fn narrower(&self, index: &IndexSchema) -> bool {
        self.index_width(index) < self.widths.table
    }

    fn index_width(&self, index: &IndexSchema) -> i16 {
        match self
            .table
            .indexes
            .iter()
            .position(|i| std::ptr::eq(i, index))
        {
            Some(i) if i < 16 => self.widths.indexes[i],
            _ => index_width(self.table, index),
        }
    }
}

fn table_width(table: &TableSchema) -> i16 {
    let mut width: u64 = table
        .columns
        .iter()
        .map(|c| size_estimate(&c.declared_type))
        .sum();
    if table.rowid_alias.is_none() {
        width += 1;
    }
    log_est(width * 4)
}

fn index_width(table: &TableSchema, index: &IndexSchema) -> i16 {
    let columns: u64 = index
        .columns
        .iter()
        .map(|c| size_estimate(&table.columns[c.position].declared_type))
        .sum();
    // Every entry also holds the rowid.
    log_est((columns + 1) * 4)
}

/// SQLite's guess at a column's size in 4-byte units, from its declared
/// type: 1 for numbers and untyped columns, 5 for TEXT and BLOB, and k/4 + 1
/// for `VARCHAR(k)` and the like.
fn size_estimate(declared: &str) -> u64 {
    if declared.is_empty() {
        return 1;
    }
    #[derive(PartialEq, PartialOrd)]
    enum Kind {
        Blob,
        Text,
        Numeric,
        Real,
    }
    let bytes = declared.as_bytes();
    let (mut hash, mut kind, mut sized_from) = (0u32, Kind::Numeric, None);
    for (i, b) in bytes.iter().enumerate() {
        hash = (hash << 8) | u32::from(b.to_ascii_lowercase());
        let word = |w: &[u8; 4]| hash == u32::from_be_bytes(*w);
        if word(b"char") {
            kind = Kind::Text;
            sized_from = Some(i + 1);
        } else if word(b"clob") || word(b"text") {
            kind = Kind::Text;
        } else if word(b"blob") && matches!(kind, Kind::Numeric | Kind::Real) {
            kind = Kind::Blob;
            if bytes.get(i + 1) == Some(&b'(') {
                sized_from = Some(i + 1);
            }
        } else if (word(b"real") || word(b"floa") || word(b"doub")) && kind == Kind::Numeric {
            kind = Kind::Real;
        } else if hash & 0x00ff_ffff == u32::from_be_bytes(*b"\0int") {
            return 1;
        }
    }
    let v: u64 = if kind < Kind::Numeric {
        match sized_from {
            Some(from) => bytes[from..]
                .iter()
                .skip_while(|b| !b.is_ascii_digit())
                .take_while(|b| b.is_ascii_digit())
                .fold(0u64, |n, d| (n * 10 + u64::from(d - b'0')).min(1 << 31)),
            None => 16,
        }
    } else {
        0
    };
    (v / 4 + 1).min(255)
}

/// SQLite's "LogEst": about 10 * log2(x), as a small integer.
fn log_est(x: u64) -> i16 {
    const A: [i16; 8] = [0, 2, 3, 5, 6, 7, 8, 9];
    if x < 2 {
        return 0;
    }
    let (mut x, mut y) = (x, 40i16);
    if x < 8 {
        while x < 8 {
            y -= 10;
            x <<= 1;
        }
    } else {
        let i = 60 - x.leading_zeros() as i16;
        y += i * 10;
        x >>= i;
    }
    A[(x & 7) as usize] + y - 10
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_est_matches_sqlite() {
        // Values from sqlite3LogEst.
        for (x, expected) in [
            (1, 0),
            (2, 10),
            (4, 20),
            (8, 30),
            (10, 33),
            (28, 48),
            (32, 50),
        ] {
            assert_eq!(log_est(x), expected, "{x}");
        }
        assert_eq!(log_est(1_048_576), 200);
    }

    #[test]
    fn column_sizes_follow_sqlite() {
        for (declared, size) in [
            ("", 1),
            ("INTEGER", 1),
            ("REAL", 1),
            ("TEXT", 5),
            ("BLOB", 5),
            ("VARCHAR(8)", 3),
            ("CHAR", 1),
            ("FLOAT", 1),
            ("NUMERIC", 1),
        ] {
            assert_eq!(size_estimate(declared), size, "{declared}");
        }
    }
}
