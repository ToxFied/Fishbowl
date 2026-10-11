//! Property tests: the B+tree must behave exactly like Rust's `BTreeMap`.
//!
//! proptest generates long random sequences of inserts, replacements and
//! deletes, runs each one on the B+tree and on a `BTreeMap`, and fails if the
//! two ever disagree. When a sequence fails, proptest shrinks it to the
//! shortest one that still fails. Commits and rollbacks are mixed in, so a
//! rollback must bring back the tree as it was at the last commit. After the
//! sequence, the tree's structure is checked, every page is accounted for,
//! and the files are reopened from their bytes to prove that nothing
//! committed lived only in the buffer pool.

use std::collections::{BTreeMap, BTreeSet};

use fishbowl::storage::btree::{BTree, Cursor};
use fishbowl::storage::pager::Pager;
use fishbowl::storage::{MemoryLog, MemoryStore, Options};
use proptest::prelude::*;

#[derive(Debug, Clone)]
enum Op {
    /// Insert or replace a value of this many bytes.
    Insert(i64, usize),
    Delete(i64),
    Get(i64),
    Commit,
    Rollback,
}

/// Bytes that depend on the key and on when they were written, so a stale or
/// misplaced value never matches by accident.
fn value(key: i64, len: usize, version: usize) -> Vec<u8> {
    let seed = key.wrapping_mul(31).wrapping_add(version as i64);
    (0..len)
        .map(|i| (seed.wrapping_add(i as i64 * 7) % 251) as u8)
        .collect()
}

fn key() -> impl Strategy<Value = i64> {
    prop_oneof![
        // A narrow range, so inserts hit existing keys and deletes find them.
        9 => -100i64..400,
        1 => any::<i64>(),
    ]
}

fn size() -> impl Strategy<Value = usize> {
    prop_oneof![
        6 => 0usize..64,
        3 => 64usize..1000,
        // Bigger than a leaf allows, so the value spills to overflow pages.
        1 => 1000usize..9000,
    ]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        5 => (key(), size()).prop_map(|(k, n)| Op::Insert(k, n)),
        3 => key().prop_map(Op::Delete),
        1 => key().prop_map(Op::Get),
        1 => Just(Op::Commit),
        1 => Just(Op::Rollback),
    ]
}

/// A pager whose log is checkpointed every few frames, so pages are read
/// back from the log and from the database file.
fn new_pager(pool: usize) -> Pager {
    let options = Options {
        pool_pages: pool,
        checkpoint_frames: 16,
    };
    let store = Box::new(MemoryStore::default());
    Pager::open(store, Box::new(MemoryLog::default()), options).unwrap()
}

fn scan(pager: &mut Pager, tree: BTree) -> Vec<(i64, Vec<u8>)> {
    let mut cursor = Cursor::new(pager, tree).unwrap();
    let mut rows = Vec::new();
    while let Some(row) = cursor.next(pager).unwrap() {
        rows.push(row);
    }
    rows
}

/// Checks the tree's rules and that every page is either in the tree, on
/// the free list, or the header. Nothing leaks and nothing is used twice.
fn check_pages(pager: &mut Pager, tree: BTree) -> usize {
    let shape = tree.check(pager).unwrap();
    let free = pager.free_pages().unwrap();
    let mut seen = BTreeSet::from([0]);
    for &page in shape.pages.iter().chain(&free) {
        assert!(seen.insert(page), "page {page} is used twice");
    }
    assert_eq!(seen.len(), pager.page_count() as usize, "pages leaked");
    assert_eq!(free.len(), pager.free_count() as usize);
    shape.depth
}

