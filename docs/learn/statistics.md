# Statistics and ANALYZE

To choose a plan, the planner needs to know how big things are: how many
rows a table has, and how many of them a condition like `species = 'koi'`
will match. Those numbers are **statistics**.

## Two ways to keep them

A database can keep statistics up to date on every write, or gather them
when asked. Fishbowl, like SQLite, gathers them when asked, with `ANALYZE`:

```sql
ANALYZE;           -- every table
ANALYZE fish;      -- one table
ANALYZE fish_species;  -- the table this index belongs to
```

`ANALYZE` reads each table once and records, as SQLite's `sqlite_stat1` does:

- how many rows the table has,
- how many different values each column holds,
- for each index, how many rows on average share the same values in its
  first column, its first two columns, and so on.

For an 8-row `fish` table with 5 species, the index on `species` gets "2":
about 2 rows per species (8 / 5, rounded up).

## Why not keep them up to date on every write?

Three reasons, the first one the most important.

1. **Plans stay put.** Without `ANALYZE`, the planner uses fixed guesses
   (below) that do not depend on the data. The same query on the same
   schema always gets the same plan, however the data changes. The plan
   changes only when you run `ANALYZE`. SQLite promises the same thing.
   That matters more than it sounds: the plan decides the order rows are
   read in, and the order can change an answer (see
   [why 2 and 2.0 matter](equal-values.md)). Stable plans mean stable
   answers.
2. **Writes stay cheap.** A row count is easy to keep, but counting distinct
   values is not. After `DELETE`, you cannot tell whether the deleted value
   was the last of its kind without another index.
3. **It matches what the tests compare against.** sqllogictest and the
   differential test never run `ANALYZE`, so SQLite plans them with its
   guesses. Fishbowl uses the same guesses, so it makes the same choices.

The cost is that statistics go stale. After a big load, run `ANALYZE` again,
as you would in SQLite or PostgreSQL.

## The guesses without statistics

These are SQLite's numbers:

| What | Guess |
| --- | --- |
| Rows in a table | 1,048,576 |
| Rows matching `x = value` on an index's first column | 10 (then 9, 8, 7, 6 and 5 for longer prefixes) |
| Rows matching a unique index's whole key | 1 |
| A range (`x > 5`) | a quarter of the rows; both ends (`BETWEEN`) a 64th, never below 10 |
| Any other condition | keeps half the rows |

They look odd on a table of 8 rows, and the plan says so: "fish has no
statistics yet (ANALYZE counts them), so sizes are SQLite's guesses". The
browser UI runs `ANALYZE` after creating its tables, so its estimates are
real.

Statistics live in the catalog, in memory. A transaction that rolls back
undoes its `ANALYZE` too. Reopening a database file forgets them, so run
`ANALYZE` again after opening one.

Code: `crates/engine/src/catalog.rs` (`TableStats`), `crates/engine/src/lib.rs`
(`Database::count`), `crates/engine/src/planner/cost.rs` (the guesses).
