---
title: "UNION, INTERSECT and EXCEPT"
description: "How compound SELECTs combine, compare and subtract rows."
---

These combine the rows of two SELECTs that have the same number of columns:

| Operator | Result |
| --- | --- |
| `UNION ALL` | Every row of the left, then every row of the right |
| `UNION` | Every distinct row that is in either |
| `INTERSECT` | Every distinct row that is in both |
| `EXCEPT` | Every distinct row of the left that is not in the right |

The result's column names come from the leftmost SELECT.

## Sorted, because of how they work

UNION ALL just streams one input after the other. The other three have to
compare whole rows, so SQLite puts the rows in a temporary index and reads it
back. An index is sorted, so the answer comes out sorted, with NULL first,
then numbers, then text. Fishbowl does the same with an ordered set, so
`SELECT 3 UNION SELECT 1` returns 1, then 3.

`1` and `1.0` count as the same row but print differently, so which one comes
out is visible. In SQLite the first of equal rows from one side stays, and a
UNION's right side replaces an equal row from the left: `SELECT 1 UNION
SELECT 1.0` returns `1.0`. INTERSECT and EXCEPT return the left side's rows.
Fishbowl follows the same rules; the differential test against `sqlite3`
found them.

## Left to right

Standard SQL makes INTERSECT bind tighter than UNION, like `*` and `+`. SQLite
does not: every operator has the same precedence and they run left to right.

```sql
SELECT 1 UNION SELECT 2 INTERSECT SELECT 2;
-- SQLite: (1 UNION 2) INTERSECT 2  =  2
-- standard SQL: 1 UNION (2 INTERSECT 2)  =  1, 2
```

The parser follows the standard, so the planner flattens its tree back into
a list and rebuilds it left to right.

## ORDER BY

ORDER BY comes after the last SELECT and sorts the combined result. Each term
must pick a result column: by number (`ORDER BY 2`), by an `AS` name, or by
repeating a column's expression from any of the SELECTs. Anything else is an
error, "does not match any column in the result set".

Code: `crates/engine/src/planner.rs` (`plan_compound`, `compound_sort_keys`),
`crates/engine/src/exec.rs` (`combine`).
