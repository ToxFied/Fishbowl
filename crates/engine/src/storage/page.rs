//! The byte layout of every kind of page, in one place.
//!
//! A database file is a row of 4 KiB pages numbered from 0. Page 0 is the file
//! header. Every other page starts with a one-byte kind. Numbers are
//! big-endian, so a hex dump reads left to right like the number itself.
//!
//! ```text
//! Header page (page 0)
//!   0..8    magic "FISHBOWL"
//!   8..12   page size (u32, always 4096)
//!   12..16  first free page (u32, 0 = none)
//!   16..20  free page count (u32)
//!
//! Leaf page: rows, in key order
//!   0       kind = 1 (table) or 5 (index)
//!   1..3    cell count (u16)
//!   3..7    next leaf to the right (u32, 0 = this is the last leaf)
//!   7..     cells, back to back:
//!           key | payload length (u32) | payload
//!           A payload over MAX_INLINE_PAYLOAD bytes is replaced by the id of
//!           its first overflow page (u32).
//!
//! Interior page: signposts to child pages
//!   0       kind = 2 (table) or 6 (index)
//!   1..3    key count (u16)
//!   3..7    rightmost child (u32): keys above the last key
//!   7..     cells, back to back:
//!           child (u32) | key: every key in child is <= key
//!
//! A table's key is its rowid (i64). An index's key is the indexed values and
//! the rowid, as a length (u16) and a row record, and it has no payload. See
//! `index.rs`.
//!
//! Overflow page: the rest of a payload too big for a leaf
//!   0       kind = 3
//!   1..5    next overflow page (u32, 0 = last)
//!   5..7    bytes used on this page (u16)
//!   7..     payload bytes
//!
//! Free page: unused, waiting to be handed out again
//!   0       kind = 4
//!   1..5    next free page (u32, 0 = last)
//! ```

use std::fmt;

use crate::error::{Error, Result};

pub const PAGE_SIZE: usize = 4096;

/// A page number. Page `n` starts at byte `n * PAGE_SIZE` of the file.
pub type PageId = u32;

pub type Page = [u8; PAGE_SIZE];

const MAGIC: &[u8; 8] = b"FISHBOWL";

const LEAF: u8 = 1;
const INTERIOR: u8 = 2;
const OVERFLOW: u8 = 3;
const FREE: u8 = 4;
pub const INDEX_LEAF: u8 = 5;
pub const INDEX_INTERIOR: u8 = 6;

/// Every B-tree and overflow page has 7 bytes of header before its contents.
pub const NODE_HEADER: usize = 7;
/// The payload length after each leaf key.
const PAYLOAD_LEN: usize = 4;
/// The child page number before each interior key.
const CHILD: usize = 4;

/// Payloads up to this size live in the leaf. The limit keeps at least four
/// cells per leaf, which guarantees that a split leaves both halves fitting.
pub const MAX_INLINE_PAYLOAD: usize = 1000;

/// Payload bytes one overflow page holds.
pub const OVERFLOW_CAPACITY: usize = PAGE_SIZE - NODE_HEADER;

/// What page 0 records about the whole file.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub first_free: PageId,
    pub free_count: u32,
}

impl Header {
    pub fn encode(&self, page: &mut Page) {
        page.fill(0);
        let mut w = Writer::new(page);
        w.bytes(MAGIC);
        w.u32(PAGE_SIZE as u32);
        w.u32(self.first_free);
        w.u32(self.free_count);
    }

    pub fn decode(page: &Page) -> Result<Header> {
        let mut r = Reader::new(page, 0);
        if r.bytes(MAGIC.len())? != MAGIC {
            return Err(Error::Storage("not a Fishbowl database file".into()));
        }
        let page_size = r.u32()?;
        if page_size as usize != PAGE_SIZE {
            return Err(Error::Storage(format!(
                "page size is {page_size}, expected {PAGE_SIZE}"
            )));
        }
        Ok(Header {
            first_free: r.u32()?,
            free_count: r.u32()?,
        })
    }
}

