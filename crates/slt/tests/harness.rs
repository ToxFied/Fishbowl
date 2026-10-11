//! The harness must fail wrong answers as reliably as it passes right ones.
//! These scripts have known outcomes, written by hand.

use fishbowl_slt::{Counts, FileResult, run_script};

const SETUP: &str = "\
statement ok
CREATE TABLE t(a INTEGER, b TEXT)

statement ok
INSERT INTO t VALUES (3, 'x'), (1, ''), (2, NULL)

";

fn run(body: &str) -> FileResult {
    run_script(&format!("{SETUP}{body}"), "fixture.test").expect("fixture parses")
}

fn queries(passed: u64, failed: u64) -> Counts {
    Counts { passed, failed }
}

#[test]
fn correct_results_pass_and_wrong_results_fail() {
    let result = run("\
query I rowsort
SELECT a FROM t
----
1
2
3

query I rowsort
SELECT a FROM t
----
1
2
4

query I nosort
SELECT a FROM t
----
1
2
3
");
    // The third query is right only after sorting; nosort must reject it.
    assert_eq!(result.queries, queries(1, 2));
    assert_eq!(result.statements, queries(2, 0));
}

#[test]
fn values_are_formatted_by_the_declared_column_type() {
    let result = run("\
query R rowsort
SELECT a FROM t WHERE a = 3
----
3.000

query T rowsort
SELECT b FROM t
----
(empty)
NULL
x

query T rowsort
SELECT a / 2 FROM t WHERE a = 3
----
1
");
    assert_eq!(result.queries, queries(3, 0));
}

#[test]
fn large_reals_keep_sixteen_significant_digits_like_sqlite() {
    // Expected values are what sqlite3's printf('%.3f', ...) writes.
    let result = run("\
query RRRR nosort
SELECT 9223372036854775807.0, -123456789012345.67, 12345678901234.5678, 99999999999999.9999
----
9223372036854776000.000
-123456789012345.700
12345678901234.570
100000000000000.000
");
    assert_eq!(result.queries, queries(1, 0));
}

#[test]
fn hashed_results_compare_count_and_md5() {
    let result = run("\
hash-threshold 2

query I rowsort
SELECT a FROM t
----
3 values hashing to c0710d6b4f15dfa88f600b0e6b624077

query I rowsort
SELECT a FROM t
----
3 values hashing to 53c225db474ffb86c7e9459e87ebf56e

query I rowsort
SELECT a FROM t WHERE a > 1
----
2
3
");
    // The second hash is of the unsorted order, so it must fail.
    assert_eq!(result.queries, queries(2, 1));
}

#[test]
fn an_expected_hash_line_is_compared_by_hash_without_a_threshold() {
    // select1.test and select4.test have no hash-threshold line but still
    // list some results as hashes, as SQLite's runner allows.
    let result = run("\
query I rowsort
SELECT a FROM t
----
3 values hashing to c0710d6b4f15dfa88f600b0e6b624077

query I rowsort
SELECT a FROM t
----
3 values hashing to 53c225db474ffb86c7e9459e87ebf56e
");
    assert_eq!(result.queries, queries(1, 1));
    assert_eq!(result.failures[0].reason, "wrong result");
}

#[test]
fn wrong_column_count_fails_even_if_values_match() {
    let result = run("\
query II rowsort
SELECT a FROM t WHERE a > 1
----
2
3
");
    assert_eq!(result.queries, queries(0, 1));
    assert_eq!(result.failures[0].reason, "wrong column count");
}

#[test]
fn an_error_while_running_reads_as_no_rows_like_sqlites_runner() {
    let result = run("\
query I nosort
SELECT sum(a) FROM (SELECT 9223372036854775807 AS a UNION ALL SELECT 1)
----

query I nosort
WITH c AS (SELECT a FROM t) SELECT a FROM c
----
");
    assert_eq!(result.queries, queries(1, 1));
    assert_eq!(
        result.failures[0].reason,
        "unsupported: WITH (common table expressions)"
    );
}

#[test]
fn unsupported_sql_never_satisfies_statement_error() {
    let result = run("\
statement error
WITH c AS (SELECT a FROM t) SELECT a FROM c

statement error
SELECT nope FROM t

statement ok
SELECT nope FROM t
");
    assert_eq!(result.statements, queries(3, 2));
    let reasons: Vec<&str> = result.failures.iter().map(|f| f.reason.as_str()).collect();
    assert_eq!(
        reasons,
        [
            "unsupported: WITH (common table expressions)",
            "no such column"
        ]
    );
}

#[test]
fn conditions_skip_records_and_halt_respects_them() {
    let result = run("\
onlyif mysql
statement ok
SELECT nope FROM t

skipif sqlite # not run by the sqlite label
query I nosort
SELECT 999
----
0

onlyif mssql
halt

query I nosort
SELECT 1
----
1

halt

query I nosort
SELECT 2
----
0
");
    // The mssql-only halt is ignored and must not leak onto `SELECT 1`;
    // the plain halt stops the file before the wrong `SELECT 2`.
    assert_eq!(result.skipped, 2);
    assert_eq!(result.queries, queries(1, 0));
}

#[test]
fn a_statement_record_runs_every_statement_in_it() {
    // SQLite's runner passes statement records to sqlite3_exec, which runs
    // them all. slt_lang_aggfunc.test relies on that.
    let result = run("\
statement ok
INSERT INTO t VALUES (4, 'y');
INSERT INTO t VALUES (5, 'z');

statement error
INSERT INTO t VALUES (6, 'w');
SELECT nope FROM t

query I rowsort
SELECT a FROM t WHERE a > 3
----
4
5
6
");
    // The failing second statement does not undo the first, as in sqlite3_exec.
    assert_eq!(result.statements, queries(4, 0));
    assert_eq!(result.queries, queries(1, 0));
}

#[test]
fn parse_errors_are_grouped_by_cause_without_position() {
    let result = run("\
statement ok
SELECT a FROM t WHERE a IN t

statement ok
SELECT b FROM t WHERE b IN t
");
    let reasons: Vec<&str> = result.failures.iter().map(|f| f.reason.as_str()).collect();
    assert_eq!(
        reasons,
        ["parse error: Expected: (", "parse error: Expected: ("]
    );
}
