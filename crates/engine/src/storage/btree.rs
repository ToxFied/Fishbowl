//! B+trees keyed by rowid: how a table's rows are organized into pages.
//!
//! Rows live only in leaf pages, sorted by rowid, and each leaf links to the
//! next one so a full scan walks along the bottom of the tree. Interior pages
//! hold only signposts: "rowids up to 40 are in page 7, up to 95 in page 3".
//! Finding a row reads one page per level, and a level holds hundreds of
//! signposts, so even large tables are only three or four pages deep.
//!
//! - **Insert**: a leaf that overflows its 4 KiB splits in two, and its parent
//!   gets a new signpost. A full parent splits the same way, up to the root.
//! - **Delete**: a page that drops below half full borrows from a neighbor.
//!   If the two fit in one page they merge, and the parent loses a signpost.
//!
//! The root never moves to another page, so a table's root page number is
//! stable for its whole life. When the root splits, its contents move down to
//! a new page instead; when it has a single child left, that child moves up.
//!
//! Every function here works on decoded [`Node`]s: read a page, change the
//! vectors, write the page back. See `page.rs` for the bytes.

use super::page::{
    self, Cell, Interior, Leaf, MAX_INLINE_PAYLOAD, Node, OVERFLOW_CAPACITY, PAGE_SIZE, PageId,
    Payload,
};
use super::pager::Pager;
use crate::error::{Error, Result};

/// A page below half full is refilled from, or merged with, a neighbor.
const MIN_FILL: usize = PAGE_SIZE / 2;

/// A B+tree, identified by its root page. The pages themselves live in the
/// [`Pager`], which every operation takes as an argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BTree {
    root: PageId,
}

/// A child that split: the largest key left behind, and the new right page.
type Split = Option<(i64, PageId)>;

impl BTree {
    /// A new, empty tree: one empty leaf that is also the root.
    pub fn create(pager: &mut Pager) -> Result<BTree> {
        let root = pager.allocate()?;
        write_node(pager, root, &Node::Leaf(Leaf::default()))?;
        Ok(BTree { root })
    }

    /// A tree that already exists, rooted at `root`.
    pub fn open(root: PageId) -> BTree {
        BTree { root }
    }

    pub fn root(&self) -> PageId {
        self.root
    }

    pub fn get(&self, pager: &mut Pager, key: i64) -> Result<Option<Vec<u8>>> {
        let mut id = self.root;
        loop {
            match read_node(pager, id)? {
                Node::Interior(node) => id = node.children[node.child_index(key)],
                Node::Leaf(mut leaf) => {
                    return match leaf.cells.binary_search_by_key(&key, |c| c.rowid) {
                        Ok(i) => payload_bytes(pager, leaf.cells.swap_remove(i).payload).map(Some),
                        Err(_) => Ok(None),
                    };
                }
            }
        }
    }

    /// The largest key, found by always following the rightmost child.
    pub fn last_key(&self, pager: &mut Pager) -> Result<Option<i64>> {
        let mut id = self.root;
        loop {
            match read_node(pager, id)? {
                Node::Interior(node) => id = *node.children.last().expect("interior has a child"),
                Node::Leaf(leaf) => return Ok(leaf.cells.last().map(|c| c.rowid)),
            }
        }
    }

    /// Inserts `value` under `key`, replacing any value already there.
    /// Returns whether the key is new.
    pub fn insert(&self, pager: &mut Pager, key: i64, value: &[u8]) -> Result<bool> {
        let cell = Cell {
            rowid: key,
            payload: store_payload(pager, value)?,
        };
        let (is_new, split) = insert_into(pager, self.root, cell)?;
        if let Some((separator, right)) = split {
            // The root page now holds only the left half. Move that half to a
            // new page and turn the root into a signpost to both halves.
            let left = pager.allocate()?;
            let node = read_node(pager, self.root)?;
            write_node(pager, left, &node)?;
            let root = Interior {
                keys: vec![separator],
                children: vec![left, right],
            };
            write_node(pager, self.root, &Node::Interior(root))?;
        }
        Ok(is_new)
    }

