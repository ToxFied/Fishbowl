---
title: "The buffer pool"
description: "How recently used pages stay in memory so most reads skip the disk."
---

Reading a page from disk is thousands of times slower than reading it from
memory. A query that touches the same page twice should not pay for the disk
twice. So the pager keeps recently used pages in memory, in a **buffer pool**.

The pool is a fixed number of **frames**, slots that each hold one page.
Fishbowl's default is 64 frames, which is 256 KiB.

When code asks for page 7:

- **Hit.** Page 7 is already in a frame. Return it. No disk access.
- **Miss.** It is not. Pick a frame to reuse, read page 7 into it, and return
  it.

## Which frame to reuse

When every frame is full, one page has to leave. Fishbowl picks the **least
recently used** one (LRU): the page nobody has asked for in the longest time.
The bet is that a page used recently will be used again soon, which holds for
the top of a B-tree: every lookup passes through the root, so the root stays in
the pool.

## Dirty pages

Changing a page only changes the copy in its frame. The frame is marked
**dirty**, meaning it differs from what is on disk.

A dirty page is never evicted. If every frame is dirty, the pool grows past
its size until the transaction ends. So an uncommitted change never reaches
the disk, and recovery after a crash never has to undo anything. Textbooks
call this policy **no steal**. The cost is that one transaction's changes must
fit in memory.

When the transaction commits, the pager hands every dirty page to the
[write-ahead log](write-ahead-log.md) at once, and the frames become clean
again. When it rolls back, the pager throws the dirty frames away; the next
read fetches the committed copy.

## Watching it work

The pager counts hits, misses and evictions. The property tests run the B-tree
with a pool of only three frames, so nearly every access between commits
evicts something, and a long transaction makes the pool grow.

Code: `crates/engine/src/storage/pager.rs` (`Pager::page`, `Pager::commit`,
`Pager::rollback`).
