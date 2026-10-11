# Durability: when is a write safe?

## What sync means

When a program writes to a file, the operating system copies the bytes into
memory and returns at once. It writes them to the disk later, in whatever
order suits it, and the drive may hold them in its own cache for a while
longer. A crash before then loses them.

**Syncing** a file (`fsync`) asks for every earlier write to reach the disk,
and returns only once it has. Fishbowl uses Rust's `File::sync_data`. On
Linux that is `fdatasync`. On macOS, Rust implements it with `F_FULLFSYNC`,
because a plain `fsync` there does not empty the drive's cache.

Only synced data is safe. Everything else is a hope.

## The promise

**A transaction is durable once `COMMIT` returns `Ok`**, or, outside `BEGIN`,
once its statement returns `Ok`. At that moment its frames are in the
[write-ahead log](write-ahead-log.md) and the log has been synced.

- If the power goes out before `COMMIT` returns, the transaction may survive
  or not, but it is never there in part.
- If `COMMIT` returns an error, the same holds. Only reopening the database
  can tell which way it went.
- Every transaction that did not reach `COMMIT` is gone after a crash.

## The disk this assumes

Fishbowl promises the above for a disk that behaves like this, and the
[crash test](crash-harness.md) simulates exactly this disk:

1. A write made before a file's last finished sync survives.
2. Each write since that sync may or may not survive, independently of the
   others, in any order.
3. The write in flight at the crash may be torn: any prefix of it survives.
   If it was making the file longer, the file may also have its new length,
   with zeros after the prefix, since some file systems record a file's
   length before its data.

It does not cover a disk that loses synced data, which some drives with
write caches do, or one that flips bits.

## Every sync Fishbowl makes

| When | Synced | Why |
| --- | --- | --- |
| Each commit | the log | the moment the transaction becomes durable |
| Checkpoint, after copying pages | the database file | must be safe before the log is emptied |
| Checkpoint, after emptying the log | the log | so an old frame can never show up behind a new one |
| Creating either file | its directory | the file's name lives in the directory, and must survive too |

A commit that changed nothing writes nothing and syncs nothing.

## When a write or sync fails

After a failed write or sync, nobody knows what reached the disk. The
operating system may even have dropped the unwritten data, so a retry can
report success while the data is gone; PostgreSQL found this out in 2018.
So Fishbowl does not retry. After any I/O error while committing or
checkpointing, the database handle refuses every later statement with
"reopen the database to recover". Reopening runs
[recovery](crash-recovery.md), which reads what is really on the disk.

## In the browser

The browser build keeps both the database and the log in memory, and a sync
does nothing. The code is the same, but nothing outlives the tab.

Code: `crates/engine/src/storage/store.rs` (`FileStore`, `FileLog`),
`crates/engine/src/storage/pager.rs` (`Pager::fail`).
