---
title: "Splits and merges"
description: "How a B+tree stays balanced as rows come and go."
---

A [B+tree](b-plus-trees.md) stays shallow and balanced by itself. Every leaf is
always the same distance from the root. Two operations keep it that way: a
page that gets too full **splits**, and a page that gets too empty **merges**.

## Insert: splitting

A new row goes into the leaf that covers its rowid. If the leaf still fits in
4 KiB, that's it. If not, the leaf splits:

1. Cut the leaf's rows into two halves with about the same number of bytes.
2. The left half stays on the old page. The right half goes to a new page.
3. Tell the parent. It adds a signpost "up to 3 → old page", and the signpost
   that used to point at the old page now points at the new one.

```text
before:  parent [ ≤40 → A | rest → B ]       A: 1 2 3 4 5 (full) + 6
after:   parent [ ≤3 → A | ≤40 → A2 | rest → B ]
         A: 1 2 3     A2: 4 5 6
```

The parent got one more signpost, so it may overflow too. Then it splits the
same way, and its middle signpost moves up to its parent. Splits can ripple all
the way to the root. When the root splits, the tree gains a level. That is the
only way a B+tree gets taller, which is why all leaves stay at the same depth.

### Appending

Rows usually arrive in rowid order, each one past the end of the last leaf.
Splitting that leaf down the middle would leave the left half half-empty
forever, because nothing will ever be inserted there again. So when a row is
appended to the last leaf, Fishbowl starts a fresh leaf with just the new row
and leaves the full one full. Inserting 1,000 rows in order uses 28 leaves this
way, instead of about 55. SQLite has the same trick.

## Delete: merging and borrowing

Removing a row can leave a leaf less than half full. Lots of nearly empty pages
waste space and make scans read more pages, so the parent steps in. It pours
the thin page and its neighbor together:

- **Merge.** If everything fits in one page, it goes on the left page. The
  right page is freed, and the parent drops one signpost.
- **Borrow.** If it does not fit, the rows are shared out evenly between the
  two pages, and the signpost between them moves to the new boundary.

```text
merge:   [ 1 2 ] [ 7 ]           →  [ 1 2 7 ]        (one page freed)
borrow:  [ 1 ] [ 5 6 7 8 9 10 ]  →  [ 1 5 6 ] [ 7 8 9 10 ]
```

If the parent lost a signpost and is now under half full, its own parent does
the same thing one level up. When the root is left with a single child, that
child moves up into the root page and the tree loses a level.

Delete every row of a table and the tree shrinks back to one empty leaf, with
every other page on the free list. A property test checks exactly that.

Code: `crates/engine/src/storage/btree.rs` (`insert_into`, `delete_from`,
`rebalance`).
