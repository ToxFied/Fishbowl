---
title: "Sorting, LIMIT and DISTINCT"
description: "The Sort, Distinct and Limit steps, and when an index makes the sort free."
---

Three steps shape the rows a query returns: Sort puts them in order, Distinct
drops repeats, and Limit cuts the list short. In the plan they sit above the
scan and filter, in this order:

```text
Limit 3                      stop after three rows
  Distinct                   drop rows already returned
    Project name, weight     compute the output columns
      Sort weight DESC       read everything, then return it in order
        SeqScan fish
```

## Sort has to see everything first

Most steps hand a row up as soon as they have one. Sort cannot: the lightest
fish might be the last row in the table, so Sort reads its whole input before
it returns anything. Databases call this a *blocking* step. In the UI you can
see it: the Sort step's input count equals the table size even when Limit asks
for only three rows.

That is also why indexes matter for ORDER BY. An index keeps rows in order
already, so the planner can skip the Sort step entirely, and then a LIMIT
stops the scan as soon as it has enough rows. `ORDER BY id LIMIT 5` on a table
with an INTEGER PRIMARY KEY reads 5 rows, not all of them. The planner weighs
it against a sort (see [choosing an index](choosing-an-index.md)). Index
entries are not read backwards yet, so ORDER BY ... DESC always sorts.

## What order, exactly

- NULL sorts first ascending and last descending, because SQLite treats NULL as
  smaller than any value. `NULLS FIRST` and `NULLS LAST` override that.
- Numbers come before text. Text compares byte by byte, so `'Z'` sorts before
  `'a'`.
- Rows that tie keep the order they arrived in. This is called a *stable* sort.
  SQLite's sort is stable too, and the test suite notices if Fishbowl's is not.

ORDER BY accepts more than column names. `ORDER BY 2` means "the second result
column", and `ORDER BY total` can name a result column's alias, even when a
table column has the same name.

## Limit stops early

`LIMIT 3 OFFSET 2` skips two rows, then returns three. Once Limit has its
three rows it stops asking for more, so without a Sort below it, the scan
reads only five rows of the table. A negative LIMIT means no limit. A LIMIT
that is not a whole number, like `1.5` or `'abc'`, is an error: "datatype
mismatch".

## Distinct remembers what it has seen

Distinct keeps a set of the rows it has already returned and drops any row
that is in the set. For this step, two NULLs count as equal. That differs from
`=`, where `NULL = NULL` is unknown. See [NULL and three-valued
logic](null-logic.md).

Try the "heaviest" example in the UI.

Code: `crates/engine/src/exec.rs` (`sort`, the Limit and Distinct cases in
`produce`), `crates/engine/src/planner.rs` (`sort_keys`, `result_term`).
