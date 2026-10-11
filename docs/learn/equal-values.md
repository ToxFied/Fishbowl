# Why 2 and 2.0 matter

In SQL, `2 = 2.0` is true. The integer and the real are equal. But they
print differently. So what should this print?

```sql
CREATE TABLE t(v);
INSERT INTO t VALUES (2), (2.0);
SELECT DISTINCT v FROM t;
```

DISTINCT keeps one of two equal rows. Which one? SQL does not say. SQLite
keeps the **first one it reads**. The same goes for GROUP BY (a group shows
the values of its first row) and for UNION, INTERSECT and EXCEPT.

## The order depends on the plan

"First one read" depends on how the table is read:

- A full scan reads in rowid order: `2` first.
- An index on `(v, w)` reads in that order, so equal `v` values come out
  sorted by `w`. If the `2.0` row has the smaller `w`, it comes first.

SQLite reads through an index whenever it is cheaper. That happens more
often than you might think: when the index holds every column the query
needs (a covering index), or when its order saves the sort that GROUP BY,
DISTINCT or UNION would need. So the same query can print `2` or `2.0`
depending on what the planner chose.

## Why Fishbowl copies SQLite's choices

Fishbowl is measured against SQLite: sqllogictest's expected answers, and a
differential test that runs random scripts through both and compares every
row. Before M5, Fishbowl read the table where SQLite read an index, and about
one random script in 40,000 printed `2` where SQLite printed `2.0`, or the
other way round. At 256 scripts a run, the test failed now and then.

The fix is not a rule about which value wins. It is reading rows in the
same order SQLite does. Fishbowl's planner uses SQLite's cost model and
SQLite's size guesses, tries the plans in SQLite's order, and asks for the
same orders SQLite asks for:

- ORDER BY, which an ordered index saves a Sort step;
- GROUP BY and DISTINCT on plain columns, in any column order;
- each side of a UNION, INTERSECT or EXCEPT, which SQLite merges sorted.

The last two buy Fishbowl no speed. Its GROUP BY and UNION sort with a tree
whatever the order. They are there so the first row read is SQLite's first
row read.

This is also why the planner has no statistics until `ANALYZE` runs. If the
plan changed as tables grew, the answers above would too.
`crates/engine/tests/sqlite_differential.rs` keeps the cases the random test
found, one per kind of plan.

Code: `crates/engine/src/planner.rs` (`wanted_order`),
`crates/engine/src/planner/paths.rs`.
