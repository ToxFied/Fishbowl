//! Read-only views of a B-tree's pages, for the page viewer.
//!
//! [`Storage::inspect_tree`] decodes every page of one table or index tree,
//! top level first, and reports what each holds: its keys, the pages it
//! points to, how full it is and any overflow chains. It changes nothing.

use serde::Serialize;

use super::page::{self, Key, Node, PageId, Payload};
use super::pager::Pager;
use super::{BTree, Storage, index::IndexKey, no_such_table};
use crate::error::{Error, Result};
use crate::value::Value;

/// Trees bigger than this are cut off, so a huge table cannot freeze the UI.
pub const MAX_INSPECT_PAGES: usize = 512;

#[derive(Debug, Clone, Serialize)]
pub struct TreeInfo {
    pub name: String,
    /// `table` or `index`.
    pub kind: &'static str,
    pub root: PageId,
    /// Levels, counting the root: 1 for a tree that is a single leaf.
    pub depth: usize,
    /// The tree's pages, level by level from the root, left to right.
    pub pages: Vec<PageInfo>,
    /// The tree has more than [`MAX_INSPECT_PAGES`] pages and `pages` stops
    /// there.
    pub truncated: bool,
    /// Pages in the whole file, and how many of them are on the free list.
    pub file_pages: u32,
    pub free_pages: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct PageInfo {
    pub id: PageId,
    /// 0 for the root.
    pub level: usize,
    /// `leaf` or `interior`.
    pub kind: &'static str,
    /// Bytes in use out of [`PAGE_SIZE`](super::page::PAGE_SIZE).
    pub used: usize,
    /// A leaf's entries, or an interior page's signposts, in order.
    pub keys: Vec<KeyInfo>,
    /// An interior page's children: `children[i]` holds keys up to
    /// `keys[i]`, and the last child the rest.
    pub children: Vec<PageId>,
    /// A leaf's right neighbor.
    pub next: Option<PageId>,
    pub overflow: Vec<OverflowChain>,
}

/// A table key is a rowid. An index key is the indexed values and a rowid.
#[derive(Debug, Clone, Serialize)]
pub struct KeyInfo {
    pub values: Vec<Value>,
    pub rowid: i64,
}

/// A payload too big for its leaf, stored across overflow pages.
#[derive(Debug, Clone, Serialize)]
pub struct OverflowChain {
    /// Which of the leaf's entries it belongs to.
    pub cell: usize,
    pub bytes: u32,
    pub pages: Vec<PageId>,
}

trait Describe: Key {
    fn describe(&self) -> KeyInfo;
}

impl Describe for i64 {
    fn describe(&self) -> KeyInfo {
        KeyInfo {
            values: Vec::new(),
            rowid: *self,
        }
    }
}

impl Describe for IndexKey {
    fn describe(&self) -> KeyInfo {
        KeyInfo {
            values: self.values.clone(),
            rowid: self.rowid,
        }
    }
}

impl Storage {
    /// Every page of the B-tree behind a table or an index, by name.
    pub fn inspect_tree(&self, name: &str) -> Result<TreeInfo> {
        let pager = &mut *self.pager.borrow_mut();
        let (kind, root, (pages, truncated)) = if let Ok(tree) = self.tree(name) {
            ("table", tree.root(), walk(pager, tree)?)
        } else if let Ok(tree) = self.index_tree(name) {
            ("index", tree.root(), walk(pager, tree)?)
        } else {
            return Err(no_such_table(name));
        };
        Ok(TreeInfo {
            name: name.to_string(),
            kind,
            root,
            depth: pages.last().map_or(0, |p| p.level + 1),
            pages,
            truncated,
            file_pages: pager.page_count(),
            free_pages: pager.free_count(),
        })
    }
}

/// Reads a tree breadth first, so pages come out level by level.
fn walk<K: Describe>(pager: &mut Pager, tree: BTree<K>) -> Result<(Vec<PageInfo>, bool)> {
    let mut pages = Vec::new();
    let mut queue = std::collections::VecDeque::from([(tree.root(), 0)]);
    while let Some((id, level)) = queue.pop_front() {
        if pages.len() == MAX_INSPECT_PAGES {
            return Ok((pages, true));
        }
        let node = Node::<K>::decode(id, pager.page(id)?)?;
        let used = node.size();
        let page = match node {
            Node::Interior(node) => {
                queue.extend(node.children.iter().map(|&child| (child, level + 1)));
                PageInfo {
                    id,
                    level,
                    kind: "interior",
                    used,
                    keys: node.keys.iter().map(Describe::describe).collect(),
                    children: node.children,
                    next: None,
                    overflow: Vec::new(),
                }
            }
            Node::Leaf(leaf) => {
                let mut overflow = Vec::new();
                for (cell, entry) in leaf.cells.iter().enumerate() {
                    if let Payload::Overflow { len, first } = entry.payload {
                        overflow.push(OverflowChain {
                            cell,
                            bytes: len,
                            pages: chain(pager, first)?,
                        });
                    }
                }
                PageInfo {
                    id,
                    level,
                    kind: "leaf",
                    used,
                    keys: leaf.cells.iter().map(|c| c.key.describe()).collect(),
                    children: Vec::new(),
                    next: leaf.next,
                    overflow,
                }
            }
        };
        pages.push(page);
    }
    Ok((pages, false))
}

/// The overflow pages of one payload, first to last.
fn chain(pager: &mut Pager, first: PageId) -> Result<Vec<PageId>> {
    let mut pages = Vec::new();
    let mut next = Some(first);
    while let Some(id) = next {
        if pages.len() > pager.page_count() as usize {
            return Err(Error::Storage("an overflow chain loops".into()));
        }
        pages.push(id);
        next = page::decode_overflow(id, pager.page(id)?)?.1;
    }
    Ok(pages)
}
