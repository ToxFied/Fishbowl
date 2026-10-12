---
title: "Property testing the B-tree"
description: "Testing the B-tree against Rust's BTreeMap with random lists of operations."
---

A normal test checks one example: insert 1, 2, 3, then expect 1, 2, 3 back.
B-tree bugs hide in rare situations, such as a merge right after a split, at
the third level, while the page being merged has just been evicted from the
buffer pool. Nobody writes that example by hand.

A **property test** states a rule that must hold for every input, then lets a
library generate hundreds of random inputs. Fishbowl uses the
[proptest](https://docs.rs/proptest) crate.

## The rule: behave like `BTreeMap`

Rust's standard library has `BTreeMap`, a sorted map that lives in memory and
is known to be correct. Fishbowl's B+tree is also a sorted map, from rowid to
bytes. So the rule is: **after any sequence of operations, the B+tree and a
`BTreeMap` given the same sequence must agree.** The `BTreeMap` is called the
model.

Each test case is a random list of up to 1,000 operations: insert or replace a
key with a value of random size (sometimes big enough to need overflow pages),
delete a key, or look one up. Keys mostly come from a narrow range, so inserts
often replace and deletes often find something. For each operation, both maps
must give the same answer. At the end:

- A full scan must return exactly the model's contents, in order.
- `BTree::check` must pass: sorted keys, correct signposts, every leaf at the
  same depth, an unbroken leaf chain.
- Every page must be accounted for: in the tree, on the free list, or the
  header. None leaked, none used twice.
- Commits and rollbacks are mixed into the sequence. After a rollback, the
  tree must match the model as it was at the last commit.
- The pager commits, and a new pager reopens the files from their bytes. The
  contents must still match, which proves nothing committed lived only in
  memory.

This runs 256 times with a three-frame buffer pool, which forces constant
eviction between commits, and 256 times with the normal pool. Another test fills a tree and
deletes every key in random order, then checks that the tree is back to one
empty page. A fixed test builds a three-level tree, which random runs rarely
reach. A last property test runs random `INSERT`, `UPDATE` and `DELETE`
statements through SQL and compares the table with a plain list.

## Shrinking

When a case fails, proptest does not just print 800 random operations. It
**shrinks** them: it tries removing operations and making values smaller,
keeping each change that still fails. The report is usually a handful of
operations, small enough to trace by hand.

## Testing the tests

A test suite that always passes might not be checking anything. So the tests
were run against deliberately broken versions of the tree: one that forgets to
relink leaves after a merge, one that puts the wrong signpost after borrowing,
and a pager that drops changed pages instead of writing them back. Each break
failed four of the five B-tree tests.

Code: `crates/engine/tests/btree.rs` and `crates/engine/tests/storage.rs`.
