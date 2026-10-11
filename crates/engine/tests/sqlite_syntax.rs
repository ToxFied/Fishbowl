//! SQLite syntax beyond the SQL standard: blob literals, `x IN table`,
//! `INDEXED BY` and `NOT INDEXED`, `REINDEX`, and `min(DISTINCT ...)` beside
//! plain columns. Expected answers come from SQLite 3.54.

use fishbowl::{Database, Error, StatementResult};

fn setup() -> Database {
    let mut db = Database::new();
    for sql in [
        "CREATE TABLE t(x, y)",
        "INSERT INTO t VALUES (1, 9), (0, 8), (2, 7), (2, 6)",
        "CREATE INDEX tx ON t(x)",
        "CREATE INDEX ty ON t(y)",
        "CREATE TABLE two(a, b)",
    ] {
        db.execute(sql).unwrap();
    }
    db
}

/// The first row's values joined by `|`, like the sqlite3 shell prints them.
fn row(db: &mut Database, sql: &str) -> String {
    match db.execute(sql) {
        Ok(StatementResult::Query(result)) => result.rows[0]
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("|"),
        other => panic!("{sql}: expected rows, got {other:?}"),
    }
}

fn error(db: &mut Database, sql: &str) -> String {
    db.execute(sql).unwrap_err().to_string()
}

#[test]
fn blobs_compare_after_text_and_read_as_text_in_arithmetic() {
    let mut db = setup();
    assert_eq!(
        row(
            &mut db,
            "SELECT typeof(x'41'), length(x'C3A9'), x'41' > 'zzz', x'41' = 'A', \
             x'00' < x'0000', x'31' + 1, -x'31', x'61' || 'b', typeof(CAST(12 AS BLOB))"
        ),
        "blob|2|1|0|1|2|-1|ab|blob"
    );
    // A blob is stored and indexed as itself, never converted by affinity,
    // and sorts after the numbers in the index the query reads.
    db.execute("CREATE TABLE b(v INTEGER)").unwrap();
    db.execute("CREATE INDEX bv ON b(v)").unwrap();
    db.execute("INSERT INTO b VALUES (x'3132'), (12), ('12')")
        .unwrap();
    assert_eq!(
        row(&mut db, "SELECT group_concat(typeof(v)) FROM b"),
        "blob,integer,integer"
    );
    assert_eq!(row(&mut db, "SELECT count(DISTINCT v) FROM b"), "2");
    assert_eq!(
        row(&mut db, "SELECT count(*) FROM b WHERE v = x'3132'"),
        "1"
    );
    assert_eq!(
        error(&mut db, "SELECT x'abc'"),
        "parse error: unrecognized token: \"X'abc'\""
    );
}

#[test]
fn in_a_table_means_in_its_rows() {
    let mut db = setup();
    db.execute("CREATE VIEW v AS SELECT x FROM t").unwrap();
    assert_eq!(
        row(&mut db, "SELECT 1 IN v, 5 IN v, 5 NOT IN v, x'303132' IN v"),
        "1|0|1|0"
    );
    assert_eq!(
        error(&mut db, "SELECT 1 IN two"),
        "parse error: sub-select returns 2 columns - expected 1"
    );
    assert_eq!(
        error(&mut db, "SELECT 1 IN nosuch"),
        "no such table: nosuch"
    );
}

#[test]
fn index_hints_decide_the_read_order() {
    let mut db = setup();
    let concat = "SELECT group_concat(x) FROM t";
    assert_eq!(row(&mut db, concat), "0,1,2,2");
    assert_eq!(row(&mut db, &format!("{concat} NOT INDEXED")), "1,0,2,2");
    assert_eq!(row(&mut db, &format!("{concat} INDEXED BY ty")), "2,2,0,1");
    assert_eq!(
        row(&mut db, &format!("{concat} AS a INDEXED BY TY WHERE y > 0")),
        "2,2,0,1"
    );
    // A string or a comment that looks like a hint is left alone.
    assert_eq!(
        row(&mut db, "SELECT 'NOT INDEXED' FROM t -- INDEXED BY nosuch"),
        "NOT INDEXED"
    );
    for (sql, index) in [
        ("SELECT x FROM t INDEXED BY nosuch", "nosuch"),
        // The index exists, but on another table.
        ("SELECT a FROM two INDEXED BY tx", "tx"),
        ("DELETE FROM t INDEXED BY nosuch", "nosuch"),
    ] {
        assert_eq!(error(&mut db, sql), format!("no such index: {index}"));
    }
}

#[test]
fn reindex_checks_the_name_and_changes_nothing() {
    let mut db = setup();
    for sql in ["REINDEX", "REINDEX t", "REINDEX tx", "REINDEX nocase"] {
        assert!(
            matches!(db.execute(sql), Ok(StatementResult::Reindex)),
            "{sql}"
        );
    }
    assert_eq!(
        db.execute("REINDEX nosuch").unwrap_err(),
        Error::Catalog("unable to identify the object to be reindexed".into())
    );
}

/// SQLite skips a repeated value without stepping min() or max(), and the
/// plain columns then follow whatever the row before decided.
#[test]
fn plain_columns_beside_min_or_max_distinct_follow_sqlite() {
    let mut db = Database::new();
    db.execute("CREATE TABLE m(a, b)").unwrap();
    for (rows, max, min) in [
        ("(1,'x'),(3,'y'),(3,'z'),(2,'w')", "z|3", "x|1"),
        ("(3,'a'),(1,'x'),(1,'y'),(3,'z')", "a|3", "z|1"),
        ("(NULL,'n'),(1,'x'),(NULL,'m')", "m|1", "m|1"),
        ("(1,'x'),(3,'y'),(NULL,'n'),(3,'z')", "y|3", "x|1"),
    ] {
        db.execute("DELETE FROM m").unwrap();
        db.execute(&format!("INSERT INTO m VALUES {rows}")).unwrap();
        assert_eq!(
            row(&mut db, "SELECT b, max(DISTINCT a) FROM m"),
            max,
            "{rows}"
        );
        assert_eq!(
            row(&mut db, "SELECT b, min(DISTINCT a) FROM m"),
            min,
            "{rows}"
        );
    }
}
