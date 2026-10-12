---
title: "Checkpoints"
description: "How committed pages move from the log back into the database file."
---

The [write-ahead log](write-ahead-log.md) grows with every commit. A
**checkpoint** moves its contents into the database file and empties it. It
takes four steps, in this order:

1. Copy the newest copy of each page in the log into the database file.
2. Sync the database file.
3. Empty the log (truncate it to zero bytes).
4. Sync the log.

## Why the order matters

During step 1, the database file can hold any mix of old pages, new pages and
torn pages. That is fine, because the log still holds every page being copied.
The log is only emptied once step 2 has made the database file durable. So at
every moment, each committed page is safe in at least one place:

| Crash during | Where the committed pages are | Recovery |
| --- | --- | --- |
| step 1 or 2 | in the log | redoes the checkpoint |
| step 3 or 4 | in the database file, maybe also in the log | redoes it, or finds the log empty |

Swap steps 2 and 3, and a crash can lose committed data. The crash test checks
this: that is one of the bugs planted on purpose to make sure the test notices
(see [the crash test](crash-harness.md)).

## Doing it twice is harmless

A checkpoint copies whole pages. Copying the same page onto the same spot a
second time changes nothing. So a checkpoint that was cut off can simply run
again from the start. This is what makes recovery safe to interrupt.

## Why empty the log, and why sync it then

SQLite instead keeps the log file and writes the next transaction over its
start, which takes extra care so the old frames behind the new ones can never
count. Fishbowl truncates the log and syncs that, so the old frames are gone
for good before any new frame is written.

## When it runs

- After a commit, once the log holds 1000 frames, SQLite's default. That is
  about 4 MB.
- Every time a database is opened, as part of [recovery](crash-recovery.md).
- When asked, with `Database::checkpoint`.

A checkpoint that fails after a commit does not fail the commit, which was
already durable. But it stops the database handle until it is reopened (see
[durability](durability.md)).

Code: `crates/engine/src/storage/wal.rs` (`Wal::checkpoint`) and
`crates/engine/src/storage/pager.rs` (`Pager::commit`).
