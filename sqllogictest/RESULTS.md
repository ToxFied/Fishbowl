# sqllogictest results

Fishbowl runs SQLite's [sqllogictest](https://www.sqlite.org/sqllogictest)
suite at check-in
[`ab599cbdd0976a63c5cfc7755588f716a7621f27`](https://www.sqlite.org/sqllogictest/info/ab599cbdd0976a63c5cfc7755588f716a7621f27)
(2026-10-07). There are two runs: the full suite gives the headline number,
and a 29-file sample is the fast check. [The harness notes](../docs/sqllogictest.md)
explain what counts as a pass.

| Corpus | Files | Records run | Failed | Skipped | Pass rate | Queries only |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Full suite | 622 | 5,939,879 | 0 | 1,480,834 | **100.00%** | 100.00% |
| 29-file sample | 29 | 129,348 | 0 | 14,153 | **100.00%** | 100.00% |

- **Full suite:** every test file in the upstream repository at that
  check-in, listed with SHA-256 in [`manifest-full.txt`](manifest-full.txt).
  It takes about 40 seconds on a 10-core Apple M4 and needs 1.2 GB of disk.
- **29-file sample:** a subset of the same files, listed in
  [`manifest.txt`](manifest.txt), with at least one file from each category.
  It runs in about 2 seconds.
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

## How the last failures were fixed

Nothing fails now. When the full suite first ran, 458 records failed. The
fixes, by cause:

| Records | Cause | Fix |
| ---: | --- | --- |
| 335 | Deeply nested `WHERE` clauses hit `sqlparser`'s default recursion limit of 50 | The limit is now 100. The deepest record needs 65, and a test runs nesting at 100 on a 1 MiB stack, the WebAssembly default. |
| 72 | `x IN t1`, SQLite's shorthand for `x IN (SELECT * FROM t1)`, did not parse | Parsed in `dialect.rs` |
| 27 | `CREATE TRIGGER` and `DROP TRIGGER` were not supported | Real triggers: BEFORE, AFTER and INSTEAD OF, `WHEN`, `NEW` and `OLD`, `RAISE`, stored in the schema |
| 7 | `min(DISTINCT x)` or `max(DISTINCT x)` next to plain columns was rejected | Accepted, with SQLite's rule for which row the plain columns come from |
| 6 | `<<` did not parse, so two rows were missing and 4 later sums were wrong | Bit operators (M5), plus two harness fixes: reals print with 16 significant digits as SQLite's `printf` does, and a query that fails while running reads as no rows, as in SQLite's runner |
| 6 | `INDEXED BY` and `NOT INDEXED` did not parse | Parsed, and they limit the plans the planner may choose, since `NOT INDEXED` changes the row order |
| 4 | Blob literals such as `X'303132'` were not supported | A blob value type |
| 1 | `REINDEX` did not parse | Parsed; it checks the name and does nothing else, since index B-trees never go stale |

The suite never uses some SQL, so 100% does not mean all of SQL works. See
[the case study](../docs/CASE_STUDY.md#known-limits) for what is still
"not supported yet".
