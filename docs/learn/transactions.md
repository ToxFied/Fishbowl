# Transactions

A **transaction** is a group of changes that happen together or not at all.
The classic example moves money between two accounts:

```sql
BEGIN;
UPDATE accounts SET balance = balance - 10 WHERE id = 1;
UPDATE accounts SET balance = balance + 10 WHERE id = 2;
COMMIT;
```

If only the first UPDATE happened, ten units of money would vanish. A
transaction rules that out, even if the power goes out between the two.

- `BEGIN` starts a transaction. `BEGIN TRANSACTION`, `BEGIN DEFERRED`,
  `BEGIN IMMEDIATE` and `BEGIN EXCLUSIVE` all mean the same thing here,
  because only one connection ever uses a database.
- `COMMIT`, or `END`, keeps every change. Once it returns, the changes survive
  a crash. See [durability](durability.md).
- `ROLLBACK` forgets every change since `BEGIN`: rows, and also tables and
  indexes that were created or dropped.

Inside a transaction, queries see the transaction's own changes.

## Autocommit

Outside `BEGIN`, every statement is a transaction of its own, which commits
as the statement ends. SQLite calls this **autocommit**. It is why a single
`INSERT` of a thousand rows is all or nothing even without `BEGIN`.

## When a statement fails

A statement that fails undoes its own changes (see
[constraints](constraints.md)), and the transaction carries on. Two kinds of
error end the whole transaction instead, as in SQLite:

- a broken constraint under `INSERT OR ROLLBACK`;
- a storage error, such as a failed disk write. After one, the statement's
  own undo cannot be trusted.

The other errors match SQLite's wording:

| Statement | Error |
| --- | --- |
| `BEGIN` inside a transaction | `cannot start a transaction within a transaction` |
| `COMMIT` outside one | `cannot commit - no transaction is active` |
| `ROLLBACK` outside one | `cannot rollback - no transaction is active` |

## How it works

There is no undo log for a whole transaction. The pager simply keeps every
changed page in memory until the transaction ends (see the
[buffer pool](buffer-pool.md)):

- `COMMIT` hands all the changed pages to the
  [write-ahead log](write-ahead-log.md) in one go.
- `ROLLBACK` throws them away. The storage layer and the catalog also go
  back to the lists of tables and indexes they saved at the first change.

So rolling back is cheap, and a crash can never leave half a transaction in
the database file, because nothing of it was ever written there.

## One difference from SQLite

SQLite keeps an undo journal for a single statement only when it thinks the
statement might fail halfway, and it does not think that of `INSERT OR
REPLACE`, `OR IGNORE`, `OR FAIL` or `OR ROLLBACK`. When such a statement fails
anyway, for example on an `INTEGER PRIMARY KEY` value that is not an integer,
SQLite inside a transaction keeps the rows it had already written. Fishbowl
undoes them, as it does for every failed statement. The differential test
against `sqlite3` found this, and avoids that one case.

## Not supported yet

- Savepoints: `SAVEPOINT`, `RELEASE` and `ROLLBACK TO` are rejected.
- More than one connection. There is no locking, so two processes that open
  the same file at once would damage it.

Code: `crates/engine/src/lib.rs` (`Database::execute_statement`),
`crates/engine/src/storage/mod.rs` (`Storage::commit`, `Storage::rollback`),
`crates/engine/src/storage/pager.rs`. Tests: `crates/engine/tests/transactions.rs`.
