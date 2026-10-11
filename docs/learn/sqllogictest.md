# Measuring correctness with sqllogictest

How do you know a database gives right answers? You compare it with one that
already does, on a very large number of queries.

SQLite's team wrote **sqllogictest** for this. It is millions of SQL statements
and queries, each stored with the answer SQLite (and other databases) gave.
The test files look like this:

```text
statement ok
CREATE TABLE t1(a INTEGER, b INTEGER)

query I rowsort
SELECT a FROM t1 WHERE b > 5
----
1
3
```

`query I rowsort` says the result has one integer column and that rows should be
sorted before comparing, because SQL does not promise any order without
`ORDER BY`. Large results are stored as an MD5 hash, written as "30 values
hashing to 3c1d...", to keep the files small.

## Why the harness is strict

A pass rate is only worth quoting if nobody can argue with it. Fishbowl's
harness pins the exact test files by checksum and treats "not supported yet" as
a failure. A crash costs one test, not the rest of the file. The harness also
has its own tests that feed it wrong answers and check they fail. Details are
in [the harness notes](../sqllogictest.md).

The first run passed 11.90%. Most failures are missing features (ORDER BY,
DISTINCT, GROUP BY, joins) and tables declared with PRIMARY KEY, which Fishbowl
cannot create until it has indexes. The report lists failures by reason, which
makes it a to-do list sorted by impact.

Code: `crates/slt/`.
