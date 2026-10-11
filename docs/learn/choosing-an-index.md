# Choosing an index

For each table a query reads, the planner lists every way to read it,
costs each one with the [cost model](cost-models.md), and keeps the
cheapest. The plan shows the ones it turned down.

## The ways to read a table

1. **A full scan**: every row, in rowid order.
2. **A rowid lookup**, when the table has an `INTEGER PRIMARY KEY` and a
   condition on it: `id = 7`, `id IN (1, 2)`, `id BETWEEN 3 AND 9`.
3. **An index lookup**, for each index. The lookup uses the longest
   prefix of the index's columns that the conditions pin down: an
   equality or `IN` list on each leading column, then a range on the next.
   With an index on `(species, weight)`:

   | WHERE | Lookup |
   | --- | --- |
   | `species = 'koi' AND weight > 1` | both columns: `species = 'koi' AND weight > 1` |
   | `species IN ('koi', 'tang')` | the first column, two keys |
   | `weight > 1` | none: the index is sorted by species first |

4. **A whole index**, read from end to end. That only pays off in two cases.
   If the index holds every column the query reads, the table is never
   touched: a **covering index**. Or if the index's order saves a sort.

## Covering indexes

An index entry holds its columns and the rowid. If the query reads nothing
else, Fishbowl rebuilds the row from the entry and skips the table:

```text
SELECT species FROM fish WHERE species > 'h'
IndexScan fish USING COVERING INDEX fish_species (species > 'h')
```

A covering index is cheaper even with no condition at all, when it is
narrower than the table. `SELECT count(*) FROM fish` reads the narrowest
index whole instead of the table.

## Indexes and ORDER BY

An index returns rows sorted by its columns. If ORDER BY asks for that
order, the Sort step can go:

```text
SELECT id, grams FROM feeding ORDER BY id LIMIT 5
Limit 5
  Project id, grams
    SeqScan feeding      rows already in rowid order: no Sort step
```

Without a Sort, `LIMIT` stops the scan after 5 rows instead of reading and
sorting all of them. The planner weighs it: reading a whole index and
fetching every row can cost more than a quick range and a small sort. A
column fixed by an equality can be skipped, so an index on
`(species, weight)` serves `WHERE species = 'koi' ORDER BY weight`.

Index entries are stored ascending, and Fishbowl does not read them
backwards yet, so `ORDER BY ... DESC` still sorts.

## Conversions

A lookup must find exactly what the comparison would match. On an INTEGER
column, `a = '20'` means the number 20, so the lookup looks for 20. On a
TEXT column, `name = 5` looks for the text `'5'`. One case cannot use an
index at all: a TEXT column compared with a numeric column, as in
`t.name = u.number`. The comparison turns `'5'` in the TEXT column into the
number 5, but the index keeps `'5'` among the text values, so a lookup for
5 would miss it.

## Ties

When two ways cost exactly the same, the one SQLite tries first wins: the
full scan, then the rowid, then the indexes from the newest. That is not
arbitrary. It keeps Fishbowl reading rows in the same order as SQLite, which
[can change an answer](equal-values.md).

Code: `crates/engine/src/planner/paths.rs` (listing and costing paths),
`crates/engine/src/access.rs` (running a lookup).
