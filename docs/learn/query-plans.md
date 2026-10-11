# What a query plan is

SQL says *what* you want, not *how* to get it. A query plan is the "how": a
tree of small steps that together produce the answer.

```text
Project name                 compute the output columns
  Filter (id > 1)            keep rows where the condition is true
    SeqScan fish             read every row of the table
```

Read it from the bottom up. The scan produces rows, the filter drops some, and
the projection shapes what is left. Each step only knows how to ask the step
below it for the next row.

## Why plans matter

The same query can have many correct plans that differ hugely in speed. If
`fish` had an index on `id`, the planner could jump straight to the rows with
`id > 1` instead of reading every row. On a table of a million rows that is the
difference between microseconds and seconds. Choosing well is the planner's job,
and it chooses by estimating the cost of each option.

Fishbowl has indexes (milestone 4) but no cost model yet. It follows one rule:
if a WHERE term is an equality or range on an indexed column, read the index
(an IndexScan); otherwise read every row (a SeqScan). The plan's "why" line
says which applied. Milestone 5 adds a cost model, and the UI will show the
rejected plans next to the chosen one. See [indexes](indexes.md).

## Seeing plans in Fishbowl

The CLI prints the plan after each query, with how many rows each step
produced. The web UI draws the same tree and lets you step through it.
Other databases show this with `EXPLAIN` (the plan) and `EXPLAIN ANALYZE` (the
plan plus what actually happened).

Code: `crates/engine/src/plan.rs`, `crates/engine/src/planner.rs`.
