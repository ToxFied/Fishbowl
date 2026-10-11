# The crash test

Saying a database survives crashes is easy. This test cuts the power at every
point where a crash could matter, and checks what comes back.

## What it checks

After every simulated crash, Fishbowl reopens the database, which runs
[recovery](crash-recovery.md), and the test requires:

1. **Durability.** Every transaction whose `COMMIT` returned is there.
2. **Atomicity.** No other change is there. The one exception is the
   transaction whose commit was running when the power went out: it is there
   in full or not at all.
3. **Integrity.** `Database::integrity_check` passes: every B-tree keeps its
   rules, every index matches its table, and every page has exactly one use
   (the header, one tree, or the free list).
4. **Life goes on.** The recovered database takes a new transaction, and that
   transaction survives another reopen.
5. **Recovery survives a crash too.** The power is cut again at every step of
   recovery, and the next recovery must still meet 1 to 3.

These are exactly the promises in [durability](durability.md), checked
against exactly the disk described there.

## How it works

The workload is a list of 30 SQL statements: tables with a `PRIMARY KEY`, a
`UNIQUE` column and indexes; transactions that commit, one that rolls back
and one that `INSERT OR ROLLBACK` ends; autocommit statements; rows large
enough for overflow pages; `DROP TABLE` inside a transaction, which fills the
free list; deletes that merge pages; and leaves nearly full of text with no
zeros in it. A
buffer pool of 8 pages and a checkpoint every 12 frames make eviction and
checkpoints frequent.

1. **The oracle run.** Run the workload once on a `SimDisk` without a crash.
   Record whether each statement succeeds, what is committed after each one,
   and every operation it makes on the disk: each page write, log write, log
   truncate and sync. That list is the set of crash points.
2. **One run per crash point.** For crash point `n`, run the workload from
   scratch with the power set to go out as operation `n` starts. Every
   statement before the crash must behave as in the oracle run. The
   statement running at the crash decides what may come back: what was
   committed before it, or after it.
3. **Reboot several ways.** Rebuild the disk as it might look when the power
   returns:
   - *lost*: nothing written since each file's last sync;
   - *kept*: everything written, except the write in flight;
   - *torn*: everything written, plus part of the write in flight, once for
     each way to tear it: cut at every 512-byte sector boundary, after its
     first byte, its middle and all but its last byte, and, when it grew the
     file, also with the file grown and zeros after the cut;
   - *random*: four more, where each unsynced write survives on a coin flip
     and the write in flight is torn at random or lost.
4. **Check** each rebooted disk against the five guarantees above. The *kept*
   reboot of each crash point is also crashed again at every operation of its
   recovery, then rebooted *lost* and *kept* and checked again.

The simulated disk (`crates/engine/src/storage/fault.rs`) keeps, for each
file, what has been synced and the list of writes since. That is what lets it
build any of these outcomes after the fact.

## The numbers

| | |
| --- | ---: |
| Crash points | 178 |
| of which log writes | 91 |
| of which database page writes (during checkpoints) | 56 |
| of which log syncs | 21 |
| of which database file syncs | 5 |
| of which log truncates | 5 |
| Recoveries checked | 7,189 |
| of which after a second crash during recovery | 3,370 |

In 136 of the recoveries the commit in flight at the crash survived, and in
2,426 it did not, so both outcomes are really exercised. The test prints these
numbers:

```bash
cargo test -p fishbowl --test crash -- --nocapture
```

It takes about 16 seconds in a debug build.

## Testing the test

The test passed the first time it ran. A test that has never failed proves
little, so eight bugs were planted in the log code, one at a time, to see
whether it would notice:

| Planted bug | Caught? |
| --- | --- |
| Empty the log before syncing the database file in a checkpoint | yes |
| Skip the database file sync in a checkpoint | yes |
| Skip the log sync in a commit | yes |
| Accept frames without checking their checksum | yes, after two fixes to the test |
| Count frames whose commit frame never arrived | yes |
| Write pages into the database file before the commit | yes |
| Checkpoint the oldest copy of a page instead of the newest | yes |
| Checksum each frame alone, without the chain | no, as expected |

The checksum bug first went unnoticed, which exposed two gaps in the test:

- A torn write at the end of the log always left the file short, and
  recovery only reads whole frames, so it never looked at a torn frame. Real
  file systems can record a file's new length before its data. The disk
  model now does too: the "grown" tears.
- Pages are filled from the front and end in zeros. A torn frame cut off in
  those zeros is identical to the whole frame, so accepting it is correct.
  The workload now updates a leaf that is about 90% text, so a tear almost
  anywhere in it cuts live data.

The chain still goes unnoticed, and that is expected. Within this disk model,
the log is never written over, and the salt and each frame's own checksum
catch every torn, missing or stale frame. The chain only matters if bytes
from an earlier log could come back, which the model rules out. SQLite chains
its checksums too, and Fishbowl keeps the chain as a second line of defense.

## What it found

No bug in the final engine. Its findings were about the test itself: the two
gaps above. Two other tests found real problems during M6:

- The first version of autocommit rolled back every failed statement. That
  broke `INSERT OR FAIL`, which keeps the rows written before the failure.
  An existing test caught it.
- The differential test against `sqlite3` found that SQLite, inside a
  transaction, keeps the partial work of a failed `INSERT OR REPLACE`, `OR
  IGNORE`, `OR FAIL` or `OR ROLLBACK`. See [transactions](transactions.md).

## What it does not cover

- The real file code. `SimDisk` follows the same rules as `FileStore` and
  `FileLog`, and two tests in `tests/transactions.rs` run on real files, but
  the operating system's side is trusted, not tested.
- Disks that lose synced data, and bit flips in the database file.
- Every subset of unsynced writes. The test tries none, all, and four random
  subsets per crash point, not all of them.
- More than one process using a database.

Code: `crates/engine/tests/crash.rs`, `crates/engine/src/storage/fault.rs`.
