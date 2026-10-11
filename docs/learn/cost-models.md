# Cost models

A planner has to choose between plans that all give the right answer. A
**cost model** puts a number on each plan, a guess at how much work it
takes, so the choice is "take the smallest number".

Fishbowl's unit of work is "rows read". It copies the main terms of
SQLite's cost model, so it chooses what SQLite would.

## The terms

| Work | Cost |
| --- | --- |
| Read one row of a table in a full scan | 3 |
| Fetch one row by its rowid | 3 |
| Read one index entry | about 1 to 3: less for a narrow index |
| Find a key in a B-tree of N entries | log2(N) |
| Sort n rows | 2 · n · log2(n), then a quarter more |
| Hand out a row a Materialize step holds in memory | 1 |

A full scan of a table costs 3 per row. SQLite chose 3 on purpose: a scan
has to be clearly cheaper than an index before it wins, because an index's
worst case is better when the size guesses are wrong.

An index entry holds a few columns and the rowid, so more entries fit on a
page than whole rows. Its cost depends on how wide it is compared with a
row. Widths are guessed from the declared types: an INTEGER counts 1, a
TEXT 5, a `VARCHAR(30)` 8. An index on one integer column of a wide table
costs about 1.1 per entry. One with most of the table's columns costs
nearly 3.

The extra quarter on a sort is SQLite's too: it tips close calls toward a
plan whose rows already come out in order.

## A worked example

`SELECT name FROM fish WHERE species = 'koi'`, with an index on `species`
and no statistics yet:

```text
IndexScan fish USING INDEX fish_species (species = 'koi')
  estimate: rows=10 cost=75
  rejected: SeqScan fish  [rows=1048576 cost=3145728]
```

- The scan reads 1,048,576 guessed rows at 3 each: 3,145,728.
- The index lookup finds the key in about 20 steps (log2 of a million),
  reads 10 entries at 2.46 each (25), and fetches each row (10 × 3 = 30):
  75 in all.

After `ANALYZE` the table has 8 rows and the index about 2 per species. The
scan costs 24 and the lookup 14, so the index still wins, by less.

## What the numbers are not

They are not milliseconds. They are only good for comparing plans of the
same query. And they are guesses: the trace shows what really happened,
next to what the planner expected, so you can see where it was wrong.

Code: `crates/engine/src/planner/cost.rs`, `crates/engine/src/planner/paths.rs`
(costs of each way to read a table).
