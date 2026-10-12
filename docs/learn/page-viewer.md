---
title: "Reading the page viewer"
description: "How to read the B-tree drawing in the web UI's pages view."
---

The **pages** view in the web UI draws the B-tree behind one table or index,
straight from the bytes on its 4 KiB [pages](pages.md). Pick a table or an
index by name at the top.

## The drawing

```text
┌ p5 ─────────────────────────────────────────────────── 7 ┐
│ ≤ 65 ≤ 130 ≤ 195 ≤ 260 ≤ 325 ≤ 390 ≤ 455                  │   root, interior
└───────────────────────────────────────────────────────────┘
┌ p7 65 ┐┌ p6 65 ┐┌ p8 65 ┐┌ p9 65 ┐ ... ┌ p13 57 ┐            leaves
│ ▇▇▇▇▇ ││ ▇▇▇▇▇ ││ ▇▇▇▇▇ ││ ▇▇▇▇▇ │     │ ▇▇▇▇░ │
│ 1…65  ││66…130 ││131…195││196…260│     │456…512│
└───────┘└───────┘└───────┘└───────┘     └───────┘
          ↳ p14
```

This is the seeded `feeding` table: 512 rows on 8 leaves under one root.

- **Rows of boxes are levels.** The root is on top and the leaves are at the
  bottom. A page sits directly above the pages it points to and is as wide as
  they are together, so you can read which leaves sit under which signpost.
- **The number after the page number** counts its entries: rows in a leaf,
  signposts in an interior page.
- **The bar is the fill level**: bytes in use out of 4,096. Leaves that
  filled up in order sit at about 4,093 bytes; the last leaf is still
  filling. The root needs only 91 bytes for its seven signposts. That is why
  B-trees are shallow: one interior page has room for about 340 signposts.
- **The range** is a leaf's first and last key. An interior page shows its
  signposts instead: "≤ 65" means everything up to 65 is in the child below
  it, and the last child takes the rest.
- **↳ p14** is an overflow chain. Row 100 of `feeding` has a 3,106-byte note.
  That is too big to keep in a leaf, so the leaf keeps only a pointer and the
  bytes live on page 14. A row needing more than one overflow page shows a
  chain such as `↳ p14 → p15`. See [row encoding](row-encoding.md).

Page numbers are not in key order. Page 6 holds rows 66 to 130 but page 7
holds rows 1 to 65, because the root split early on: its contents moved to a
new page and the root kept its own number. Pages are handed out in the order
they are needed, and the signposts are what keep the keys in order.

Click any page to see its details: kind, exact bytes used, every key, its
child pointers (clickable), the next leaf to the right and its overflow
pages.

## An index tree

Select `fish_species` to see an index. Its keys are the indexed value and
then the rowid, such as `koi #2`, `koi #6`, `koi #8`. The rowid makes every
key distinct and keeps equal values in rowid order. See [indexes](indexes.md).

## Watching a split

**Insert until a split** adds one row at a time to the selected table (or
the table an index belongs to) and stops when the tree changes shape. Orange
marks what changed in the last step, and a line says what happened:

- **A leaf split.** "p13 was full, so it split: new page p15 takes keys from
  521, and p5 gets a signpost to it." Rows usually arrive in rowid order, so
  the new row starts a fresh page and the full one stays full.
- **A root split.** "Root p3 was full. Its contents moved down into p17 and
  p16, and the tree grew to 2 levels." A tree only gets taller at the root,
  which keeps every leaf at the same depth.

Select `fish` to see a root split: its 8 rows fit in one leaf, which is also
the root, so about 50 inserts split it. See [splits and
merges](splits-and-merges.md).

Statements you run in the editor are compared the same way. Run a `DELETE`
and pages that merged away are listed. They go to the free list, counted in
the summary line as "free".

## Where it comes from

`Storage::inspect_tree` reads each page of the tree, root first, decodes it
with the same code the B-tree uses, and reports what it finds. It only
reads: the pages pass through the buffer pool like any other read, and
nothing is written.

Code: `crates/engine/src/storage/inspect.rs`,
`web/src/components/PageViewer.tsx`.