    /// Removes `key`. Returns whether it was there.
    pub fn delete(&self, pager: &mut Pager, key: i64) -> Result<bool> {
        if delete_from(pager, self.root, key)?.is_none() {
            return Ok(false);
        }
        // A root with one child left is a level that does nothing: copy the
        // child up into the root page, making the tree one level shorter.
        if let Node::Interior(root) = read_node(pager, self.root)? {
            if root.keys.is_empty() {
                let child = root.children[0];
                let node = read_node(pager, child)?;
                write_node(pager, self.root, &node)?;
                pager.free(child)?;
            }
        }
        Ok(true)
    }

    /// Frees every page of the tree, including the root.
    pub fn destroy(self, pager: &mut Pager) -> Result<()> {
        free_subtree(pager, self.root)
    }

    /// Counts rows by walking the leaves.
    pub fn count(&self, pager: &mut Pager) -> Result<usize> {
        let mut count = 0;
        let mut next = Some(first_leaf(pager, self.root)?);
        while let Some(id) = next {
            let leaf = read_leaf(pager, id)?;
            count += leaf.cells.len();
            next = leaf.next;
        }
        Ok(count)
    }

    /// Checks every rule the tree must keep and reports its shape. Tests run
    /// this after every change.
    pub fn check(&self, pager: &mut Pager) -> Result<Shape> {
        let mut check = Check::default();
        check.walk(pager, self.root, None, None, 0)?;
        // Following the leaf links must visit the same leaves as walking the
        // tree, in the same order.
        let mut linked = Vec::new();
        let mut next = check.leaves.first().copied();
        while let Some(id) = next {
            if linked.len() > check.leaves.len() {
                return Err(broken("the leaf links form a loop"));
            }
            linked.push(id);
            next = read_leaf(pager, id)?.next;
        }
        if linked != check.leaves {
            return Err(broken("the leaf links skip or reorder leaves"));
        }
        Ok(Shape {
            pages: check.pages,
            depth: check.leaf_depth.unwrap_or(0) + 1,
            leaves: check.leaves.len(),
        })
    }
}

/// What [`BTree::check`] found.
#[derive(Debug)]
pub struct Shape {
    /// Every page the tree uses: its nodes and their overflow pages.
    pub pages: Vec<PageId>,
    /// Levels, counting the root: 1 for a tree that is a single leaf.
    pub depth: usize,
    pub leaves: usize,
}

/// Walks a tree's rows in rowid order, one leaf at a time.
pub struct Cursor {
    cells: std::vec::IntoIter<Cell>,
    next_leaf: Option<PageId>,
    /// Leaves read so far. More leaves than pages means the links loop.
    leaves: u32,
}

impl Cursor {
    pub fn new(pager: &mut Pager, tree: BTree) -> Result<Cursor> {
        let first = first_leaf(pager, tree.root)?;
        let leaf = read_leaf(pager, first)?;
        Ok(Cursor {
            cells: leaf.cells.into_iter(),
            next_leaf: leaf.next,
            leaves: 1,
        })
    }

    pub fn next(&mut self, pager: &mut Pager) -> Result<Option<(i64, Vec<u8>)>> {
        loop {
            if let Some(cell) = self.cells.next() {
                return Ok(Some((cell.rowid, payload_bytes(pager, cell.payload)?)));
            }
            let Some(id) = self.next_leaf else {
                return Ok(None);
            };
            self.leaves += 1;
            if self.leaves > pager.page_count() {
                return Err(broken("the leaf links form a loop"));
            }
            let leaf = read_leaf(pager, id)?;
            self.cells = leaf.cells.into_iter();
            self.next_leaf = leaf.next;
        }
    }
}

