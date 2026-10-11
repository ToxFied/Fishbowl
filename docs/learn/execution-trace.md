# The execution trace

The plan tree says how many rows each step produced. The trace says which
rows, and in what order. It is the recording the web UI plays back when you
press play.

## What gets recorded

The [iterator model](iterator-model.md) moves one row at a time, so every
step can report what it did with each row. A trace is a list of events in the
order they happened. Each event names a step, says what it did and carries
the row:

| Event | Meaning |
| --- | --- |
| `out` | The step passed the row up to the step above it |
| `drop` | The step read the row and threw it away |
| `run` | A subquery started a run, for the outer row it carries |
| `reuse` | A subquery that does not depend on the outer row handed back its first answer |

There is no "read" event. A step reads a row exactly when the step below it
passes one up, so the child's `out` is the parent's read. The parent's next
event is its verdict on that row.

For this query against the 8-row `fish` table:

```sql
SELECT name, weight FROM fish WHERE species = 'koi' AND weight > 1;
```

Rows are cut down to a few columns here:

```text
IndexScan  out   #2 bubbles koi 1.5     the index points at row 2
Filter     out   bubbles koi 1.5        weight > 1, keep it
Project    out   bubbles 1.5            into the result
IndexScan  out   #6 koko koi 2.4
Filter     out   koko koi 2.4
Project    out   koko 2.4
IndexScan  out   #8 pearl koi NULL
Filter     drop  pearl koi NULL         NULL > 1 is not true
```

Eight events. The other five fish never appear, because the index never led
to them. The trace also lists every row of each scanned table, so the UI can
show those five as untouched.

## Which steps drop rows

| Step | Drops |
| --- | --- |
| Filter | Rows where the condition is false or NULL |
| NestedLoopJoin | Pairs that fail the ON condition |
| Distinct | Rows it has already passed on |
| Limit | The rows it skips for OFFSET |

Sort, Aggregate and Materialize drop nothing, but they pass nothing on until
their input runs out. In the trace you see their input's `out` events pile
up first, then their own.

## Joins and subqueries

A join's events show the nested loop directly. One left row arrives, then
the right side hands up its rows one by one, and after each one the join says
`out` (a match) or `drop` (no match). When the right side runs out, the next
left row arrives and the right side starts over. Under a Materialize step
that replay comes from memory, so the table scan below it reports each row
once while Materialize reports it once per left row. See [the nested-loop
join](nested-loop-join.md).

A correlated subquery reports `run` once per outer row, followed by its own
steps' events. An uncorrelated one reports a single `run`, then `reuse` for
every later row. See [subqueries](subqueries.md).

## Paying nothing when it is off

Most queries are never watched: the sqllogictest harness runs 129,348
records, and the CLI only prints results. So tracing is off unless
`Database::set_trace` turns it on, and the executor holds the recorder as an
`Option`. When it is `None`, recording an event is one check of that option.
No row is copied and nothing is stored. The harness runs as fast as it did
before the trace existed.

A trace is capped at a limit you choose: the browser keeps 5,000 events, so
a big join cannot send megabytes of JSON. Past the limit the trace is marked
`truncated` and keeps the first events. The row counts in the plan are always
complete.

## How it is tested

`crates/engine/tests/trace.rs` checks that the trace agrees with the plan:

- Every step's `out` events number exactly its row count, and each
  subquery's `run` events its run count.
- The root's `out` rows are the query's result, in order.
- For every Filter, its `out` and `drop` rows, in order, are exactly the rows
  its input passed on.
- Turning tracing on changes no result and no plan.

The last check was also run over the whole sqllogictest corpus with tracing
forced on, and the report came out byte for byte the same.

Code: `crates/engine/src/trace.rs` (the event types and recorder),
`crates/engine/src/exec.rs` (where each step records), `web/src/trace.ts`
(playback).
