# The iterator model

Fishbowl executes plans with the iterator model, also called the Volcano model
after the 1990s research system that popularized it. Most databases you have
heard of, including PostgreSQL and SQLite, work in a similar way.

Every step in a plan offers one operation: `next()`, "give me your next row".

- A **scan** returns the next row of the table.
- A **filter** calls `next()` on its input until it gets a row that passes the
  condition, then returns that row.
- A **projection** calls `next()` on its input and computes the output columns.

The executor calls `next()` on the top step until it says "no more rows". Each
call ripples down the tree and a single row bubbles back up.

## Why this design

- **Memory stays small.** Only one row per step is in flight. A query over a
  huge table never needs the whole table in memory.
- **Steps compose.** Any step can sit on top of any other, so new features are
  new steps, not changes to old ones.
- **It is easy to watch.** Because rows move one at a time, each step can count
  what passed through it. That count is the number shown next to every plan
  node in Fishbowl's UI.

The cost is one function call per row per step, which is slow for analytics
over billions of rows. Engines built for that (DuckDB, for example) pass batches
of rows instead. For a database you want to watch, one row at a time is ideal.

Code: `crates/engine/src/exec.rs`. Each step also charges a shared "row budget",
so a runaway query stops with an error instead of hanging.
