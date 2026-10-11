# Fishbowl plan

Fishbowl is a SQL database written from scratch in Rust that you can watch
work. The engine compiles to WebAssembly and runs in a browser page. You type a
query and see the plan the planner chose, why it chose it, and how many rows
moved through each step. SQLite's own test suite measures correctness, and that
pass rate is the project's headline claim.

## Architecture

```text
             SQL text
                │
   ┌────────────▼────────────┐
   │ parse      (sqlparser)  │  text -> syntax tree
   ├─────────────────────────┤
   │ plan       planner.rs   │  resolve names against the catalog,
   │                         │  choose the steps (later: choose between plans)
   ├─────────────────────────┤
   │ execute    exec.rs      │  pull rows through the plan, one at a time,
   │                         │  counting rows per step
   ├─────────────────────────┤
   │ store      storage/     │  pager -> B+trees on 4 KiB pages
   │                         │  later: write-ahead log
   └────────────┬────────────┘
                │
     ┌──────────┼───────────────┬──────────────────┐
     ▼          ▼               ▼                  ▼
  fishbowl   fishbowl-wasm   fishbowl-slt       (tests)
  CLI shell  -> web/ (React) sqllogictest runner
```

### Crates

| Crate | Path | Role |
| --- | --- | --- |
| `fishbowl` | `crates/engine` | The engine. Everything that matters lives here. |
| `fishbowl-cli` | `crates/cli` | `fishbowl` shell: runs SQL, prints rows and the plan tree. |
| `fishbowl-slt` | `crates/slt` | Runs the pinned sqllogictest corpus and writes the report. |
| `fishbowl-wasm` | `crates/wasm` | Small WebAssembly API that returns JSON: run SQL, inspect pages. |

`web/` is a static React app that loads the WebAssembly build. It has no server.

### Engine modules today

| Module | What it does |
| --- | --- |
| `value.rs` | SQL values, SQLite type affinity, comparison and number formatting |
| `catalog.rs` | Table schemas and views |
| `planner.rs` | Syntax tree to plan, name resolution, loud rejection of unsupported SQL |
| `plan.rs` | Plan nodes and the profiled plan tree the UI draws |
| `expr.rs` | Bound expressions and their evaluation (three-valued logic, arithmetic, subqueries through an `Env`) |
| `planner/schema.rs` | `PRIMARY KEY` and `UNIQUE` in `CREATE TABLE`, `CREATE INDEX`, `DROP INDEX` |
| `access.rs` | Access paths: when to use an index, and the IndexScan that reads one |
| `exec.rs` | Iterator-model executor with per-step row counts, a row budget and an optional row-level trace |
| `trace.rs` | Trace events (passed on, dropped, subquery run) and the recorder that collects them |
| `write.rs` | Row writes: rowids, index maintenance, constraint checks, undo on failure |
| `storage/` | Page store (file or memory), pager and buffer pool, row encoding, B+trees for tables and indexes, schema table, read-only page inspection |
| `lib.rs` | `Database`: runs statements, `integrity_check` |

## Key design choices

