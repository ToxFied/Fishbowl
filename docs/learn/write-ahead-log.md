---
title: "The write-ahead log"
description: "How commits append to a log first, so a crash can never leave half a change."
---

## The problem

A commit usually changes several pages: a leaf, its parent, an index page,
the header. A disk writes them one at a time, and the power can go out
between any two. If Fishbowl wrote them straight into the database file, a
crash could leave a parent pointing at a leaf that was never written. A single
4 KiB write can even be **torn**: the first half new and the second half old.

## The idea

Never change the database file at commit time. Instead, append a copy of
every changed page to a second file, the **log**, then sync the log. The
database file only changes later, at a [checkpoint](checkpoints.md). It is
called write-*ahead* because the log is always written before the database
file.

A crash while appending leaves the database file as it was, plus some junk at
the end of the log, which [recovery](crash-recovery.md) recognizes and
ignores. SQLite's WAL mode works the same way.

## Frames

Each page copy in the log is a **frame**: a 20-byte header, then the page.
The last frame of a transaction is its **commit frame**. Its header holds the
database's size in pages after the commit; every other frame holds 0 there.

```text
header | p5 | p9 | p12 commit | p5 | p7 commit | p3 | p4 (torn)
         transaction 1           transaction 2   never committed
```

A transaction counts only if its commit frame made it to the disk intact.
Above, the log holds two transactions. The last two frames belong to a commit
that was cut off, so recovery drops them.

## Telling a good frame from junk

Three things in each frame let recovery decide:

- **A checksum** of the frame's page number, commit size and page. A torn
  frame, or a region of zeros where a frame should be, gets the wrong sum.
- **A chain.** Each checksum continues from the one before it, so a frame
  vouches for every frame before it too.
- **A salt**, a number in the log header that is copied into every frame. It
  changes each time the log is emptied, so a frame from an older log can
  never pass for a new one.

Recovery reads from the start and stops at the first frame that fails, so it
only ever keeps a run of good frames with no gap. The checksum is SQLite's:
two running sums over 32-bit words. It is cheap and catches accidents. It is
not meant to stop someone who forges frames on purpose.

## Reading pages

While frames wait in the log, the newest committed copy of a page is there,
not in the database file. The log keeps an index in memory from page number
to its newest frame. A read checks the index first, then the database file.

## Only redo, never undo

Fishbowl never lets an uncommitted page leave memory (see the
[buffer pool](buffer-pool.md)). So the log only ever holds committed
transactions, plus at most one half-written one at the end. Recovery copies
committed pages forward and drops the rest; it never has to undo anything.
Databases that let uncommitted pages reach the disk, such as those built on
the ARIES design, also need undo records. Fishbowl trades memory for that
simplicity: one transaction's changes must fit in RAM.

## Layout

| Bytes | Log header (24 bytes) |
| --- | --- |
| 0..8 | magic `FISHWAL1` |
| 8..12 | page size, 4096 |
| 12..16 | salt |
| 16..24 | checksum of bytes 0..16 |

| Bytes | Frame (20 bytes, then the page) |
| --- | --- |
| 0..4 | page number |
| 4..8 | database size after the commit, or 0 if not a commit frame |
| 8..12 | salt, copied from the header |
| 12..20 | checksum of bytes 0..8 and the page, continuing the chain |
| 20..4116 | the page |

Code: `crates/engine/src/storage/wal.rs`.