fn insert_into(pager: &mut Pager, id: PageId, cell: Cell) -> Result<(bool, Split)> {
    match read_node(pager, id)? {
        Node::Leaf(mut leaf) => {
            let (is_new, at) = match leaf.cells.binary_search_by_key(&cell.rowid, |c| c.rowid) {
                Ok(i) => {
                    let old = std::mem::replace(&mut leaf.cells[i], cell);
                    free_payload(pager, old.payload)?;
                    (false, i)
                }
                Err(i) => {
                    leaf.cells.insert(i, cell);
                    (true, i)
                }
            };
            if leaf.size() <= PAGE_SIZE {
                write_node(pager, id, &Node::Leaf(leaf))?;
                return Ok((is_new, None));
            }
            // Rows usually arrive in rowid order, each one past the end of
            // the last leaf. Splitting those leaves down the middle would
            // leave every page half empty forever, so the new row starts a
            // fresh page on its own instead.
            let appending = is_new && at == leaf.cells.len() - 1 && leaf.next.is_none();
            let split_at = if appending {
                at
            } else {
                split_point(&leaf.cells.iter().map(Cell::size).collect::<Vec<_>>())
            };
            let right_id = pager.allocate()?;
            let right = Leaf {
                cells: leaf.cells.split_off(split_at),
                next: leaf.next,
            };
            leaf.next = Some(right_id);
            let separator = leaf.cells.last().expect("left half is not empty").rowid;
            write_node(pager, right_id, &Node::Leaf(right))?;
            write_node(pager, id, &Node::Leaf(leaf))?;
            Ok((is_new, Some((separator, right_id))))
        }
        Node::Interior(mut node) => {
            let i = node.child_index(cell.rowid);
            let (is_new, split) = insert_into(pager, node.children[i], cell)?;
            let Some((separator, right)) = split else {
                return Ok((is_new, None));
            };
            // Child i split: it keeps keys up to `separator`, and the new page
            // takes the rest of its range.
            node.keys.insert(i, separator);
            node.children.insert(i + 1, right);
            if node.keys.len() <= Interior::CAPACITY {
                write_node(pager, id, &Node::Interior(node))?;
                return Ok((is_new, None));
            }
            let (left, promoted, right) = split_interior(node);
            let right_id = pager.allocate()?;
            write_node(pager, right_id, &Node::Interior(right))?;
            write_node(pager, id, &Node::Interior(left))?;
            Ok((is_new, Some((promoted, right_id))))
        }
    }
}

/// Removes `key` below page `id`. Returns `None` if the key was not found,
/// otherwise whether page `id` is now below half full.
fn delete_from(pager: &mut Pager, id: PageId, key: i64) -> Result<Option<bool>> {
    match read_node(pager, id)? {
        Node::Leaf(mut leaf) => {
            let Ok(i) = leaf.cells.binary_search_by_key(&key, |c| c.rowid) else {
                return Ok(None);
            };
            free_payload(pager, leaf.cells.remove(i).payload)?;
            let underfull = leaf.size() < MIN_FILL;
            write_node(pager, id, &Node::Leaf(leaf))?;
            Ok(Some(underfull))
        }
        Node::Interior(mut node) => {
            let i = node.child_index(key);
            match delete_from(pager, node.children[i], key)? {
                Some(true) => {
                    rebalance(pager, &mut node, i)?;
                    let underfull = node.size() < MIN_FILL;
                    write_node(pager, id, &Node::Interior(node))?;
                    Ok(Some(underfull))
                }
                // This page did not change, so it is no emptier than before.
                found => Ok(found.map(|_| false)),
            }
        }
    }
}

/// Child `i` of `parent` dropped below half full. Pour it and a neighbor
/// together: if everything fits in one page, merge them and drop the right
/// page; otherwise share the contents out evenly between the two.
fn rebalance(pager: &mut Pager, parent: &mut Interior, i: usize) -> Result<()> {
    let l = if i > 0 { i - 1 } else { i };
    let (left_id, right_id) = (parent.children[l], parent.children[l + 1]);
    match (read_node(pager, left_id)?, read_node(pager, right_id)?) {
        (Node::Leaf(mut left), Node::Leaf(right)) => {
            left.cells.extend(right.cells);
            if left.size() <= PAGE_SIZE {
                left.next = right.next;
                write_node(pager, left_id, &Node::Leaf(left))?;
                pager.free(right_id)?;
                parent.keys.remove(l);
                parent.children.remove(l + 1);
            } else {
                let at = split_point(&left.cells.iter().map(Cell::size).collect::<Vec<_>>());
                let right = Leaf {
                    cells: left.cells.split_off(at),
                    next: right.next,
                };
                parent.keys[l] = left.cells.last().expect("left half is not empty").rowid;
                write_node(pager, left_id, &Node::Leaf(left))?;
                write_node(pager, right_id, &Node::Leaf(right))?;
            }
        }
        (Node::Interior(mut left), Node::Interior(right)) => {
            // The parent's signpost between the two comes down between them.
            left.keys.push(parent.keys[l]);
            left.keys.extend(right.keys);
            left.children.extend(right.children);
            if left.keys.len() <= Interior::CAPACITY {
                write_node(pager, left_id, &Node::Interior(left))?;
                pager.free(right_id)?;
                parent.keys.remove(l);
                parent.children.remove(l + 1);
            } else {
                let (left, promoted, right) = split_interior(left);
                parent.keys[l] = promoted;
                write_node(pager, left_id, &Node::Interior(left))?;
                write_node(pager, right_id, &Node::Interior(right))?;
            }
        }
        _ => return Err(broken("sibling pages are at different depths")),
    }
    Ok(())
}

