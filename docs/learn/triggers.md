# Triggers

A trigger is a list of statements the database runs by itself whenever a
table changes. This one logs every fish that leaves the bowl:

```sql
CREATE TRIGGER fish_gone AFTER DELETE ON fish
BEGIN
  INSERT INTO gone VALUES (old.id, old.name);
END;
```

`DELETE FROM fish WHERE weight < 4` now also writes one `gone` row for each
fish it deletes. The code is in `crates/engine/src/trigger.rs`.

## When a trigger fires

SQLite has only row triggers: the body runs once for each row the statement
changes, never once per statement. For each row, Fishbowl:

1. runs the `BEFORE` triggers,
2. writes the row,
3. runs the `AFTER` triggers.

Several triggers on the same event fire newest first. `WHEN new.x > 0`
skips the trigger for rows where the condition is not true. `UPDATE OF price`
fires only when the `SET` list names `price`, whether or not the value
changes.

Inside the body, `NEW.column` is the row being written and `OLD.column` the
row being replaced or deleted. An INSERT has no `OLD` and a DELETE has no
`NEW`. Naming one is "no such column", and SQLite only reports it when the
trigger fires, not when it is created.

## How `NEW` and `OLD` get their values

A trigger keeps its body as SQL text. Each time it fires, Fishbowl replaces
each `NEW.x` and `OLD.x` with the row's value written as a literal, then
parses and plans the statement like any other. `INSERT INTO gone VALUES
(old.id, old.name)` runs as `INSERT INTO gone VALUES (7, 'nemo')`.

Planning once per row is slow, but it keeps triggers out of the planner and
the executor. It is also exact. A column reference normally brings its
column's [affinity](types-and-affinity.md) to a comparison, and SQLite's
documentation says `NEW.x` does too. SQLite 3.54 does not do that: with an
INTEGER column `x` holding 5, `NEW.x = '5'` is false. A bare literal gives
the same result.

## All or nothing, including the triggers

A statement and every trigger it fires share one undo log. If any of them
fails, such as a body INSERT that breaks a UNIQUE rule, Fishbowl reverses
all of it, newest change first. See [how constraints are
checked](constraints.md).

`RAISE()` lets a trigger stop the statement on purpose:

```sql
CREATE TRIGGER no_sharks BEFORE INSERT ON fish
WHEN new.species = 'shark'
BEGIN
  SELECT RAISE(ABORT, 'no sharks');
END;
```

- `RAISE(ABORT, msg)` fails the statement and undoes it.
- `RAISE(FAIL, msg)` fails it but keeps the rows written before.
- `RAISE(ROLLBACK, msg)` ends the whole transaction.
- `RAISE(IGNORE)` skips this row and the triggers still to fire for it, and
  the statement carries on.

The RAISE's own rule wins over the statement's `OR` clause, so `RAISE(FAIL)`
inside `INSERT OR ROLLBACK` keeps the earlier rows.

## Details that show

- **No recursion.** A trigger that is running does not fire again, even if
  its body writes to its own table. Other triggers still fire. This is
  SQLite's default, `PRAGMA recursive_triggers = OFF`.
- **Rowid order.** When an UPDATE or DELETE fires triggers, SQLite collects
  the matching rowids and visits them in order, whatever order its scan found
  them in. The order shows in what the triggers write, so Fishbowl does the
  same.
- **Rows can change under the statement.** A `BEFORE UPDATE` trigger may
  change the row about to be updated. The UPDATE then takes the columns it
  does not set from the row as the trigger left it. A row that an earlier
  trigger deleted is skipped.
- **An unassigned rowid is -1.** In a `BEFORE INSERT` trigger, `NEW.rowid`
  and an `INTEGER PRIMARY KEY` left NULL read as -1, because the row has no
  rowid yet.

## Views

A view holds no rows, so `INSERT INTO v` is normally an error. An `INSTEAD
OF` trigger makes it work: the statement writes nothing itself, and the
trigger fires once for each row it would have written, with `NEW` and `OLD`
holding the view's columns. The trigger decides what really changes.

## Storage

Like a view, a trigger is only its `CREATE TRIGGER` statement, kept in the
schema table with type `trigger`. Opening the database replays it, so
triggers survive a restart and crash recovery. Dropping a table or view drops
its triggers. A `TEMP` trigger lives in memory and is gone on close.

## Not supported yet

- `RAISE()` anywhere but as a result column of a trigger's SELECT, or as a
  `CASE` result inside one.
- A trigger body that writes to a view through its `INSTEAD OF` triggers.
- Qualified names, as in `CREATE TRIGGER main.t`.