fn run(ops: &[Op], pool: usize) {
    let mut pager = new_pager(pool);
    let tree = BTree::create(&mut pager).unwrap();
    pager.commit().unwrap();
    let mut model = BTreeMap::new();
    let mut committed = model.clone();
    for (version, op) in ops.iter().enumerate() {
        match *op {
            Op::Insert(k, len) => {
                let v = value(k, len, version);
                let is_new = tree.insert(&mut pager, k, &v).unwrap();
                assert_eq!(is_new, model.insert(k, v).is_none(), "insert {k}");
            }
            Op::Delete(k) => {
                let found = tree.delete(&mut pager, k).unwrap();
                assert_eq!(found, model.remove(&k).is_some(), "delete {k}");
            }
            Op::Get(k) => {
                assert_eq!(tree.get(&mut pager, k).unwrap().as_ref(), model.get(&k));
            }
            Op::Commit => {
                pager.commit().unwrap();
                committed = model.clone();
            }
            Op::Rollback => {
                pager.rollback();
                model = committed.clone();
            }
        }
    }
    let expected: Vec<(i64, Vec<u8>)> = model.into_iter().collect();
    assert_eq!(scan(&mut pager, tree), expected);
    assert_eq!(tree.count(&mut pager).unwrap(), expected.len());
    assert_eq!(
        tree.last_key(&mut pager).unwrap(),
        expected.last().map(|e| e.0)
    );
    check_pages(&mut pager, tree);

    // Everything committed must survive a trip through the files' bytes.
    pager.commit().unwrap();
    let (store, log) = pager.into_stores();
    let mut reopened = Pager::open(store, log, Options::default()).unwrap();
    assert_eq!(scan(&mut reopened, tree), expected);
    check_pages(&mut reopened, tree);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// A pool of three frames forces constant eviction between commits,
    /// and growth past three frames within one.
    #[test]
    fn matches_btreemap_with_a_tiny_buffer_pool(ops in prop::collection::vec(op(), 0..1000)) {
        run(&ops, 3);
    }

    #[test]
    fn matches_btreemap_with_the_default_buffer_pool(ops in prop::collection::vec(op(), 0..1000)) {
        run(&ops, 64);
    }

    /// Fill a tree, then delete every key in random order. The tree must
    /// shrink back to one empty root page, with every other page free.
    #[test]
    fn deleting_everything_frees_every_page(
        keys in prop::collection::btree_set(key(), 1..400),
        sizes in prop::collection::vec(size(), 400),
        order in any::<u64>(),
    ) {
        let mut pager = new_pager(4);
        let tree = BTree::create(&mut pager).unwrap();
        let mut keys: Vec<i64> = keys.into_iter().collect();
        for (i, &k) in keys.iter().enumerate() {
            tree.insert(&mut pager, k, &value(k, sizes[i], 0)).unwrap();
        }
        // Delete in a scrambled order that proptest can reproduce.
        keys.sort_by_key(|&k| (k as u64 ^ order).wrapping_mul(0x9E37_79B9_7F4A_7C15));
        for &k in &keys {
            prop_assert!(tree.delete(&mut pager, k).unwrap());
        }
        prop_assert_eq!(check_pages(&mut pager, tree), 1);
        prop_assert_eq!(pager.free_count(), pager.page_count() - 2);
    }
}

/// Random sequences rarely grow past two levels, because an interior page
/// holds 340 children. This test builds a three-level tree on purpose, so
/// interior pages split, borrow from each other and merge.
#[test]
fn three_level_tree_grows_and_shrinks() {
    let mut pager = new_pager(16);
    let tree = BTree::create(&mut pager).unwrap();
    let mut model = BTreeMap::new();
    // About 900 bytes per row puts four rows in a leaf, so 2,000 rows need
    // 500 leaves: more than one interior page can point to.
    let n = 2_000i64;
    let key = |i: i64| (i * 7_919) % n; // every key once, out of order
    for i in 0..n {
        let v = value(key(i), 850 + (i as usize % 100), 0);
        tree.insert(&mut pager, key(i), &v).unwrap();
        model.insert(key(i), v);
    }
    assert_eq!(check_pages(&mut pager, tree), 3);

    for i in 0..n {
        if i % 3 != 0 {
            assert!(tree.delete(&mut pager, key(i)).unwrap());
            model.remove(&key(i));
        }
        if i % 250 == 0 {
            check_pages(&mut pager, tree);
        }
    }
    assert_eq!(check_pages(&mut pager, tree), 2);
    let expected: Vec<(i64, Vec<u8>)> = model.into_iter().collect();
    assert_eq!(scan(&mut pager, tree), expected);
}

#[test]
fn appending_in_rowid_order_fills_pages() {
    let mut pager = new_pager(64);
    let tree = BTree::create(&mut pager).unwrap();
    for k in 1..=1_000 {
        tree.insert(&mut pager, k, &[7; 100]).unwrap();
    }
    // 112 bytes per cell, so a full leaf holds 36 rows: 1,000 rows need 28
    // leaves. Splitting each full leaf down the middle would take about 55.
    let shape = tree.check(&mut pager).unwrap();
    assert_eq!(shape.leaves, 28);
}