/// Where to cut a list of cells so the bigger half is as small as possible.
/// Both halves get at least one cell.
fn split_point(sizes: &[usize]) -> usize {
    let total: usize = sizes.iter().sum();
    let mut left = 0;
    let mut best = (usize::MAX, 1);
    for (i, size) in sizes[..sizes.len() - 1].iter().enumerate() {
        left += size;
        let bigger = left.max(total - left);
        if bigger < best.0 {
            best = (bigger, i + 1);
        }
    }
    best.1
}

/// Splits an overfull interior node around its middle key, which moves up to
/// the parent rather than staying in either half.
fn split_interior(mut node: Interior) -> (Interior, i64, Interior) {
    let mid = node.keys.len() / 2;
    let right = Interior {
        keys: node.keys.split_off(mid + 1),
        children: node.children.split_off(mid + 1),
    };
    let promoted = node.keys.pop().expect("an overfull node has keys");
    (node, promoted, right)
}

fn first_leaf(pager: &mut Pager, root: PageId) -> Result<PageId> {
    let mut id = root;
    while let Node::Interior(node) = read_node(pager, id)? {
        id = node.children[0];
    }
    Ok(id)
}

fn free_subtree(pager: &mut Pager, id: PageId) -> Result<()> {
    match read_node(pager, id)? {
        Node::Leaf(leaf) => {
            for cell in leaf.cells {
                free_payload(pager, cell.payload)?;
            }
        }
        Node::Interior(node) => {
            for child in node.children {
                free_subtree(pager, child)?;
            }
        }
    }
    pager.free(id)
}

/// Keeps a small value in the leaf, or writes a big one to overflow pages.
fn store_payload(pager: &mut Pager, bytes: &[u8]) -> Result<Payload> {
    if bytes.len() <= MAX_INLINE_PAYLOAD {
        return Ok(Payload::Inline(bytes.to_vec()));
    }
    let len = u32::try_from(bytes.len())
        .map_err(|_| Error::Storage("a row cannot be larger than 4 GiB".into()))?;
    let chunks: Vec<&[u8]> = bytes.chunks(OVERFLOW_CAPACITY).collect();
    let ids = (0..chunks.len())
        .map(|_| pager.allocate())
        .collect::<Result<Vec<_>>>()?;
    for (i, chunk) in chunks.iter().enumerate() {
        page::encode_overflow(pager.overwrite(ids[i])?, ids.get(i + 1).copied(), chunk);
    }
    Ok(Payload::Overflow { len, first: ids[0] })
}

fn payload_bytes(pager: &mut Pager, payload: Payload) -> Result<Vec<u8>> {
    let (len, first) = match payload {
        Payload::Inline(bytes) => return Ok(bytes),
        Payload::Overflow { len, first } => (len as usize, first),
    };
    let mut bytes = Vec::with_capacity(len);
    let mut next = Some(first);
    while let Some(id) = next {
        let (data, following) = page::decode_overflow(id, pager.page(id)?)?;
        bytes.extend_from_slice(data);
        if bytes.len() > len {
            break;
        }
        next = following;
    }
    if bytes.len() != len {
        return Err(broken("an overflow chain has the wrong length"));
    }
    Ok(bytes)
}

fn free_payload(pager: &mut Pager, payload: Payload) -> Result<()> {
    if let Payload::Overflow { first, .. } = payload {
        for id in overflow_chain(pager, first)? {
            pager.free(id)?;
        }
    }
    Ok(())
}

