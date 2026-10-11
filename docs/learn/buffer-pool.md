# The buffer pool

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
**dirty**, meaning it differs from what is in the file. A dirty page must be
written back before its frame is reused, or the change would be lost. This is
called **write-back**.

At the end of every statement that changes data, Fishbowl **flushes**: it
writes every dirty page to the store, then asks the operating system to sync
the file to disk.

Flushing at the end of a statement is not yet crash-safe. If the power fails
halfway through the writes, the file can hold some new pages and some old ones.
Milestone 6 fixes that with a write-ahead log.

## Watching it work

The pager counts hits, misses and evictions. The property tests run the B-tree
with a pool of only three frames, so nearly every access evicts something and
every write-back path is exercised.

Code: `crates/engine/src/storage/pager.rs` (`Pager::page`, `Pager::flush`).
