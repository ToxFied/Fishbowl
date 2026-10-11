# The sqllogictest harness

Fishbowl's correctness claim is a pass rate on SQLite's
[sqllogictest](https://www.sqlite.org/sqllogictest) suite. This page explains
what the number counts, how the harness avoids inflating it, and where the
test files come from. The latest numbers are in
[`sqllogictest/RESULTS.md`](../sqllogictest/RESULTS.md).

## Run it

```bash
scripts/fetch-sqllogictest.sh          # download the pinned files, check SHA-256
cargo run --release -p fishbowl-slt -- \
  --markdown sqllogictest/RESULTS.md --json sqllogictest/results.json
```

Add `--verbose` to print every failure with its file and line. Pass file paths
to run just those files; the report is then marked "unpinned" and should not be
quoted.

## What counts

A test file is a list of records. A `statement` record runs SQL and expects
success or an error. A `query` record runs SQL and lists the expected values.

- **Pass rate** is passed records divided by records run. The report also gives
  the query-only rate, since `CREATE TABLE` and `INSERT` statements are easier
  and would otherwise flatter the total.
- **Skipped** records are ones the file itself marks as not applying to SQLite,
  with `skipif sqlite` or `onlyif <other database>`. They are reported, never
  counted as passes.
- Each file runs against a fresh, empty database.

## Rules that keep the number honest

1. **The corpus is pinned.** `sqllogictest/manifest.txt` lists every file with
   its SHA-256 and the upstream check-in. The runner refuses to report if any
   file is missing or changed.
2. **"Not supported yet" is a failure,** even where the test expects an error.
   A `statement error` record passes only when Fishbowl rejects the SQL for a
   real reason: a parse error, an unknown name, or a broken constraint.
3. **A panic fails one record** and the run continues. A crash cannot hide the
   records after it.
4. **A runaway query fails cleanly.** Each statement may process 20 million
   rows. Over that, it fails with "row budget exceeded". Unlike a timeout, this
   fails the same way on every machine.
5. **Results follow SQLite's reference runner.** Values are formatted by the
   column type the test declares (`I` as an integer, `R` with three decimals,
   `T` as text, `NULL`, `(empty)` for an empty string). Then they are sorted by
   the sort mode, and above the `hash-threshold` they are compared by MD5 hash.
   An expected result written as "N values hashing to ..." is compared by hash
   whatever the threshold, as SQLite's runner does; `select1.test` and
   `select4.test` rely on that. This logic is in `crates/slt/src/lib.rs`.
6. **The harness is tested against itself.** `crates/slt/tests/harness.rs`
   feeds it scripts with known outcomes, including wrong answers, wrong hashes,
   wrong column counts and unsorted results, and checks that each one fails.
7. **The report is deterministic.** It holds no timings or machine details, so
   a change in the committed file always means a change in behavior.

## Two fixes on top of the parser

The `sqllogictest` crate parses the files. Fishbowl evaluates them itself and
corrects two parsing differences from SQLite's reference runner:

- SQLite's files put notes after conditions, as in `skipif mysql # not
  compatible`. The crate rejects that line. The harness drops the note before
  parsing, keeping line numbers intact.
- The crate attaches `onlyif`/`skipif` to statements and queries but not to
  `halt`. Left alone, `onlyif mssql` followed by `halt` would stop the file for
  every engine and then skip the next statement. The harness tracks conditions
  per record block instead.

Both fixes have tests in `crates/slt/tests/harness.rs`.

## The corpus

29 files from the SQLite sqllogictest repository, check-in
[`ab599cbdd0976a63`](https://www.sqlite.org/sqllogictest/info/ab599cbdd0976a63c5cfc7755588f716a7621f27)
(2026-10-07). About 143,000 records in total.

| Files | Why |
| --- | --- |
| `select1.test` to `select5.test` | The classic core: expressions, subqueries, joins, `ORDER BY` |
| `evidence/*.test` (12 files) | Short tests tied to specific statements in SQLite's documentation |
| `index/<kind>/10/slt_good_0.test` (7 files) | Generated queries over indexed tables: `BETWEEN`, commuted comparisons, `DELETE`, `IN`, ordering |
| `index/view/10/slt_good_1.test` | Views over indexed tables. Upstream `slt_good_0.test` at this path is an empty file, so the next one is used |
| `random/<kind>/slt_good_0.test` (4 files) | Randomly generated aggregates, expressions, `GROUP BY` and `SELECT`s |

The full upstream suite has millions of records. This subset covers every
category while keeping a full run under a few seconds. A later milestone can
widen it; widening it changes the denominator, so the report always states the
corpus it measured.

### License

The files are not stored in this repository. `scripts/fetch-sqllogictest.sh`
downloads them from sqlite.org. Upstream's `COPYRIGHT.md` reads, in part:

> This program is free software. As far as the author is concerned, you can
> redistribute it and/or modify the code as you see fit. No attribution is
> required. Use whichever of the following license terms best applies to your
> situation: GNU General Public License, BSD License, MIT License, CC0 License.

Copyright 2008 D. Richard Hipp.