| Choice | Reason |
| --- | --- |
| Rust for the engine | Memory safety without a garbage collector, and it compiles to WebAssembly. |
| `sqlparser` crate for parsing | Parsing is a solved problem; storage, planning and recovery are the hard parts worth writing. |
| SQLite semantics as the target | The sqllogictest expected results are SQLite's answers, so matching SQLite is what the pass rate measures. |
| Iterator ("Volcano") execution | One row at a time maps directly onto the plan tree the UI draws, and each step can count its rows. |
| Unsupported SQL is an error, never ignored | A database that silently skips `ORDER BY` returns wrong answers; an error only costs pass rate. |
| One file, fixed 4 KiB pages (M3) | Matches the OS page size and SQLite's layout, and pages are easy to draw. |
| B+tree tables keyed by rowid, separate index B-trees (M3, M4) | SQLite's proven layout, and it makes "scan the table or use the index" a real, visible choice. |
| One B-tree implementation, generic over its key (M4) | Tables use the rowid, indexes use (values, rowid). Splits, merges and their property tests serve both. |
| IndexScan keeps the Filter above it (M4) | The index only narrows rows down and the Filter decides them, so a loose bound costs speed, never correctness. |
| Constraints checked row by row with an undo log (M4) | Matches SQLite's order of checks exactly, and a failed statement still leaves no trace. |
| Write-ahead log for crash safety (M6) | Recovery is "replay committed frames", which is simple to reason about and to animate. |
| Storage behind a page-store trait (M3) | A real file on native builds, memory in the browser, a fault-injecting store in crash tests. |
| JSON across the WebAssembly boundary | A three-method API: run, inspect pages, reset. A Rust test pins the shape that `web/src/engine.ts` relies on. |
| Trace recorder is an `Option` on the executor context (M7) | An untraced query pays one check per row and copies nothing, so the harness keeps its speed. One run gives both the result and its trace, so they always agree. |
| Page viewer reads pages, never a copy (M7) | It decodes the same bytes the B-tree uses, so what it draws is what is stored. Splits are found by comparing two snapshots, so the B-tree code needs no hooks. |
| Own sqllogictest evaluator on the crate's parser | Formatting, sorting and hashing rules from SQLite's reference runner live in one tested file. |
| Corpus pinned by check-in and SHA-256, fetched not vendored | The repo stays small and every run is checked against the exact files behind the number. |
| Row budget instead of wall-clock timeouts | A runaway query fails the same way on every machine, so results stay deterministic. |
| Report without timings, committed to git | The same engine and corpus give the same bytes, so a diff always means a behavior change. |
| Static React, Vite and Tailwind site | Hosting costs nothing and there is no server to keep alive. |
| Separate `wasm` Cargo profile at `opt-level = "s"` | Cuts the gzipped browser download by about a quarter without slowing the native harness. |

## Milestones

Pass-rate numbers come from `sqllogictest/RESULTS.md`. Targets are guesses to
steer by, not promises.

### M0. Foundation (done)

Workspace, in-memory engine slice (CREATE TABLE, INSERT, SELECT with WHERE),
CLI, WebAssembly build, UI with plan tree and step-through, sqllogictest harness
with a pinned 29-file corpus. Baseline: **11.90%** of records.

After M1 and M3: **31.06%**. After M4: **84.31%**. After M2 on top of M4:
**99.89%**, with 4 wrong results left (the `1<<63` rows in
`slt_lang_aggfunc.test`: `<<` is not supported, so those rows are missing).

### M1. Query breadth

`ORDER BY`, `LIMIT`/`OFFSET`, `DISTINCT`, aggregates with `GROUP BY` and
`HAVING`, `CASE`, `BETWEEN`, `IN (list)`, `CAST`, `COALESCE`/`NULLIF`/`ABS` and
other scalar functions, comparison affinity. New plan nodes: Sort, Limit,
Aggregate, Distinct. These are the top failure reasons after "no such table".

Files: `planner.rs`, `expr.rs`, `exec.rs`, `plan.rs` (new nodes),
`docs/learn/`.

### M2. Joins, subqueries and compound queries (done)

Several tables in `FROM`, `JOIN ... ON`, nested-loop join, scalar, `IN` and
`EXISTS` subqueries, `UNION`/`INTERSECT`/`EXCEPT`. Depends on M1 (same files).

Done: inner, cross and LEFT joins with conditions pushed down to the table
they read; NestedLoopJoin and Materialize steps; scalar, `IN` and `EXISTS`
subqueries, correlated ones included, drawn under the step that runs them;
subqueries in FROM; compound SELECTs, left to right as in SQLite; `CREATE
VIEW` and `DROP VIEW`, saved in the schema table (TEMP views are not). Each
table in a join can use an index for its own conditions, and inner joins
follow a simple order rule (start from a filtered table, then follow the
conditions) until M5 brings costs.

### M3. Storage engine

A pager over one database file with fixed 4 KiB pages and a small buffer pool.
Row encoding. B+tree tables keyed by rowid, with insert, delete, split and
merge. `UPDATE`, `DELETE`, `DROP TABLE`. A page-store trait with a file backend
and a memory backend for the browser. Property tests that compare the B-tree
with a `BTreeMap`.

Files: new `crates/engine/src/storage/` directory replacing `storage.rs`, the
`Database` fields in `lib.rs`, and DML execution.

### M4. Indexes

`CREATE INDEX`, `PRIMARY KEY` and `UNIQUE` (unblocks most of the
"no such table" failures, which cascade from `PRIMARY KEY` tables), index
B-trees keyed by (value, rowid), index maintenance on every write, and an
IndexScan plan node. Depends on M3.

