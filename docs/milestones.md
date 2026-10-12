---
title: "Milestones"
description: "How Fishbowl was built, one milestone at a time, with the pass rate after each."
---

Fishbowl was built in nine milestones. Each ran on its own branch and merged
when its tests passed. Independent ones, such as storage and query breadth,
ran at the same time. Pass rates are from the 29-file sqllogictest sample
until M8, then from the full suite.

| Milestone | Pass rate after |
| --- | ---: |
| M0. Foundation | 11.90% |
| M1. Query breadth | 31.04% |
| M3. Storage engine | 31.06% |
| M4. Indexes | 84.31% |
| M2. Joins, subqueries and compound queries | 99.89% |
| M6. Transactions and crash recovery | 99.89% |
| M7. Visual depth | 99.89% |
| M5. Cost-based planner | 99.89% |
| M8. Proof: the full suite | 100.00% of 5,939,879 records |

## M0. Foundation

A workspace, an in-memory engine slice (`CREATE TABLE`, `INSERT`, `SELECT`
with `WHERE`), the CLI, the WebAssembly build, a UI with the plan tree and
step-through, and the sqllogictest harness with a pinned 29-file corpus.
Baseline: **11.90%** of records.

## M1. Query breadth

`ORDER BY`, `LIMIT`/`OFFSET`, `DISTINCT`, aggregates with `GROUP BY` and
`HAVING`, `CASE`, `BETWEEN`, `IN (list)`, `CAST`, `COALESCE`, `NULLIF`, `ABS`
and other scalar functions, and comparison affinity. New plan steps: Sort,
Limit, Aggregate and Distinct.

## M2. Joins, subqueries and compound queries

Inner, cross and LEFT joins, with conditions pushed down to the table they
read. NestedLoopJoin and Materialize steps. Scalar, `IN` and `EXISTS`
subqueries, correlated ones included, drawn under the step that runs them.
Subqueries in `FROM`. Compound SELECTs, run left to right as in SQLite.
`CREATE VIEW` and `DROP VIEW`, saved in the schema table. Each table in a join
could use an index for its own conditions, and inner joins followed a simple
order rule until M5 brought costs.

With M4 in place, M2 took the sample to **99.89%**. The four wrong results
left were `1<<63` rows: `<<` was not supported yet.

## M3. Storage engine

A pager over one database file with fixed 4 KiB pages and a small buffer pool.
Row encoding. B+tree tables keyed by rowid, with insert, delete, split and
merge. `UPDATE`, `DELETE` and `DROP TABLE`. A page-store trait with a file
backend and a memory backend for the browser. Property tests that compare the
B-tree with Rust's `BTreeMap`.

## M4. Indexes

`CREATE INDEX`, `PRIMARY KEY` and `UNIQUE`, index B-trees keyed by (values,
rowid), index maintenance on every write, and an IndexScan step. Also:
`INTEGER PRIMARY KEY` as the rowid, `DROP INDEX`, `INSERT ... SELECT`,
`INSERT OR REPLACE/IGNORE/FAIL/ABORT`, UPDATE and DELETE finding rows through
an index, `Database::integrity_check`, and a differential test against the
`sqlite3` CLI.

Most failures until then cascaded from `PRIMARY KEY` tables that could not be
created, so the pass rate jumped from 31% to **84.31%**.

## M5. Cost-based planner

`ANALYZE`, with SQLite's guesses until it runs. SQLite's cost model. Lookups on
the longest usable prefix of each index, `IN` lists included. Covering
indexes, which skip the table. An index or the rowid standing in for ORDER BY's
Sort, so `LIMIT` stops early. Index lookups inside joins and correlated
subqueries, with values from the left or outer row. Join order by cost, LEFT
JOINs included. UPDATE and DELETE costed the same way. The plans turned down,
shown under "why this plan" and in the CLI.

Also fixed: `2` versus `2.0` in groups (see
[why 2 and 2.0 matter](learn/equal-values.md)) and the bit operators `&`, `|`,
`<<`, `>>` and `~`. Every corpus record and every differential test gives the
same result with the old planner and the new one, the 2 versus 2.0 cases
aside.

## M6. Transactions and crash recovery

`BEGIN`, `COMMIT` and `ROLLBACK`, autocommit as in SQLite, a write-ahead log
with checksummed frames, checkpoints, and recovery on open. A transaction is
durable once `COMMIT` returns (see [durability](learn/durability.md)).

The [crash test](learn/crash-harness.md) cuts the power at 178 points and
reboots each crashed disk in several ways: unsynced writes lost, kept, torn or
random. It runs 7,189 recoveries, 3,370 of them after a second crash during
recovery. Eight bugs planted in the log code checked the test itself. The
differential test against `sqlite3` runs transactions too.

## M7. Visual depth

A row-level execution trace (`trace.rs`, `exec.rs`) and its playback: rows rise
from their table through each step into the result, joins play pair by pair,
and correlated subqueries run by run. The reason for each choice, and the rows
an index never touched next to what a full scan would read. A B-tree page
viewer (`storage/inspect.rs`) that shows splits and merges as rows are inserted
and deleted. A first screen that is already playing a query.

## M8. Proof

The full upstream suite: 622 files and 5,939,879 records, pinned by
`scripts/pin-sqllogictest.sh` through Fossil's SHA-1 chain. It passes
**100.00%** in about 40 seconds on 10 cores. The last 452 failures were parser
gaps, blobs and triggers; [RESULTS.md](https://github.com/ToxFied/Fishbowl/blob/main/sqllogictest/RESULTS.md)
lists what fixed each one. GitHub Actions runs format, lint, tests, the web
build and the sample on every push, and the full suite weekly. The
[case study](case-study.md) came last.
