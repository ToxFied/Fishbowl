---
title: "How constraints are checked"
description: "How PRIMARY KEY, UNIQUE and NOT NULL are enforced on every write, and undone on failure."
---

A **constraint** is a rule the data must follow. Fishbowl checks three, on
every INSERT and UPDATE:

- `NOT NULL`: the column must have a value.
- `UNIQUE`: no two rows may share a value in the column, or in a group of
  columns like `UNIQUE (a, b)`.
- `PRIMARY KEY`: the row's identity. Like SQLite, Fishbowl treats it as
  `UNIQUE` (and NULL is allowed, an old SQLite quirk), except for
  `INTEGER PRIMARY KEY`, which is the rowid itself.

## UNIQUE is an index lookup

Checking "is this value already taken?" by reading every row would make each
insert slower as the table grows. So every `UNIQUE` and `PRIMARY KEY`
constraint gets an index, named like SQLite's `sqlite_autoindex_fish_1`.
Checking the rule is then a short walk down that index: seek to the new value
and see whether an entry with another rowid is already there.

`CREATE UNIQUE INDEX` makes the same kind of index by hand. On a table that
already holds duplicates, it fails and leaves nothing behind.

For an `INTEGER PRIMARY KEY`, the check is whether the table already has that
rowid. The value must be an integer: `'5'` becomes 5, but `5.5` or `'abc'` is a
"datatype mismatch", as in SQLite.

## NULL never clashes

`UNIQUE` allows any number of NULLs. In SQL, NULL means "unknown", and two
unknowns are not known to be equal. A key with a NULL in any of its columns is
skipped by the check.

## All or nothing

Constraints are checked one row at a time, against the table as it is at that
moment, the way SQLite does it. So if row 3 of an INSERT breaks a rule, rows 1
and 2 are already stored. The statement must still leave no trace.

The writer keeps an **undo log**: one entry per change, with what is needed to
reverse it ("inserted row 7", "deleted row 4, which held ..."). When the
statement fails, it walks the log backwards and undoes each change, table and
indexes together. When it succeeds, the log is thrown away. A
[transaction](transactions.md) can then still roll back the whole statement
along with the rest of its changes.

Checking row by row, rather than all rows at the end, matters for UPDATE.
`UPDATE t SET a = a + 1` on a UNIQUE column holding 1, 2 and 3 fails in SQLite:
row 1 becomes 2 while row 2 still holds 2. Fishbowl fails the same way. The
order rows are visited in therefore changes the outcome. SQLite visits them in
the order of the index it used to find them, unless the UPDATE changes a column
of that index; then it gathers the rows first and visits them in rowid order.
Fishbowl follows the same rule.

## Choosing what a clash does

`INSERT OR <rule>` changes what a broken constraint does:

| Rule | On a clash |
| --- | --- |
| `ABORT` (the default) | fail and undo the whole statement |
| `FAIL` | fail, but keep the rows already inserted |
| `IGNORE` | skip this row, carry on with the next |
| `REPLACE` | delete the rows it clashes with, then insert it |

`REPLACE INTO` is short for `INSERT OR REPLACE INTO`. `ROLLBACK` undoes the
statement like `ABORT`, then rolls back the rest of the transaction too. Outside
a transaction it is the same as `ABORT`.

## Checking the checks

Three tests guard this:

- A property test runs random INSERT, UPDATE and DELETE statements and, after
  each one, checks that every index holds exactly one entry per row with that
  row's values (`Database::integrity_check`, like SQLite's
  `PRAGMA integrity_check`). When a statement fails, the table must be
  unchanged.
- A differential test runs random scripts through Fishbowl and the real
  `sqlite3` and requires the same statements to fail and the same rows to come
  back. It found both subtleties above: `OR FAIL` keeps earlier rows only when
  a constraint breaks (a datatype mismatch still undoes everything), and the
  UPDATE visiting order.
- The sqllogictest corpus, whose index files are built on `PRIMARY KEY` tables.

Code: `crates/engine/src/write.rs` (`TableWriter`), constraint parsing in
`crates/engine/src/planner/schema.rs`.
