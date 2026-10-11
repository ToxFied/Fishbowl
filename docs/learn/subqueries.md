# Subqueries and correlation

A subquery is a SELECT inside another statement. Fishbowl runs three kinds
inside expressions:

- **Scalar**: `(SELECT avg(weight) FROM fish)` is the first column of the
  first row, or NULL if there is none. It must return one column.
- **IN**: `species IN (SELECT species FROM koi_ponds)` is true if any row
  equals the value. NULLs follow [three-valued logic](null-logic.md): no match
  plus a NULL in the list gives NULL, and an empty list gives false, even for
  `NULL IN (...)`.
- **EXISTS**: `EXISTS (SELECT ...)` is true if there is at least one row. The
  columns do not matter.

## Correlated subqueries

A subquery can read a column of the query around it:

```sql
SELECT name, weight
FROM fish AS f
WHERE weight = (SELECT max(weight) FROM fish WHERE species = f.species);
```

`f.species` changes from row to row, so the subquery has a different answer
for each fish. It is *correlated* with the outer query, and it has to run
again for every outer row. A subquery that reads nothing from outside gives
the same answer every time, so it runs once and the answer is reused.

The plan shows the difference. A subquery hangs under the step whose
expression uses it, and its count is how many times it ran:

```text
Filter (weight = (SELECT max(weight) ...))   [rows=4]
  SeqScan fish                               [rows=8]
  Subquery scalar, correlated                [rows=8]   ran once per fish
    ...
      SeqScan fish                           [rows=64]  8 runs of 8 rows
```

## How the outer row gets in

The planner resolves names from the inside out: the subquery's own tables
first, then its own column aliases, then each enclosing query's tables. A name
found outside becomes an *outer column*: "column 2 of the row one query out".

When the executor runs a correlated subquery, it pushes the current outer row
onto a stack, runs the subquery, and pops it. An outer column reads from that
stack. Subqueries can nest, so the stack can hold several rows, and "two
queries out" reads the second from the top.

## Subqueries in FROM, and views

`SELECT * FROM (SELECT ...) AS s` treats a subquery's result as a table. A
**view** is the same thing with a name: `CREATE VIEW v AS SELECT ...` stores
the SELECT, and every query that uses `v` plans that SELECT again in its
place. The plan shows it as a SubqueryScan step with the view's plan beneath.
Like SQLite, Fishbowl checks a view's SELECT when a query uses it, not when it
is created. Views are not saved to the database file yet.

Code: `crates/engine/src/planner.rs` (`Binder::identifier`,
`Binder::subquery`, `plan_view`), `crates/engine/src/exec.rs` (`SubqueryOp`).
