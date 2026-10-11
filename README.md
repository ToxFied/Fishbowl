<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="brand/lockup.svg">
    <img src="brand/lockup-light.svg" width="420" alt="Fishbowl">
  </picture>
</p>

<p align="center"><strong>A SQL database you can see inside.</strong></p>

<p align="center">
  <a href="docs/PLAN.md">Plan</a>
  · <a href="sqllogictest/RESULTS.md">Test results</a>
  · <a href="docs/sqllogictest.md">How it's measured</a>
  · <a href="docs/learn/README.md">Learn</a>
</p>

Fishbowl is a SQL database written from scratch in Rust. It compiles to
WebAssembly and runs in your browser, where you type a query and watch it run:
the plan tree, why each step was chosen, and how many rows moved through each
step.

Correctness is measured, not claimed. A harness runs SQLite's own
[sqllogictest](https://www.sqlite.org/sqllogictest) suite against Fishbowl.

| Corpus | Records run | Pass rate | Queries only |
| --- | ---: | ---: | ---: |
| 29 pinned files, sqllogictest `ab599cbdd0` | 129,348 | **84.31%** | 85.48% |

Single-table queries work, including sorting, grouping and aggregates, and
`INSERT`, `UPDATE` and `DELETE` write to B-trees on 4 KiB pages. `PRIMARY KEY`,
`UNIQUE` and `CREATE INDEX` are enforced on every write, and a `WHERE` on an
indexed column reads the index instead of the whole table. Everything else is
rejected with "not supported yet" rather than answered wrongly. The biggest
remaining gaps are joins, subqueries and views (M2).
[`RESULTS.md`](sqllogictest/RESULTS.md) breaks the failures down by reason.

## Status

| Part | State |
| --- | --- |
| Parse, plan, execute pipeline | Working: one table, filters, sorting, grouping, aggregates |
| Storage | Pager, buffer pool and B+tree tables on 4 KiB pages (M3). Not crash-safe until M6 |
| Indexes and constraints | `PRIMARY KEY`, `UNIQUE`, `CREATE INDEX`, IndexScan chosen by a simple rule (M4) |
| Cost-based planner | Planned (M5) |
| Transactions and crash recovery | Planned, write-ahead log (M6) |
| Browser UI | Plan tree, step-through, results, schema |
| sqllogictest harness | Pinned corpus, self-tested, deterministic report |

## Try it

You need Rust (via [rustup](https://rustup.rs)) with the `wasm32-unknown-unknown`
target, `wasm-bindgen-cli` 0.2.129, Node.js and pnpm.

```bash
# Engine tests
cargo test --workspace

# SQL shell
cargo run -p fishbowl-cli -- -c "
  CREATE TABLE fish (id INTEGER, name TEXT);
  INSERT INTO fish VALUES (1, 'goldie'), (2, 'nemo');
  SELECT name FROM fish WHERE id > 1;"
```

```text
name
----
nemo
(1 row)

plan:
Project name  [rows=1]
  Filter (id > 1)  [rows=1]
    SeqScan fish  [rows=2]
      why: fish has no index, so every row is read
```

```bash
# Browser UI at http://localhost:5173
cargo install wasm-bindgen-cli --version 0.2.129
cd web && pnpm install && pnpm dev

# sqllogictest pass rate
scripts/fetch-sqllogictest.sh
cargo run --release -p fishbowl-slt
```

## Layout

| Path | What |
| --- | --- |
| `crates/engine` | The engine: values, catalog, planner, executor, storage |
| `crates/cli` | `fishbowl` SQL shell |
| `crates/slt` | sqllogictest runner and report |
| `crates/wasm` | WebAssembly bindings |
| `web/` | React, Vite and Tailwind UI. A static site with no server |
| `sqllogictest/` | Corpus manifest and generated results |
| `docs/` | [Plan and roadmap](docs/PLAN.md), [harness notes](docs/sqllogictest.md), [learning notes](docs/learn/README.md) |
| `brand/` | Logo, colors and type. See [BRAND.md](brand/BRAND.md) |

## License

MIT. The sqllogictest files are downloaded from sqlite.org and are not part of
this repository; see [their terms](docs/sqllogictest.md#license).
