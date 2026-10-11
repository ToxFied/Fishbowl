# sqllogictest results

Fishbowl runs SQLite's [sqllogictest](https://www.sqlite.org/sqllogictest)
suite at check-in
[`ab599cbdd0976a63c5cfc7755588f716a7621f27`](https://www.sqlite.org/sqllogictest/info/ab599cbdd0976a63c5cfc7755588f716a7621f27)
(2026-10-07). There are two runs: the full suite gives the headline number,
and a 29-file sample is the fast check. [The harness notes](../docs/sqllogictest.md)
explain what counts as a pass.

| Corpus | Files | Records run | Failed | Skipped | Pass rate | Queries only |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Full suite | 622 | 5,939,879 | 452 | 1,480,834 | **99.99%** | 99.99% |
| 29-file sample | 29 | 129,348 | 125 | 14,153 | **99.90%** | 99.91% |

- **Full suite:** every test file in the upstream repository at that
  check-in, listed with SHA-256 in [`manifest-full.txt`](manifest-full.txt).
  It takes about 35 seconds on a 10-core Apple M4 and needs 1.2 GB of disk.
- **29-file sample:** a subset of the same files, listed in
  [`manifest.txt`](manifest.txt), with at least one file from each category.
  It runs in about 8 seconds.
- **Skipped** records are ones the test file marks as not applying to SQLite:
  `onlyif mysql`, `onlyif mssql`, `onlyif oracle` and `skipif sqlite`. They
  are neither run nor counted as passes.
- Pass rates are rounded down. A `cargo test` checks that this table and the
  README match the generated reports.

The generated reports, with every file's numbers: [full suite](full.md)
([JSON](full.json)) and [sample](sample.md) ([JSON](sample.json)).

## What the suite covers

Most of the full suite is generated. Of the 5.9 million records run, 3.9
million come from `random/` (random expressions, aggregates, `GROUP BY` and
joins over small tables) and 2.1 million from `index/` (the same query run
with and without indexes, over 1 to 10,000 rows). The hand-written
`select1` to `select5` files add 10,706 and the 12 `evidence/` files 489,
each tied to a sentence in SQLite's documentation.

So a high pass rate says Fishbowl answers this kind of SQL the way SQLite
does: joins, subqueries, compound queries, aggregates, NULL handling, type
affinity and index use. It says nothing about SQL the suite never uses, such
as window functions, common table expressions, `ALTER TABLE` or JSON.

## What fails and why

All 452 failures in the full suite, grouped by cause. Each cause is in the
engine, not the harness. The lane names come from [the plan](../docs/PLAN.md).

| Records | Cause | Where | Likely fix |
| ---: | --- | --- | --- |
| 335 | Deeply nested `WHERE` clauses hit the parser's recursion limit. `sqlparser` stops at 50 levels by default. | Generated queries in `index/between`, `in`, `commute` and `orderby_nosort` | Lane A: raise the limit where `parse()` in `crates/engine/src/lib.rs` builds the parser. Check stack depth in the planner, evaluator and browser build when doing so. |
| 72 | `x IN t1`, SQLite's shorthand for `x IN (SELECT * FROM t1)`, does not parse. | `evidence/in1.test` | Lane A: accept the form in the parser or rewrite it before parsing. |
| 27 | Triggers: `CREATE TRIGGER` (13) and `DROP TRIGGER` (14) are not supported. | `evidence/slt_lang_createtrigger.test`, `slt_lang_droptrigger.test` | New feature, unassigned. |
| 7 | `min(DISTINCT x)` or `max(DISTINCT x)` next to plain columns is rejected. | `random/groupby/slt_good_12.test` | Lane A: aggregates in `planner.rs`. |
| 6 | `INDEXED BY` and `NOT INDEXED` after a table name do not parse. | `evidence/slt_lang_aggfunc.test` | Lane A: parser; the planner can ignore the hint. |
| 4 | Blob literals such as `X'303132'` are not supported. | `evidence/in1.test` | Lane A: a blob value type. |
| 1 | `REINDEX` does not parse. | `evidence/slt_lang_reindex.test` | Lane A: parse it and do nothing. Index B-trees never go stale. |

Every failure is an error. No record in 5.9 million gives a wrong result,
panics the engine or runs over the 20 million row budget.

The same causes make up the sample's 125 failures: 72 `x IN t1`, 27 trigger
records, 15 recursion-limit records, 6 `INDEXED BY`, 4 blob
literals and 1 `REINDEX`.
