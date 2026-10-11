# The sqllogictest harness

Fishbowl's correctness claim is a pass rate on SQLite's
[sqllogictest](https://www.sqlite.org/sqllogictest) suite. This page explains
what the number counts, how the harness avoids inflating it, and where the
test files come from. The latest numbers, and what fails and why, are in
[`sqllogictest/RESULTS.md`](../sqllogictest/RESULTS.md).

## Run it

```bash
# The 29-file sample: the fast check
scripts/fetch-sqllogictest.sh                    # download, check SHA-256
cargo run --release -p fishbowl-slt -- --write   # writes sqllogictest/sample.{md,json}

# The full suite: the headline number
scripts/fetch-sqllogictest.sh --full
cargo run --release -p fishbowl-slt -- --full --write   # writes sqllogictest/full.{md,json}
```

Add `--verbose` to print every failure with its file and line. Pass file paths
to run just those files; the report is then marked "unpinned" and should not be
quoted. `--jobs N` sets the number of threads; the default is one per core.

After regenerating, update the quoted rows in `README.md` and
`sqllogictest/RESULTS.md`. `cargo test` fails until they match the reports.

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
   a change in the committed file always means a change in behavior. Files run
   in parallel, largest first, each against its own database, and the report
   is sorted by path, so thread timing cannot change it. Two full runs give
   identical bytes.
8. **Failures are grouped by cause.** A parse error is bucketed by the
   parser's message without its position, so `x IN t1` and `x IN t2` count as
   one cause and the report reads as a to-do list.

## Three fixes on top of the parser

The `sqllogictest` crate parses the files. Fishbowl evaluates them itself and
corrects three differences from SQLite's reference runner:

- SQLite's files put notes after conditions, as in `skipif mysql # not
  compatible`. The crate rejects that line. The harness drops the note before
  parsing, keeping line numbers intact.
- The crate attaches `onlyif`/`skipif` to statements and queries but not to
  `halt`. Left alone, `onlyif mssql` followed by `halt` would stop the file for
  every engine and then skip the next statement. The harness tracks conditions
  per record block instead.
- The reference runner passes a `statement` record to `sqlite3_exec`, which
  runs every statement in it. One record in `slt_lang_aggfunc.test` holds two.
  The harness runs them all and reports the first error, if any.

All three fixes have tests in `crates/slt/tests/harness.rs`.

## The corpus

Both corpora come from the SQLite sqllogictest repository at check-in
[`ab599cbdd0976a63`](https://www.sqlite.org/sqllogictest/info/ab599cbdd0976a63c5cfc7755588f716a7621f27)
(2026-10-07).

### The full suite

All 622 test files at that check-in: 1.2 GB and 7.4 million records, of which
5.9 million apply to SQLite. `sqllogictest/manifest-full.txt` lists them with
their SHA-256. A run takes about 35 seconds on a 10-core Apple M4 and peaks at
about 570 MB of memory. The first download takes about 8 minutes.

`scripts/pin-sqllogictest.sh <checkin>` writes that manifest. Fossil, the
version control system SQLite uses, names a check-in by the SHA-1 of its
manifest, and the manifest lists every file's SHA-1. The script downloads the
manifest from `/raw/<checkin>`, checks its SHA-1 against the check-in name,
downloads each file and checks its SHA-1, and only then writes the SHA-256
list. So the pinned list matches upstream exactly, with no file guessed or
missed. Run it only to move to a new check-in.

### The sample

29 of those files, about 143,000 records, listed in
`sqllogictest/manifest.txt`. It runs in about 8 seconds, so CI runs it on
every push and checks that the committed report still matches.

| Files | Why |
| --- | --- |
| `select1.test` to `select5.test` | The classic core: expressions, subqueries, joins, `ORDER BY` |
| `evidence/*.test` (12 files) | Short tests tied to specific statements in SQLite's documentation |
| `index/<kind>/10/slt_good_0.test` (7 files) | Generated queries over indexed tables: `BETWEEN`, commuted comparisons, `DELETE`, `IN`, ordering |
| `index/view/10/slt_good_1.test` | Views over indexed tables. Upstream `slt_good_0.test` at this path is an empty file, so the next one is used |
| `random/<kind>/slt_good_0.test` (4 files) | Randomly generated aggregates, expressions, `GROUP BY` and `SELECT`s |

The sample covers every category of the full suite. Its pass rate is lower
than the full suite's because the `evidence/` files, where most failures are,
weigh far more in 129,000 records than in 5.9 million. Every report states
which corpus it measured.

### License

The files are not stored in this repository. `scripts/fetch-sqllogictest.sh`
downloads them from sqlite.org. Upstream's `COPYRIGHT.md` reads, in part:

> This program is free software. As far as the author is concerned, you can
> redistribute it and/or modify the code as you see fit. No attribution is
> required. Use whichever of the following license terms best applies to your
> situation: GNU General Public License, BSD License, MIT License, CC0 License.

Copyright 2008 D. Richard Hipp.
