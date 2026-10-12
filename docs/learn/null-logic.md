---
title: "NULL and three-valued logic"
description: "Why NULL means unknown, and how true, false and unknown combine."
---

`NULL` in SQL means "unknown". It is not zero and not an empty string. That one
idea has surprising consequences.

If a fish's weight is unknown, is it greater than 1? Unknown. So `NULL > 1` is
`NULL`, not false. Even `NULL = NULL` is `NULL`: two unknown values are not
known to be equal. To test for NULL you write `weight IS NULL`, which always
answers true or false.

Logic therefore has three values: true, false and unknown.

| a | b | a AND b | a OR b |
| --- | --- | --- | --- |
| true | unknown | unknown | true |
| false | unknown | false | unknown |
| unknown | unknown | unknown | unknown |

The rule of thumb: if the known side already decides the answer, the unknown
side does not matter. `false AND anything` is false. `true OR anything` is true.

## Where it bites

`WHERE` keeps a row only when the condition is **true**. Unknown counts as not
true. So `SELECT * FROM fish WHERE weight > 1` skips fish with no weight, and so
does `WHERE NOT (weight > 1)`. A row can vanish from both a query and its
opposite. That is correct SQL, and a common source of bugs.

Try it in the UI with the "nulls" example.

## NULL in other places

The same "unknown" idea runs through the rest of SQL, with a few twists:

- **IN.** `2 IN (1, NULL)` is unknown: 2 is not 1, but it might be the missing
  value. `1 IN (1, NULL)` is true. `x IN ()`, with an empty list, is false
  even when x is NULL.
- **BETWEEN and CASE.** `x BETWEEN a AND b` means `x >= a AND x <= b`, so
  NULL anywhere can make it unknown. `CASE x WHEN NULL` never matches, since
  `x = NULL` is never true. Write `CASE WHEN x IS NULL` instead.
- **Aggregates skip NULL.** `count(weight)` counts the known weights, while
  `count(*)` counts rows. `avg(weight)` averages only the known weights.
- **Grouping treats NULLs as equal.** GROUP BY puts all the NULLs in one group,
  and DISTINCT keeps one NULL. Here "equal" means "the same for grouping
  purposes", not `NULL = NULL`.
- **Sorting puts NULL first.** SQLite treats NULL as smaller than every value,
  so it comes first in ascending order and last in descending order.
- **`IS DISTINCT FROM`** is `=` with NULL treated as an ordinary value:
  `NULL IS NOT DISTINCT FROM NULL` is true, and the answer is never unknown.

Code: `crates/engine/src/expr.rs` (`eval_binary`, `eval_in_list`),
`crates/engine/src/expr/aggregate.rs`, and `Value::truthiness` in
`crates/engine/src/value.rs`.
