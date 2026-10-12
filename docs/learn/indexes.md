---
title: "Indexes"
description: "Separate B-trees that find rows by value instead of by rowid."
---

A table's B-tree is sorted by rowid, so finding "row 42" takes a few page
reads. Finding "every fish named nemo" does not: names are scattered across the
whole tree, so the only way is to read every row. An **index** fixes that for
one column (or a few). It is a second B-tree, kept beside the table, sorted by
that column instead.

```text
table fish (sorted by rowid)          index fish_name (sorted by name)
  1  goldie   goldfish                  (bubbles, 2)
  2  bubbles  koi                       (dory,    4)
  3  nemo     clownfish                 (goldie,  1)
  4  dory     tang                      (nemo,    3)
```

Each index entry is a value and the rowid of the row it came from. To answer
`WHERE name = 'nemo'`, walk down the index to `nemo` (a few page reads), read
rowid 3, then walk down the table to row 3. Two short walks instead of a full
read.

## Why the key is (value, rowid)

The index B-tree is the same code as the table's B-tree (`btree.rs`), just with
a different key. A table's key is its rowid. An index's key is the pair
**(value, rowid)**, compared value first, then rowid.

- **Every key is distinct.** Many rows can share `species = 'koi'`, but no two
  share `('koi', 2)`. A B-tree needs distinct keys so each entry has exactly one
  place, and so deleting row 2's entry removes that entry and not another koi.
- **Equal values sit together, in rowid order.** All the `'koi'` entries are
  neighbors in the leaves, so "every koi" is one walk along the leaf chain:
  seek to the first `'koi'`, read until the value changes.
- **The rowid is the way back.** The entry carries no copy of the row. The
  rowid leads to it in the table's tree.

An index on two columns, like `(species, weight)`, has keys
`(species, weight, rowid)`. It is sorted by species, then weight within each
species, so it helps `WHERE species = 'koi'` but not `WHERE weight > 1`
alone. That second sort only matters within a species.

Values compare the way SQLite orders them: NULL first, then numbers by value
(`3` and `3.0` are equal), then text byte by byte.

## INTEGER PRIMARY KEY needs no index

A column declared `INTEGER PRIMARY KEY` *is* the rowid, as in SQLite. Inserting
`(7, 'nemo')` stores the row under rowid 7, and inserting NULL there picks the
next rowid. The table's own B-tree is already sorted by it, so `WHERE id = 7`
or `WHERE id BETWEEN 3 AND 9` walks the table tree directly. The plan shows
`IndexScan fish USING INTEGER PRIMARY KEY (id = 7)`.

## Keeping an index in step

An index is only useful if it always agrees with its table. Every write
changes both:

| Statement | Table | Each index |
| --- | --- | --- |
| INSERT | add the row | add (value, rowid) |
| DELETE | remove the row | remove (value, rowid) |
| UPDATE | replace the row | remove the old entry, add the new one |

`CREATE INDEX` on a table that already has rows reads them all once to fill the
new index. `DROP TABLE` drops the table's indexes too.

## When a query uses an index

When a WHERE term is an equality, a range, a `BETWEEN` or an `IN` list on an
index's leading columns, the plan can read that index (an **IndexScan**)
instead of every row (a **SeqScan**). Several range terms on the same column
combine, so `a > 5 AND a < 9` reads only from 5 to 9. Since M5 the planner
weighs every index, and the full scan, by estimated cost, and the plan lists
the ones it turned down. See [choosing an index](choosing-an-index.md).

```text
Project pk  [rows=2]
  Filter ((col0 > 5) AND (col1 = 'x'))  [rows=2]
    IndexScan tab1 USING INDEX idx_col0 (col0 > 5)  [rows=3]
      why: index idx_col0 finds about 262,144 of the 1,048,576 rows, and each leads to its row by rowid...
```

The Filter stays above the IndexScan with the whole WHERE clause. The index
only narrows the rows down; the Filter still decides them. That keeps the index
code simple and safe: as long as it never skips a row that could match, the
answer is right. Here the index found 3 rows and the Filter kept 2.

One detail needs care: comparisons convert types first. On an INTEGER column,
`col0 = '5'` means the number 5, so the index must look for 5, not the text
`'5'`. The planner converts the constant the same way the comparison would
before it searches. A property test checks that every query returns the same
rows with and without indexes, on random data mixing numbers, text and NULL.

Rows from an IndexScan come out in index order, not rowid order. SQL promises
no order without `ORDER BY`, and SQLite behaves the same way.

## The cost

Indexes are not free. Each one is another B-tree to update on every write, and
more pages on disk. That is why a database does not index every column.

Code: `crates/engine/src/storage/index.rs` (the key), `storage/btree.rs` (the
tree, now generic over its key), `access.rs` (choosing and running an
IndexScan), `write.rs` (keeping indexes in step).
