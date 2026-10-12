---
title: "Getting started"
description: "Build Fishbowl, run SQL in the shell, start the browser UI, and run the test suites."
---

You need Rust (via [rustup](https://rustup.rs)) with the
`wasm32-unknown-unknown` target, `wasm-bindgen-cli` 0.2.129, Node.js, pnpm, and
the `sqlite3` CLI, which a differential test compares answers with. CI pins
sqlite3 3.54.0. The repository's `rust-toolchain.toml` picks the Rust version.

```bash
git clone https://github.com/ToxFied/Fishbowl.git
cd Fishbowl
cargo test --workspace
```

## The SQL shell

```bash
cargo run -p fishbowl-cli -- -c "
  CREATE TABLE fish (id INTEGER PRIMARY KEY, name TEXT, species TEXT);
  INSERT INTO fish VALUES (1, 'goldie', 'goldfish'), (2, 'nemo', 'clownfish'),
                          (3, 'koko', 'koi'), (4, 'pearl', 'koi');
  CREATE INDEX fish_species ON fish (species);
  ANALYZE;
  SELECT name FROM fish WHERE species = 'koi';"
```

```text
name
-----
koko
pearl
(2 rows)

plan:
Project name  [rows=2]
  Filter (species = 'koi')  [rows=2]
    SeqScan fish  [rows=4]
      why: reading all 4 rows of fish is the cheapest way here
      estimate: rows=4 cost=12
      rejected: IndexScan fish USING INDEX fish_species (species = 'koi')  [rows=2 cost=13]
```

On four rows, reading them all is cheaper than finding two through the index.
On a big table the index wins. See [cost models](learn/cost-models.md).

## The browser UI

```bash
cargo install wasm-bindgen-cli --version 0.2.129
pnpm --dir web install
pnpm --dir web dev
```

It opens at <http://localhost:5173>. `pnpm --dir web build` writes a static
site to `web/dist` with relative paths, so it works from any URL prefix.

## The tests

```bash
# Crash test: prints how many crash points it tried
cargo test -p fishbowl --test crash -- --nocapture

# sqllogictest, the 29-file sample: 26 MB, about 2 seconds
scripts/fetch-sqllogictest.sh
cargo run --release -p fishbowl-slt -- --write

# sqllogictest, the full suite: 1.2 GB, about 40 seconds on 10 cores
scripts/fetch-sqllogictest.sh --full
cargo run --release -p fishbowl-slt -- --full --write
```

`--write` updates the committed reports in `sqllogictest/`.

## This documentation

The site is a [Fumadocs](https://fumadocs.dev) app in `website/` that reads
the Markdown in `docs/`.

```bash
pnpm --dir website install
pnpm --dir website dev
```
