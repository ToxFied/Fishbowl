# Pages and the page store

A Fishbowl database is one file cut into **pages** of exactly 4,096 bytes.
Page 0 is the first 4 KiB of the file, page 1 the next 4 KiB, and so on. Every
read and every write moves a whole page, never a single row.

## Why fixed-size pages

- **Disks and operating systems already work in pages.** A 4 KiB page is the
  usual size of an OS memory page and of a disk block. Reading one costs about
  the same as reading one byte.
- **Addresses are simple.** Page `n` starts at byte `n × 4096`. A page number
  is all you need to find anything, so pages can point at each other by number.
- **Space is easy to reuse.** A freed page fits any later need, so the file
  never has odd-sized holes.

SQLite, PostgreSQL and most other databases are built the same way. SQLite also
uses 4 KiB by default.

## Kinds of page

The first byte of every page (after the header) says what it holds:

| Kind | Holds |
| --- | --- |
| Header (page 0) | The magic word `FISHBOWL`, the page size, and the start of the free list |
| Leaf | Rows, sorted by rowid. See [B+trees](b-plus-trees.md) |
| Interior | Signposts to other pages, also in [B+trees](b-plus-trees.md) |
| Index leaf and index interior | The same, for an index's (value, rowid) entries. See [indexes](indexes.md) |
| Overflow | The tail of a row too big for a leaf. See [row encoding](row-encoding.md) |
| Free | Nothing. It waits on the free list to be reused |

Page 1 is always the schema table: one row per table or index, with its kind,
its name, its table, its root page and the `CREATE TABLE` or `CREATE INDEX`
statement that made it. When Fishbowl opens a file, it reads that table and
replays the statements to rebuild its list of tables. SQLite does the same with
its `sqlite_schema` table, which has the same five columns.

Here are the first bytes of a real leaf page after
`INSERT INTO fish VALUES (1, 'goldie'), (2, 'nemo')`:

```text
01                         leaf page
00 02                      2 rows
00 00 00 00                no next leaf
00 00 00 00 00 00 00 01    rowid 1
00 00 00 16                the row is 22 bytes:
00 02 01 00 00 00 00 00 00 00 01 03 00 00 00 06 67 6f 6c 64 69 65
                           (1, 'goldie')
00 00 00 00 00 00 00 02    rowid 2
...
```

Numbers are stored big-endian, most significant byte first, so `00 02` reads
as 2 the way you would write it.

## The free list

Dropping a table or deleting rows can leave whole pages unused. Instead of
shrinking the file, Fishbowl puts those pages on a **free list**: the header
points to the first free page, each free page points to the next, and the last
one points to 0. The next time a page is needed, the first free page is taken
off the list. The file only grows when the list is empty.

## Where pages live: the page store

The pager, which manages pages, talks to storage through a tiny interface
called `PageStore`: read page `n`, write page `n`, count pages, sync. There are
two implementations:

- **`FileStore`** keeps pages in a real file. Used by the CLI with `--db`.
- **`MemoryStore`** keeps pages in a list in memory. A browser tab has no file
  system, so the web UI uses this one. The tests use it too.

Everything above the store, from the buffer pool to the B-trees, is the same
code in both cases. A later milestone adds a third store that fails on purpose
partway through a write, to test crash recovery.

Code: `crates/engine/src/storage/page.rs` (every page layout) and
`crates/engine/src/storage/store.rs` (the stores).