/// What a B+tree is sorted by. Tables use the rowid; indexes use
/// [`IndexKey`](super::index::IndexKey).
pub trait Key: Ord + Clone + fmt::Debug {
    /// The page kinds this tree uses, so a table page is never read as an
    /// index page or the other way round.
    const LEAF: u8;
    const INTERIOR: u8;
    /// Bytes the key takes on a page.
    fn encoded_size(&self) -> usize;
    fn write(&self, w: &mut Writer);
    fn read(r: &mut Reader) -> Result<Self>;
}

impl Key for i64 {
    const LEAF: u8 = LEAF;
    const INTERIOR: u8 = INTERIOR;

    fn encoded_size(&self) -> usize {
        8
    }

    fn write(&self, w: &mut Writer) {
        w.i64(*self);
    }

    fn read(r: &mut Reader) -> Result<Self> {
        r.i64()
    }
}

/// A B+tree page, decoded. The tree code edits these and writes them back.
#[derive(Debug, Clone, PartialEq)]
pub enum Node<K = i64> {
    Leaf(Leaf<K>),
    Interior(Interior<K>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Leaf<K = i64> {
    pub cells: Vec<Cell<K>>,
    /// The leaf holding the next keys, so a scan can walk leaf to leaf.
    pub next: Option<PageId>,
}

impl<K> Default for Leaf<K> {
    fn default() -> Self {
        Leaf {
            cells: Vec::new(),
            next: None,
        }
    }
}

/// One entry in a leaf: its key and its encoded bytes. In a table the key
/// is the rowid and the payload is the row.
#[derive(Debug, Clone, PartialEq)]
pub struct Cell<K = i64> {
    pub key: K,
    pub payload: Payload,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Payload {
    Inline(Vec<u8>),
    /// Too big for the leaf: `len` bytes stored in a chain of overflow pages.
    Overflow {
        len: u32,
        first: PageId,
    },
}

/// `children[i]` holds keys `<= keys[i]` (and above `keys[i - 1]`). The last
/// child holds everything above the last key, so there is one more child than
/// there are keys.
#[derive(Debug, Clone, PartialEq)]
pub struct Interior<K = i64> {
    pub keys: Vec<K>,
    pub children: Vec<PageId>,
}

impl<K: Key> Cell<K> {
    pub fn size(&self) -> usize {
        self.key.encoded_size()
            + PAYLOAD_LEN
            + match &self.payload {
                Payload::Inline(bytes) => bytes.len(),
                Payload::Overflow { .. } => 4,
            }
    }
}

impl<K: Key> Leaf<K> {
    pub fn size(&self) -> usize {
        NODE_HEADER + self.cells.iter().map(Cell::size).sum::<usize>()
    }
}

impl<K: Key> Interior<K> {
    pub fn size(&self) -> usize {
        NODE_HEADER + self.keys.iter().map(Interior::cell_size).sum::<usize>()
    }

    /// Bytes one signpost takes: a child page number and a key.
    pub fn cell_size(key: &K) -> usize {
        CHILD + key.encoded_size()
    }

    /// Which child to follow for `key`.
    pub fn child_index(&self, key: &K) -> usize {
        self.child_index_by(|k| k < key)
    }

    /// Which child holds the first key for which `before` is false. Keys
    /// are sorted, so `before` is true for a run of keys and then false.
    pub fn child_index_by(&self, before: impl Fn(&K) -> bool) -> usize {
        self.keys.partition_point(before)
    }
}

impl<K: Key> Node<K> {
    pub fn size(&self) -> usize {
        match self {
            Node::Leaf(leaf) => leaf.size(),
            Node::Interior(interior) => interior.size(),
        }
    }

    pub fn encode(&self, page: &mut Page) {
        debug_assert!(self.size() <= PAGE_SIZE, "node does not fit its page");
        page.fill(0);
        let mut w = Writer::new(page);
        match self {
            Node::Leaf(leaf) => {
                w.u8(K::LEAF);
                w.u16(leaf.cells.len() as u16);
                w.u32(leaf.next.unwrap_or(0));
                for cell in &leaf.cells {
                    cell.key.write(&mut w);
                    match &cell.payload {
                        Payload::Inline(bytes) => {
                            w.u32(bytes.len() as u32);
                            w.bytes(bytes);
                        }
                        Payload::Overflow { len, first } => {
                            w.u32(*len);
                            w.u32(*first);
                        }
                    }
                }
            }
            Node::Interior(interior) => {
                w.u8(K::INTERIOR);
                w.u16(interior.keys.len() as u16);
                w.u32(*interior.children.last().expect("interior has a child"));
                for (child, key) in interior.children.iter().zip(&interior.keys) {
                    w.u32(*child);
                    key.write(&mut w);
                }
            }
        }
    }

    pub fn decode(id: PageId, page: &Page) -> Result<Node<K>> {
        let mut r = Reader::new(page, id);
        let kind = r.u8()?;
        let count = r.u16()? as usize;
        let pointer = r.u32()?;
        match kind {
            _ if kind == K::LEAF => {
                let mut cells = Vec::with_capacity(count);
                for _ in 0..count {
                    let key = K::read(&mut r)?;
                    let len = r.u32()?;
                    let payload = if len as usize <= MAX_INLINE_PAYLOAD {
                        Payload::Inline(r.bytes(len as usize)?.to_vec())
                    } else {
                        Payload::Overflow {
                            len,
                            first: r.u32()?,
                        }
                    };
                    cells.push(Cell { key, payload });
                }
                Ok(Node::Leaf(Leaf {
                    cells,
                    next: (pointer != 0).then_some(pointer),
                }))
            }
            _ if kind == K::INTERIOR => {
                let mut keys = Vec::with_capacity(count);
                let mut children = Vec::with_capacity(count + 1);
                for _ in 0..count {
                    children.push(r.u32()?);
                    keys.push(K::read(&mut r)?);
                }
                children.push(pointer);
                Ok(Node::Interior(Interior { keys, children }))
            }
            other => Err(r.corrupt(&format!(
                "expected a B-tree page of kind {} or {}, found kind {other}",
                K::LEAF,
                K::INTERIOR
            ))),
        }
    }
}

/// Writes one overflow page: up to [`OVERFLOW_CAPACITY`] bytes and a link.
pub fn encode_overflow(page: &mut Page, next: Option<PageId>, data: &[u8]) {
    debug_assert!(data.len() <= OVERFLOW_CAPACITY);
    page.fill(0);
    let mut w = Writer::new(page);
    w.u8(OVERFLOW);
    w.u32(next.unwrap_or(0));
    w.u16(data.len() as u16);
    w.bytes(data);
}

/// Reads one overflow page: its bytes and the next page in the chain.
pub fn decode_overflow(id: PageId, page: &Page) -> Result<(&[u8], Option<PageId>)> {
    let mut r = Reader::new(page, id);
    if r.u8()? != OVERFLOW {
        return Err(r.corrupt("expected an overflow page"));
    }
    let next = r.u32()?;
    let len = r.u16()? as usize;
    Ok((r.bytes(len)?, (next != 0).then_some(next)))
}

pub fn encode_free(page: &mut Page, next: Option<PageId>) {
    page.fill(0);
    let mut w = Writer::new(page);
    w.u8(FREE);
    w.u32(next.unwrap_or(0));
}

pub fn decode_free(id: PageId, page: &Page) -> Result<Option<PageId>> {
    let mut r = Reader::new(page, id);
    if r.u8()? != FREE {
        return Err(r.corrupt("expected a free page"));
    }
    let next = r.u32()?;
    Ok((next != 0).then_some(next))
}

/// Writes fields into a page one after another.
pub struct Writer<'a> {
    page: &'a mut Page,
    pos: usize,
}

impl<'a> Writer<'a> {
    fn new(page: &'a mut Page) -> Self {
        Writer { page, pos: 0 }
    }

    pub fn bytes(&mut self, bytes: &[u8]) {
        self.page[self.pos..self.pos + bytes.len()].copy_from_slice(bytes);
        self.pos += bytes.len();
    }

    fn u8(&mut self, v: u8) {
        self.bytes(&[v]);
    }

    pub fn u16(&mut self, v: u16) {
        self.bytes(&v.to_be_bytes());
    }

    fn u32(&mut self, v: u32) {
        self.bytes(&v.to_be_bytes());
    }

    pub fn i64(&mut self, v: i64) {
        self.bytes(&v.to_be_bytes());
    }
}

/// Reads fields in order, failing cleanly instead of panicking if a damaged
/// page claims more bytes than it has.
pub struct Reader<'a> {
    page: &'a Page,
    pos: usize,
    id: PageId,
}

impl<'a> Reader<'a> {
    fn new(page: &'a Page, id: PageId) -> Self {
        Reader { page, pos: 0, id }
    }

