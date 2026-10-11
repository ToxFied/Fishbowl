# Aggregation: GROUP BY and HAVING

An aggregate function turns many rows into one value. `count(*)` counts rows,
`sum(weight)` adds up a column, `max(weight)` finds the largest. With GROUP BY,
the rows are first split into groups and each group gets its own answer:

```sql
SELECT species, count(*), avg(weight)
FROM fish
GROUP BY species;
```

## The Aggregate step

The plan gets an Aggregate step between the filter and the projection:

```text
Project species, count(*), avg(weight)
  Aggregate count(*), avg(weight) GROUP BY species
    SeqScan fish
```

Aggregate reads every input row, works out which group it belongs to, and
feeds it to that group's *accumulators*. An accumulator is a small running
total: `count` keeps a number, `sum` keeps a running sum, `max` keeps the
largest value so far. When the input runs out, each accumulator is finished
into one value and Aggregate returns one row per group.

Like Sort, Aggregate is blocking: no group is complete until the last row has
been read. Groups come out ordered by their GROUP BY values, which is the order
SQLite produces too.

Sometimes the plan says `GROUP BY species DESC`. When ORDER BY has as many
terms as GROUP BY, SQLite orders the groups in the ORDER BY's direction, hoping
the final sort has less to do. Groups that tie in ORDER BY keep that order, so
Fishbowl copies it to give the same answer.

## WHERE runs before grouping, HAVING after

`WHERE weight > 1` drops rows before they reach Aggregate. `HAVING count(*) > 1`
drops whole groups after. That is why HAVING can use aggregates and WHERE
cannot: when WHERE runs, there are no groups to count yet. In the plan, HAVING
is a second Filter step above Aggregate.

## Details that follow SQLite

- **No rows in, one row out.** `SELECT count(*) FROM fish WHERE 0` returns one
  row with 0, because without GROUP BY the whole table is one group, even when
  it is empty. `sum` of no rows is NULL. `total` of no rows is 0.0.
- **NULLs are skipped.** `count(weight)` counts only rows where weight is not
  NULL. `count(*)` counts every row.
- **sum stays exact as long as it can.** While every value is an integer, sum
  adds 64-bit integers and fails loudly with "integer overflow" if they
  overflow. As soon as a real number appears, it switches to floating point.
- **Plain columns are allowed.** SQLite accepts `SELECT species, name FROM fish
  GROUP BY species` even though `name` is not grouped. Each group shows the
  name from its first row. With exactly one `min()` or `max()`, it shows the row
  that holds the minimum or maximum instead, so `SELECT name, max(weight)` names
  the heaviest fish.

Try the "groups" example in the UI.

Code: `crates/engine/src/expr/aggregate.rs` (the accumulators),
`crates/engine/src/exec.rs` (`aggregate`), and `crates/engine/src/planner.rs`,
which finds the aggregates in a query and decides whether it is an aggregate
query at all.
