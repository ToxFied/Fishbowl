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

## Join order

FROM order is not always a good loop order. In `FROM t51, t29, t31, t55` the
loops multiply every row of t51 by every row of t29 unless a condition ties
them together, and the test suite has joins of up to 64 tables written in
scrambled order. So for inner joins the planner picks the order with a simple
rule: start with a table that has a condition of its own, like `a29 = 6`,
then keep adding a table that a condition connects to the ones already
joined. `SELECT *` still lists columns in FROM order. With that rule all 732
queries in `select5.test` run in under half a second. A real cost model,
which counts rows instead of following a rule, is the cost-based planner's
job (M5). LEFT JOINs keep FROM order, since moving one changes its answer.

## What it cannot do yet

Each side of a join can already use an [index](indexes.md) for the
conditions on that table alone: `WHERE tank.id IN (1, 2)` reads two rows of
`tank` through its primary key. The join itself does not use one yet. An index
on the right table's join column could replace the inner loop with a lookup:
for each left row, jump straight to the matching right rows. Choosing that is
also the cost-based planner's job, which is why the plan says "joins do not
look up matching rows by index yet".

Code: `crates/engine/src/planner.rs` (`join_order`), `crates/engine/src/exec.rs`
(the NestedLoopJoin and Materialize cases
in `produce`, `Operator::replay`).
