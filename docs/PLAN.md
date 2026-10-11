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
| `fishbowl-wasm` | `crates/wasm` | Two-method WebAssembly API that returns JSON. |

`web/` is a static React app that loads the WebAssembly build. It has no server.

### Engine modules today

| Module | What it does |
| --- | --- |
| `value.rs` | SQL values, SQLite type affinity, comparison and number formatting |
| `catalog.rs` | Table schemas and views |
| `planner.rs` | Syntax tree to plan, name resolution, loud rejection of unsupported SQL |
| `plan.rs` | Plan nodes and the profiled plan tree the UI draws |
| `expr.rs` | Bound expressions and their evaluation (three-valued logic, arithmetic, subqueries through an `Env`) |
| `exec.rs` | Iterator-model executor with per-step row counts and a row budget |
| `storage/` | Page store (file or memory), pager and buffer pool, row encoding, B+tree tables, schema table |
| `lib.rs` | `Database`: runs statements, makes INSERT all-or-nothing |

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
| Write-ahead log for crash safety (M6) | Recovery is "replay committed frames", which is simple to reason about and to animate. |
| Storage behind a page-store trait (M3) | A real file on native builds, memory in the browser, a fault-injecting store in crash tests. |
| JSON across the WebAssembly boundary | A two-method API. A Rust test pins the shape that `web/src/engine.ts` relies on. |
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

After M1 and M3: **31.06%**. After M2: **40.78%**.

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
VIEW` and `DROP VIEW`.

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

### M5. Cost-based planner

Table statistics, a cost model, a choice between a full scan and each usable
index, join ordering. The planner keeps the rejected plans and their costs so
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

- `PRIMARY KEY` and `UNIQUE` are rejected, not ignored, until M4 can enforce them.
- Changes are written to disk at the end of each statement, but a crash mid-write
  can leave a mix of old and new pages until the write-ahead log lands (M6).
- The browser build is about 700 KB gzipped, mostly `sqlparser`. `wasm-opt` and
  trimming dialects are possible later.
- Views live in the catalog only: they are not saved to the database file, so
  a reopened file has no views. Saving them needs a schema-table entry (M4's
  files).
- Tables join in FROM order. Two 8-table joins in `select4.test` exceed the row
  budget for that reason; join ordering is M5.
- `INSERT ... SELECT`, RIGHT and FULL joins, `USING`, `NATURAL`, subqueries in
  `UPDATE`/`DELETE`, and an aggregate of only an enclosing query's columns are
  rejected as "not supported yet".
