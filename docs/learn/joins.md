# Joins

A join combines rows from two tables. Put two tables in FROM and every row of
the first is paired with every row of the second:

```sql
SELECT fish.name, tank.label
FROM fish, tank
WHERE fish.tank_id = tank.id;
```

With 8 fish and 3 tanks there are 24 pairs. WHERE keeps the pairs where the
fish is in that tank. That is all a join is: pairs of rows, then a filter.

## The kinds Fishbowl runs

- **Comma and CROSS JOIN** pair every row with every row.
- **JOIN ... ON** (or INNER JOIN) keeps the pairs where the ON condition is
  true. It means exactly the same as moving the condition into WHERE.
- **LEFT JOIN ... ON** keeps every row of the left table, even one that
  matched nothing. That row comes out once, with NULL in every column of the
  right table.

The difference between ON and WHERE only matters for LEFT JOIN. In
`t1 LEFT JOIN t2 ON t1.a = t2.a AND t2.c = 'r'` the ON condition decides which
t2 rows match, and t1 rows without a match still appear. Move `t2.c = 'r'`
into WHERE and those padded rows are dropped, because NULL = 'r' is not true.

RIGHT JOIN, FULL JOIN, USING and NATURAL are rejected as "not supported yet".

## Names

A joined row is the left row followed by the right row, so `t2.a` is just a
position further along. When two tables share a column name, a bare `a` is an
error ("ambiguous column name: a"); write `t1.a` or `t2.a`. A table with an
alias goes only by its alias: in `FROM fish AS f`, `fish.name` is unknown.

## Where conditions go

The planner splits WHERE into its AND-ed parts and puts each one as low in the
plan as it can:

```text
Project t1.a
  NestedLoopJoin INNER ON (t1.a = t2.a)     reads both tables
    Filter (t2.c = 'q')                     reads only t2
      SeqScan t2
    Materialize
      Filter t1.b IS NOT NULL               reads only t1
        SeqScan t1
```

t2 comes first because its condition is expected to keep fewer rows. Which
table goes first is [join ordering](join-ordering.md).

A condition that reads one table filters that table before the join, so fewer
rows are paired up. A condition that reads two tables waits for the join where
both are present. This is called *predicate pushdown*, and it is the
difference between pairing 3 rows and pairing 300. The one rule it must obey:
a WHERE condition on a LEFT JOIN's right table runs after that join, since it
has to see the NULLs.

How the pairs are made is the [nested-loop join](nested-loop-join.md).

Code: `crates/engine/src/planner.rs` (`plan_from`) and
`crates/engine/src/planner/join.rs` (`Analysis::build`).
