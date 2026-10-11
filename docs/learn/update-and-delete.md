# How UPDATE and DELETE run

`UPDATE` and `DELETE` both start like a query: scan the table and keep the rows
where the `WHERE` condition is true. Then they change those rows. Fishbowl
splits that into two separate phases.

1. **Find.** Scan the whole table and collect every matching row with its
   rowid. For `UPDATE`, compute each new row from the old one and check its
   constraints, such as `NOT NULL`.
2. **Write.** Only then change the B-tree: replace each row under its rowid,
   or delete it.

## Why not change rows during the scan

**The Halloween problem.** Imagine `UPDATE emp SET salary = salary * 1.1
WHERE salary < 50000` running while it scans, if the scan were in salary
order. A raised salary can land later in the scan, be seen again, and be raised
again, until it passes 50,000. IBM engineers found this on Halloween 1976,
hence the name. Collecting first means every row is judged once, by its old
value.

**All or nothing.** If the fifth row breaks a `NOT NULL` constraint, the
statement fails before anything is written, so rows one to four are unchanged
too. Without transactions (milestone 6), this is how a single statement stays
atomic.

**The tree stays still.** Deleting rows can merge leaves and free pages. A scan
in the middle of those leaves could find its next page freed. Changing nothing
until the scan ends avoids the problem.

The cost is memory: the matching rows are held in a list. Real databases avoid
that with more careful cursors, and milestone 6's transactions make it
unnecessary for atomicity.

## Every value comes from the old row

`UPDATE t SET a = b, b = a` swaps two columns. Each expression on the right is
evaluated against the row as it was before the statement, not after the
earlier assignments. SQLite and the SQL standard both work this way.

## DROP TABLE

`DROP TABLE` frees every page of the table's B-tree, including overflow pages,
onto the free list, and deletes its row from the schema table. The next table
created reuses those pages.

Code: `crates/engine/src/lib.rs` (`Database::update`, `Database::delete`,
`matching_rows`).