fn overflow_chain(pager: &mut Pager, first: PageId) -> Result<Vec<PageId>> {
    let mut ids = Vec::new();
    let mut next = Some(first);
    while let Some(id) = next {
        if ids.len() > pager.page_count() as usize {
            return Err(broken("an overflow chain loops"));
        }
        ids.push(id);
        next = page::decode_overflow(id, pager.page(id)?)?.1;
    }
    Ok(ids)
}

fn read_node(pager: &mut Pager, id: PageId) -> Result<Node> {
    Node::decode(id, pager.page(id)?)
}

fn read_leaf(pager: &mut Pager, id: PageId) -> Result<Leaf> {
    match read_node(pager, id)? {
        Node::Leaf(leaf) => Ok(leaf),
        Node::Interior(_) => Err(broken(&format!("page {id} should be a leaf"))),
    }
}

fn write_node(pager: &mut Pager, id: PageId, node: &Node) -> Result<()> {
    node.encode(pager.overwrite(id)?);
    Ok(())
}

fn broken(what: &str) -> Error {
    Error::Storage(format!("B-tree is damaged: {what}"))
}

/// State for [`BTree::check`].
#[derive(Default)]
struct Check {
    pages: Vec<PageId>,
    leaves: Vec<PageId>,
    leaf_depth: Option<usize>,
}

impl Check {
    /// Checks the subtree at `id`, whose keys must be above `low` and at most
    /// `high`.
    fn walk(
        &mut self,
        pager: &mut Pager,
        id: PageId,
        low: Option<i64>,
        high: Option<i64>,
        depth: usize,
    ) -> Result<()> {
        if self.pages.len() > pager.page_count() as usize {
            return Err(broken("the tree has more pages than the file"));
        }
        self.pages.push(id);
        let in_range = |key: i64| low.is_none_or(|l| key > l) && high.is_none_or(|h| key <= h);
        match read_node(pager, id)? {
            Node::Leaf(leaf) => {
                if *self.leaf_depth.get_or_insert(depth) != depth {
                    return Err(broken("leaves are at different depths"));
                }
                if depth > 0 && leaf.cells.is_empty() {
                    return Err(broken(&format!("leaf {id} is empty")));
                }
                let keys: Vec<i64> = leaf.cells.iter().map(|c| c.rowid).collect();
                if !keys.is_sorted_by(|a, b| a < b) || !keys.iter().all(|&k| in_range(k)) {
                    return Err(broken(&format!("leaf {id} has keys out of order")));
                }
                for cell in leaf.cells {
                    if let Payload::Overflow { first, .. } = cell.payload {
                        self.pages.extend(overflow_chain(pager, first)?);
                        payload_bytes(pager, cell.payload)?;
                    }
                }
                self.leaves.push(id);
            }
            Node::Interior(node) => {
                if node.keys.is_empty() {
                    return Err(broken(&format!("interior page {id} has no keys")));
                }
                if !node.keys.is_sorted_by(|a, b| a < b) || !node.keys.iter().all(|&k| in_range(k))
                {
                    return Err(broken(&format!("interior page {id} has keys out of order")));
                }
                for (i, &child) in node.children.iter().enumerate() {
                    let child_low = if i == 0 { low } else { Some(node.keys[i - 1]) };
                    let child_high = node.keys.get(i).copied().or(high);
                    self.walk(pager, child, child_low, child_high, depth + 1)?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_point_balances_bytes() {
        assert_eq!(split_point(&[1, 1]), 1);
        assert_eq!(split_point(&[10, 1, 1, 1, 1]), 1);
        assert_eq!(split_point(&[1, 1, 1, 1, 10]), 4);
        assert_eq!(split_point(&[3, 3, 3, 3]), 2);
    }

    #[test]
    fn split_interior_moves_the_middle_key_up() {
        let node = Interior {
            keys: vec![10, 20, 30, 40, 50],
            children: vec![1, 2, 3, 4, 5, 6],
        };
        let (left, promoted, right) = split_interior(node);
        assert_eq!((left.keys, left.children), (vec![10, 20], vec![1, 2, 3]));
        assert_eq!(promoted, 30);
        assert_eq!((right.keys, right.children), (vec![40, 50], vec![4, 5, 6]));
    }
}
