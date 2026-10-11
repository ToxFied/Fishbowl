# The nested-loop join

The simplest way to join two tables is two loops, one inside the other:

```text
for each row L of the left table:
    for each row R of the right table:
        if the condition holds for (L, R):
            output L followed by R
```

That is Fishbowl's NestedLoopJoin step. In the [iterator
model](iterator-model.md) it keeps one left row, asks the right side for its
next row, and returns the pair if it matches. When the right side runs out, it
fetches the next left row and starts the right side over.

For a LEFT JOIN it also remembers whether the current left row matched
anything. If not, it returns that row once, padded with NULLs, before moving
on.

## The cost is a product

With 1,000 rows on each side the condition is checked 1,000,000 times. With a
third table of 1,000 rows, a billion. Two things keep that in check:

- **Filters first.** Conditions on a single table run before the join (see
  [joins](joins.md)), so the loops only see rows that can match.
- **Materialize.** The right side sits under a Materialize step. It reads its
  input once and keeps the rows in memory, so starting the right side over is
  a walk through a list instead of another scan of the B-tree. PostgreSQL
  puts the same step in the same place.

You can watch the cost in the plan. In a self-join of the 8-row `fish` table,
the scan under Materialize reads 8 rows, and Materialize hands out 64: 8 for
each left row.

## What it cannot do yet

The tables are joined in the order FROM lists them. Put a big unfiltered
table first and the loops multiply its rows by everything after it. Two
queries in the test suite join 8 tables in such an order and run out of their
row budget. Choosing a better order is the cost-based planner's job (M5).

An index on the right table's join column would replace the inner loop with a
lookup: for each left row, jump straight to the matching right rows. That
needs indexes (M4), and is why the plan says "no index can find the matching
rows".

Code: `crates/engine/src/exec.rs` (the NestedLoopJoin and Materialize cases
in `produce`, `Operator::replay`).