Done. Also shipped: `INTEGER PRIMARY KEY` as the rowid, `DROP INDEX`,
`INSERT ... SELECT`, `INSERT OR REPLACE/IGNORE/FAIL/ABORT`, UPDATE and DELETE
finding rows through an index, `Database::integrity_check`, and a differential
test against the system `sqlite3`.

### M5. Cost-based planner

Table statistics, a cost model, a choice between a full scan and each usable
index, join ordering (replacing M2's connect-the-conditions rule), and index
lookups inside a join. The planner keeps the rejected plans and their costs so
the UI can show the chosen plan beside the alternatives. Depends on M2 and M4.

### M6. Transactions and crash recovery

`BEGIN`, `COMMIT`, `ROLLBACK`, a write-ahead log with checksummed frames,
checkpointing, and recovery on open. A crash-test harness that cuts power at
every write in a workload (through a fault-injecting page store) and checks that
each committed transaction survives and no uncommitted one appears. Depends on
M3.

### M7. Visual depth

Row-level execution trace: which rows each step read, kept and dropped, played
back at the user's pace. Chosen plan versus rejected plans with costs. A B-tree
page viewer. A WAL viewer that replays recovery. Engine side: a trace hook in
`exec.rs`, behind a flag so the harness pays nothing. UI side: `web/`.

Done: the trace (`trace.rs`, `exec.rs`) and its playback, with rows rising
from their table through each step into the result, joins shown pair by pair
and correlated subqueries run by run; the reason for each choice and the rows
an index never touched next to what a full scan would read; a B-tree page
viewer (`storage/inspect.rs`) that shows splits and merges as rows are
inserted and deleted; and a first screen that is already playing a query.
Waiting on other milestones: rejected plans with their costs (M5) and the WAL
viewer (M6).

### M8. Proof and presentation

GitHub Actions running tests and the corpus on every push, a larger corpus, the
engineering case study, the published static site. Deploying and creating the
GitHub repo need Anestis's go-ahead.

## Which milestones can run in parallel

Threads can run at the same time when they do not edit the same files.

| Lane | Milestones | Owns |
| --- | --- | --- |
| A. SQL | M1, then M2, then M5 | `planner.rs`, `expr.rs`, `exec.rs`, `plan.rs` |
| B. Storage | M3, then M4, then M6 | `storage/`, transaction and WAL modules |
| C. UI | M7 (web side) | `web/` |
| D. Proof | M8 (CI and corpus) | `crates/slt`, `scripts/`, `.github/`, `sqllogictest/manifest.txt` |

Ready to start now in parallel threads: **M1, M3, the UI half of M7, and the CI
half of M8.**

Shared seams to coordinate:

- `crates/engine/src/lib.rs`: M3 changes the `Database` fields; M1 should not
  need to touch it. Merge M3 last if both are open.
- `crates/wasm` and `web/src/engine.ts`: any lane that adds fields to the JSON
  contract updates both, plus the contract test in `crates/wasm/src/lib.rs`.
- `sqllogictest/RESULTS.md` and `results.json` are generated. Regenerate them
  after a merge instead of resolving conflicts by hand.
- `docs/learn/`: every lane adds its own notes; new files rarely conflict.
- M4 needs M3's B-tree, and M5 needs M2's joins and M4's indexes. Those cannot
  start early.

## Known gaps

- An IndexScan uses only an index's first column, one index per query, and
  never for `OR` or `ORDER BY`. Choosing better is M5's cost model.
- Index entries are limited to 512 bytes, so a long text value in an indexed
  column is rejected with "not supported yet".
- `AUTOINCREMENT`, `WITHOUT ROWID`, partial and expression indexes, `UPDATE OR`
  and `ON CONFLICT` clauses are rejected.
- Changes are written to disk at the end of each statement, but a crash mid-write
  can leave a mix of old and new pages until the write-ahead log lands (M6).
- The browser download is about 970 KB gzipped (engine 888 KB, UI 83 KB),
  plus 141 KB of fonts. Most of the engine is `sqlparser`. `wasm-opt` and
  trimming dialects are possible later.
- A join reads its right side in full for every left row (from memory, after
  Materialize). It does not look matching rows up by index yet; that is M5.
- RIGHT and FULL joins, `USING`, `NATURAL`, subqueries in `UPDATE`/`DELETE`,
  and an aggregate of only an enclosing query's columns are rejected as "not
  supported yet".
