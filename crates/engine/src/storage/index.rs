//! Index keys: what an index B-tree is sorted by.
//!
//! An index on `tab1(col0)` is a second B-tree beside the table. Each table
//! row has one entry in it: the row's `col0` value and its rowid. Entries are
//! sorted by value, so finding `col0 = 42` or `col0 > 40` is a walk down the
//! tree instead of a read of every row. The rowid then leads back to the row
//! in the table's own tree.
//!
//! The rowid is part of the key, not a payload, for two reasons:
//!
//! - **Every key is distinct.** Ten rows can share `col0 = 42`, but no two
//!   share (42, rowid). The B-tree needs distinct keys to put each entry in
//!   exactly one place, and deleting one row's entry finds exactly that entry.
//! - **Equal values stay in rowid order.** All the 42s sit together, sorted
//!   by rowid, so a scan of "every 42" is one run along the leaves.
//!
//! On a page a key is a length (u16), the values as a row record (see
//! `record.rs`) and the rowid (i64). Index entries carry no payload.

use std::cmp::Ordering;

use super::page::{INDEX_INTERIOR, INDEX_LEAF, Key, Reader, Writer};
use super::record;
use crate::error::Result;
use crate::value::Value;

/// The largest key an index accepts, in bytes. Keeping keys well under a
/// quarter page means any page that splits leaves two halves that fit.
pub const MAX_KEY_SIZE: usize = 512;

/// One index entry: the indexed column values of a row, then its rowid.
#[derive(Debug, Clone)]
pub struct IndexKey {
    pub values: Vec<Value>,
    pub rowid: i64,
}

impl IndexKey {
    /// Compares this key's first values with `prefix`, ignoring the rest and
    /// the rowid. A search for `col0 >= 40` compares against `[40]`.
    pub fn compare_prefix(&self, prefix: &[Value]) -> Ordering {
        compare_values(&self.values[..prefix.len().min(self.values.len())], prefix)
    }
}

/// Values in index order: NULL first, then numbers by value, then text byte
/// by byte. This is [`Value::compare`] applied column by column.
fn compare_values(a: &[Value], b: &[Value]) -> Ordering {
    a.iter()
        .zip(b)
        .map(|(x, y)| x.compare(y))
        .find(|ord| ord.is_ne())
        .unwrap_or_else(|| a.len().cmp(&b.len()))
}

impl Ord for IndexKey {
    fn cmp(&self, other: &Self) -> Ordering {
        compare_values(&self.values, &other.values).then(self.rowid.cmp(&other.rowid))
    }
}

impl PartialOrd for IndexKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Equal when the order says so: `3` and `3.0` are the same value to an
/// index, as they are to `=`.
impl PartialEq for IndexKey {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for IndexKey {}

impl Key for IndexKey {
    const LEAF: u8 = INDEX_LEAF;
    const INTERIOR: u8 = INDEX_INTERIOR;

    fn encoded_size(&self) -> usize {
        2 + record::encoded_len(&self.values) + 8
    }

    fn write(&self, w: &mut Writer) {
        let bytes = record::encode(&self.values);
        w.u16(bytes.len() as u16);
        w.bytes(&bytes);
        w.i64(self.rowid);
    }

    fn read(r: &mut Reader) -> Result<Self> {
        let len = r.u16()? as usize;
        let values = record::decode(r.bytes(len)?)?;
        Ok(IndexKey {
            values,
            rowid: r.i64()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(values: Vec<Value>, rowid: i64) -> IndexKey {
        IndexKey { values, rowid }
    }

    #[test]
    fn keys_sort_by_value_then_rowid() {
        let mut keys = [
            key(vec![Value::Text("a".into())], 1),
            key(vec![Value::Integer(5)], 9),
            key(vec![Value::Null], 4),
            key(vec![Value::Real(2.5)], 2),
            key(vec![Value::Integer(5)], 3),
        ];
        keys.sort();
        let order: Vec<i64> = keys.iter().map(|k| k.rowid).collect();
        assert_eq!(order, [4, 2, 3, 9, 1]);
    }

    #[test]
    fn integers_and_reals_with_the_same_value_are_equal() {
        assert_eq!(
            key(vec![Value::Integer(3)], 1),
            key(vec![Value::Real(3.0)], 1)
        );
        let k = key(vec![Value::Integer(3), Value::Text("x".into())], 1);
        assert!(k.compare_prefix(&[Value::Real(3.0)]).is_eq());
        assert!(k.compare_prefix(&[Value::Integer(4)]).is_lt());
    }
}
