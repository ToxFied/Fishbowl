---
title: "B+trees"
description: "How each table is stored as a tree of pages sorted by rowid."
---

Each Fishbowl table is a **B+tree**: a tree of pages that keeps rows sorted by
their **rowid**, a 64-bit number every row gets when it is inserted (one more
than the largest rowid so far).

```text
                 [ interior: ≤40 → p7 | ≤95 → p3 | rest → p9 ]
                  /                    |                    \
   [ leaf p7: rows 1..40 ] → [ leaf p3: rows 41..95 ] → [ leaf p9: rows 96.. ]
```

There are two kinds of page:

- **Leaf pages** hold the rows, sorted by rowid. Each leaf also stores the
  page number of the next leaf, so the leaves form a chain from left to right.
- **Interior pages** hold no rows, only signposts: "rowids up to 40 are under
  page 7, up to 95 under page 3, the rest under page 9".

The "+" in B+tree means rows live only in the leaves, and the leaves are
linked. A plain B-tree also keeps rows in interior pages.

## Finding a row

To find rowid 50, start at the root. The signposts say 50 is under page 3. Read
page 3. If it is a leaf, binary-search its rows for 50. If it is another
interior page, follow its signposts down a level.

Each step reads one page. An interior page holds 340 signposts, so:

| Levels | Leaves it can point to | Rows (at ~40 per leaf) |
| ---: | ---: | ---: |
| 1 | 1 | 40 |
| 2 | 340 | 13,600 |
| 3 | 115,600 | about 4.6 million |

Four page reads find any row in a table of a billion rows. That is why nearly
every database stores tables and indexes this way.

## Reading every row

A full table scan (the `SeqScan` step in a plan) goes down to the leftmost leaf
once, then follows the chain of next-leaf links. It never climbs back up the
tree. Rows come out in rowid order, which is the order they were inserted
unless some were deleted.

## The root never moves

When a tree grows a level, the root's contents move to a new page and the root
page becomes the new top level. When it shrinks, the reverse happens. The root
stays at the same page number for the table's whole life, so the schema table's
note of "table `fish` starts at page 2" never goes stale.

How the tree grows and shrinks is in [splits and merges](splits-and-merges.md).

Code: `crates/engine/src/storage/btree.rs`. `BTree::check` verifies every rule
on this page: sorted keys, signposts that match their children, all leaves at
the same depth, and a leaf chain that visits every leaf in order.