    pub fn corrupt(&self, what: &str) -> Error {
        Error::Storage(format!("page {} is corrupt: {what}", self.id))
    }

    pub fn bytes(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self.pos + len;
        let bytes = self
            .page
            .get(self.pos..end)
            .ok_or_else(|| self.corrupt("a field runs past the end of the page"))?;
        self.pos = end;
        Ok(bytes)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.bytes(N)?.try_into().expect("slice has length N"))
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.array::<1>()?[0])
    }

    pub fn u16(&mut self) -> Result<u16> {
        self.array().map(u16::from_be_bytes)
    }

    fn u32(&mut self) -> Result<u32> {
        self.array().map(u32::from_be_bytes)
    }

    pub fn i64(&mut self) -> Result<i64> {
        self.array().map(i64::from_be_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<K: Key>(node: &Node<K>) -> Node<K> {
        let mut page = [0; PAGE_SIZE];
        node.encode(&mut page);
        Node::decode(9, &page).unwrap()
    }

    #[test]
    fn leaf_bytes_follow_the_documented_layout() {
        let leaf = Node::Leaf(Leaf {
            cells: vec![Cell {
                key: 5,
                payload: Payload::Inline(vec![0xAA, 0xBB]),
            }],
            next: Some(3),
        });
        let mut page = [0; PAGE_SIZE];
        leaf.encode(&mut page);
        assert_eq!(
            page[..21],
            [
                1, 0, 1, 0, 0, 0, 3, // kind, one cell, next leaf 3
                0, 0, 0, 0, 0, 0, 0, 5, // rowid 5
                0, 0, 0, 2, 0xAA, 0xBB, // two payload bytes
            ]
        );
        assert_eq!(round_trip(&leaf), leaf);
    }

    #[test]
    fn nodes_round_trip() {
        let interior = Node::Interior(Interior {
            keys: vec![-4, 10, i64::MAX],
            children: vec![2, 3, 4, 5],
        });
        assert_eq!(round_trip(&interior), interior);
        let overflowing = Node::Leaf(Leaf {
            cells: vec![Cell {
                key: 1,
                payload: Payload::Overflow {
                    len: 5000,
                    first: 7,
                },
            }],
            next: None,
        });
        assert_eq!(round_trip(&overflowing), overflowing);
    }

    #[test]
    fn damaged_pages_are_errors_not_panics() {
        let mut page = [0; PAGE_SIZE];
        page[0] = LEAF;
        page[1..3].copy_from_slice(&500u16.to_be_bytes());
        page[7 + 8..7 + 12].copy_from_slice(&900u32.to_be_bytes());
        // 500 cells of 900 bytes cannot fit in one page.
        assert!(matches!(
            Node::<i64>::decode(1, &page),
            Err(Error::Storage(_))
        ));
        assert!(Node::<i64>::decode(1, &[0; PAGE_SIZE]).is_err());
        assert!(Header::decode(&[0; PAGE_SIZE]).is_err());
    }
}
