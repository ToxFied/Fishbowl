---
title: "Architecture"
description: "The crates, the engine modules, and the design choices behind them."
---

Fishbowl is one Rust engine with three front ends: a SQL shell, a WebAssembly
build for the browser UI, and the sqllogictest runner.

```text
             SQL text
                │
   ┌────────────▼────────────┐
   │ parse      (sqlparser)  │  text -> syntax tree
   ├─────────────────────────┤
   │ plan       planner.rs   │  resolve names against the catalog,
   │                         │  choose the steps and compare plans by cost
   ├─────────────────────────┤
   │ execute    exec.rs      │  pull rows through the plan, one at a time,
   │                         │  counting rows per step
   ├─────────────────────────┤
   │ store      storage/     │  pager -> B+trees on 4 KiB pages,
   │                         │  write-ahead log, recovery
   └────────────┬────────────┘
                │
     ┌──────────┼───────────────┬──────────────────┐
     ▼          ▼               ▼                  ▼
  fishbowl   fishbowl-wasm   fishbowl-slt       (tests)
  CLI shell  -> web/ (React) sqllogictest runner
```

## Crates

| Crate | Path | Role |
| --- | --- | --- |
| `fishbowl` | `crates/engine` | The engine. Everything that matters lives here. |
| `fishbowl-cli` | `crates/cli` | `fishbowl` shell: runs SQL, prints rows and the plan tree. |
| `fishbowl-slt` | `crates/slt` | Runs the pinned sqllogictest corpus and writes the report. |
| `fishbowl-wasm` | `crates/wasm` | Small WebAssembly API that returns JSON: run SQL, inspect pages. |

`web/` is a static React app that loads the WebAssembly build. It has no server.

## Engine modules

| Module | What it does |
| --- | --- |
| `value.rs` | SQL values, SQLite type affinity, comparison and number formatting |
| `catalog.rs` | Table schemas, views, and the statistics `ANALYZE` gathers |
| `dialect.rs` | The SQL dialect: `sqlparser`'s SQLite dialect plus `<<`, `>>` and SQLite's operator precedence |
| `planner.rs` | Syntax tree to plan, name resolution, loud rejection of unsupported SQL |
| `planner/cost.rs` | The cost model: SQLite's weights and its size guesses before `ANALYZE` |
| `planner/paths.rs` | Every way to read one table (scan, rowid, each index prefix, covering index), costed |
| `planner/join.rs` | Join order search, where each condition goes, and the plans turned down |
| `plan.rs` | Plan nodes and the profiled plan tree the UI draws |
| `expr.rs` | Bound expressions and their evaluation (three-valued logic, arithmetic, subqueries through an `Env`) |
| `planner/schema.rs` | `PRIMARY KEY` and `UNIQUE` in `CREATE TABLE`, `CREATE INDEX`, `DROP INDEX` |
| `access.rs` | Running an IndexScan: multi-column lookups, values from a join's left row, covering indexes |
| `exec.rs` | Iterator-model executor with per-step row counts, a row budget and an optional row-level trace |
| `trace.rs` | Trace events (passed on, dropped, subquery run) and the recorder that collects them |
| `write.rs` | Row writes: rowids, index maintenance, constraint checks, undo on failure |
| `storage/` | Page store and log store (file or memory), pager and buffer pool, row encoding, B+trees for tables and indexes, schema table, read-only page inspection |
| `storage/wal.rs` | Write-ahead log: checksummed frames, commit, checkpoint, recovery |
| `storage/fault.rs` | Simulated disk that loses power on purpose, for the crash test |
| `lib.rs` | `Database`: runs statements in transactions, autocommit, `integrity_check` |

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
| Statistics only from `ANALYZE`, SQLite's guesses before (M5) | A plan depends only on the SQL and the schema until `ANALYZE` runs, as in SQLite. Stable plans mean stable answers, since read order decides which of `2` and `2.0` a group shows. Writes pay nothing. |
| Cost model copied from SQLite (M5) | Same weights, same guesses, same order of trying plans, so Fishbowl reads rows in SQLite's order, which decides answers where equal values differ in how they print. |
| GROUP BY, DISTINCT and UNION ask for an ordered read (M5) | Fishbowl gains no speed from it, but SQLite reads that way, and the first row read is the one a group shows. |
| Join order by dynamic programming, then a beam of 16 (M5) | Exact up to 8 tables; past that, SQLite-style "N best paths" keeps 64-table joins fast to plan. |
| Index lookups in joins replace Materialize (M5) | The right side restarts for each left row with that row's values, so a correlated subquery gets index lookups the same way. |
| Write-ahead log for crash safety (M6) | Recovery is "replay committed frames", which is simple to reason about and to animate. SQLite's WAL mode, minus its multi-process index. |
| No steal: uncommitted pages stay in memory (M6) | Nothing uncommitted ever reaches a file, so recovery only redoes. The cost: one transaction's changes must fit in RAM. |
| Truncate the log after each checkpoint (M6) | Old frames can never be mistaken for new ones, with no extra header sync. |
| After a write-path I/O error, the handle stops until reopened (M6) | What reached the disk is unknown, and retrying a failed `fsync` can lie. Recovery reads the truth. |
| Crash test on a simulated disk, checked by planted bugs (M6) | Every write point, torn writes and lost unsynced writes, in seconds. Planting bugs shows what the test can and cannot see. |
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

