---
title: "What a query plan is"
description: "The tree of steps that says how a query will be answered."
---

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

Fishbowl's planner (milestone 5) lists the ways to read each table and the
orders to join them in, estimates the cost of each with a
[cost model](cost-models.md), and keeps the cheapest. The plan says why, with
the estimate and the plans it turned down:

```text
IndexScan fish USING INDEX fish_species (species = 'koi')  [rows=3]
  why: index fish_species finds about 2 of the 8 rows, and each leads to its row by rowid
  estimate: rows=2 cost=14
  rejected: SeqScan fish  [rows=8 cost=24]
```

The UI shows the same side by side under "why this plan". See
[choosing an index](choosing-an-index.md) and [join ordering](join-ordering.md).

## Seeing plans in Fishbowl

The CLI prints the plan after each query, with how many rows each step
produced. The web UI draws the same tree and lets you step through it.
Other databases show this with `EXPLAIN` (the plan) and `EXPLAIN ANALYZE` (the
plan plus what actually happened).

Code: `crates/engine/src/plan.rs`, `crates/engine/src/planner.rs`.
