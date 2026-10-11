# Crash recovery

Every time Fishbowl opens a database, it first recovers. It does not know
whether the last run ended cleanly, and it does not need to: recovery does the
same thing either way.

1. **Read the log header.** If there is no log, or the header is damaged,
   nothing was committed since the last checkpoint. Empty the log and stop.
2. **Read the frames in order,** checking each one's salt and checksum. Stop
   at the first frame that is damaged or incomplete.
3. **Keep the frames up to the last commit frame.** Frames after it belong to
   a commit that never finished, or are a torn write. Drop them.
4. **Checkpoint the frames that were kept** into the database file, and empty
   the log.

Then the pager reads the header page, and the schema table rebuilds the list
of tables, as on any open. `Database::recovery` reports what step 3 found: how
many transactions and frames were recovered, and how many bytes were dropped.

## An example

A commit appends three frames and syncs. The power goes out during the
second frame's write. The log holds: the header, earlier committed
transactions, the first new frame and half of the second. Recovery keeps the
earlier transactions and drops the rest. The interrupted commit never
returned, so nobody was told it succeeded, and it is gone in full.

If the power had gone out during the sync instead, the operating system might
already have written every frame. Then the commit survives in full. Both
outcomes are allowed; half of it never is.

## A crash during recovery

Recovery is a checkpoint, and a checkpoint can be cut off and run again (see
[checkpoints](checkpoints.md)). So a crash during recovery is just another
crash. The next open recovers again and gets the same result. The crash test
checks this by cutting the power again at every step of each recovery.

## A short last page

A crash during a checkpoint can tear the last page of a database file that
was growing, leaving the file short of a whole number of pages. The file store
counts that last page anyway, with zeros for its missing bytes, and recovery
rewrites it from the log. The log still has it, because the log is only
emptied after the database file is synced.

## What recovery cannot fix

A disk that loses data after a sync said it was safe, or that flips bits in
the database file. The log's checksums protect frames, not the database
file's own pages, as in SQLite by default. Backups are the answer to those.

Code: `crates/engine/src/storage/wal.rs` (`Wal::open`, `recover`).
