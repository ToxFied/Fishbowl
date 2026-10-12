---
title: "Limitations"
description: "What Fishbowl does not do yet. Unsupported SQL fails loudly instead of answering wrong."
---

Anything outside what Fishbowl supports is rejected with "not supported yet"
rather than answered wrongly. The suite never uses window functions, common
table expressions or `ALTER TABLE`, so passing it does not mean all of SQL
works.

## Known gaps

- A table is read through one index at a time, never through two at once,
  and never for an `OR` of conditions. Index entries are not read backwards,
  so `ORDER BY ... DESC` always sorts.
- Statistics live in memory. Reopening a database file forgets them until
  `ANALYZE` runs again.
- A join with no usable index replays its right side from memory for every
  left row. There is no hash join, and no automatic index as in SQLite.
- Index entries are limited to 512 bytes, so a long text value in an indexed
  column is rejected with "not supported yet".
- `AUTOINCREMENT`, `WITHOUT ROWID`, partial and expression indexes, `UPDATE OR`
  and `ON CONFLICT` clauses are rejected.
- One transaction's changed pages must fit in memory, since they stay there
  until it commits.
- One connection per database. There is no file locking, so two processes
  using one file at once would damage it.
- Savepoints (`SAVEPOINT`, `RELEASE`, `ROLLBACK TO`) are rejected.
- A failed statement inside a transaction always undoes itself. SQLite keeps
  the partial work of a failed `INSERT OR REPLACE`, `OR IGNORE`, `OR FAIL` or
  `OR ROLLBACK` there, because it skips its statement journal for them.
- A database is not checkpointed when it is closed. Its log stays next to the
  file until the next open recovers it, so copying the `.db` file alone can
  miss recent commits.
- The browser download is about 1 MB gzipped (engine 921 KB, UI 84 KB),
  plus 141 KB of fonts. Most of the engine is `sqlparser`. `wasm-opt` and
  trimming dialects are possible later.
- RIGHT and FULL joins, `USING`, `NATURAL`, subqueries in `UPDATE`/`DELETE`,
  and an aggregate of only an enclosing query's columns are rejected as "not
  supported yet".
- The web UI has no view of the write-ahead log or of recovery yet.
