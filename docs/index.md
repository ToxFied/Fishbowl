---
title: "Overview"
description: "A SQL database written from scratch in Rust that runs in your browser and shows each query as it runs."
---

Fishbowl compiles to WebAssembly and runs in a browser tab. Type a query and
watch it run row by row: each row rises out of its table, through every step
of the plan, into the result. You can see why the planner chose each step,
which rows an index let the query skip, and the B-tree pages the data lives on.

[Open the live demo](https://toxfied.github.io/Fishbowl/). It runs entirely
in your browser, with no server.

![A join paused mid-trace: the nested-loop join tries fish row 5 against tank row 3](assets/trace-join.png)

## Correct on SQLite's own tests

Fishbowl passes **100%** of SQLite's sqllogictest suite: 5,939,879 records
across 622 files, in about 40 seconds. Every file is pinned by SHA-256, and
"not supported yet" counts as a failure. [The harness notes](sqllogictest.md)
explain how the number is measured.

## What is inside

- A parser front end (`sqlparser`), then name resolution and a cost-based
  planner using SQLite's cost model and statistics from `ANALYZE`.
- An iterator-model executor with joins, subqueries, compound SELECTs, views,
  sorting, grouping, aggregates and triggers.
- B+tree tables and indexes on 4 KiB pages, a buffer pool, and a write-ahead
  log with crash recovery, checked by a test that cuts the power at every
  write.
- A React UI that plays back the execution trace and draws the B-tree pages.

## Where to go next

- [Getting started](getting-started.md): build it and run the SQL shell, the
  browser UI and the test suite.
- [Case study](case-study.md): the decisions, the testing, and what Fishbowl
  still gets wrong.
- [How it works](learn/index.md): short notes on each idea, from pages and
  B+trees to join ordering and crash recovery.
