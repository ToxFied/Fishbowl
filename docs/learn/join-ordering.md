---
title: "Join ordering"
description: "How the planner picks which table to read first in a join."
---

A [nested-loop join](nested-loop-join.md) of three tables can run in six
orders, and they can differ in work by a factor of millions. Join ordering
is choosing one.

## Lookups change everything

Take `feeding JOIN fish ON feeding.fish = fish.id JOIN tank ON fish.tank =
tank.id WHERE tank.name = 'reef'`. Read in FROM order, every feeding row
looks up its fish, then its tank, and only then is the tank name checked.
Start from the other end instead: find the one reef tank, its fish, then
only those fishes' feedings, through an index on `feeding(fish)`. For each
row on the left side, the right side is no longer read whole. Its matching
rows are looked up:

```text
NestedLoopJoin
  ...tank, then fish...
  IndexScan feeding USING INDEX feeding_fish (fish = fish.id)
```

The lookup's value, `fish.id`, comes from the left row, so it runs again
for each one. Without such an index, the right side is read once into a
Materialize step and replayed from memory for each left row.

## How the planner searches

The planner builds the order one table at a time. Adding a table costs:

- for a lookup, its cost for one left row, times the rows on the left;
- otherwise, reading the table once, plus replaying its rows for each left
  row.

It also estimates how many rows come out: the rows so far, times the new
table's rows, times the share each newly complete condition keeps.

The search keeps, for each set of tables joined so far, the cheapest order
found to join them. Two orders that join the same tables lead to the same
future, so only the cheaper one is worth extending. That is dynamic
programming, and it finds the cheapest order for up to 8 tables. Beyond 8,
the number of sets grows too fast, so at each step it keeps only the 16
cheapest, like SQLite's "N best paths". `select5.test` joins up to 64
tables this way.

When two orders cost the same, the one with fewer rows along the way wins,
then the one closest to FROM order.

## What may not move

- A LEFT JOIN's right table comes after every table before it in FROM. It
  must see each of their rows to know which to pad with NULLs.
- Only the LEFT JOIN's ON condition may narrow how its right table is read.
  A WHERE condition on that table has to see the padded rows, so it waits
  until after the join.
- `SELECT *` lists columns in FROM order, whatever the join order.

## Seeing the choice

The top join in the plan shows the order chosen, its estimate, and the best
orders it turned down. In the browser, open "why this plan" on the "join
order" example.

Code: `crates/engine/src/planner/join.rs`.
