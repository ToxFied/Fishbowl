//! Indexes, PRIMARY KEY and UNIQUE: constraints checked on every write,
//! statements that fail leaving no trace, and IndexScan returning exactly
//! what a full scan would. Expected behavior was checked against sqlite3.

use fishbowl::{Database, Error, PlanProfile, StatementResult, Value};
use proptest::prelude::*;

fn query(db: &mut Database, sql: &str) -> (Vec<Vec<Value>>, PlanProfile) {
    match db.execute(sql).expect("query should run") {
        StatementResult::Query(result) => (result.rows, result.plan),
        other => panic!("expected rows, got {other:?}"),
    }
}

fn rows(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    query(db, sql).0
}

/// The rows as text, sorted, so results compare regardless of order.
fn sorted(rows: Vec<Vec<Value>>) -> Vec<String> {
    let mut out: Vec<String> = rows.iter().map(|row| format!("{row:?}")).collect();
    out.sort();
    out
}

fn int(i: i64) -> Value {
    Value::Integer(i)
}

fn text(s: &str) -> Value {
    Value::Text(s.into())
}

fn run(db: &mut Database, script: &str) {
    for entry in db.run_script(script) {
        entry
            .result
            .unwrap_or_else(|e| panic!("{}: {e}", entry.sql));
    }
}

fn error(db: &mut Database, sql: &str) -> String {
    db.execute(sql)
        .expect_err("statement should fail")
        .to_string()
}

/// The scan at the bottom of a single-table plan.
fn scan(plan: &PlanProfile) -> &PlanProfile {
    match plan.children.first() {
        Some(child) => scan(child),
        None => plan,
    }
}

#[test]
fn integer_primary_key_is_the_rowid() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE p(x INTEGER PRIMARY KEY, y);
         INSERT INTO p VALUES ('5', 'text five');
         INSERT INTO p VALUES (NULL, 'next');
         INSERT INTO p VALUES (2.0, 'real two');",
    );
    assert_eq!(
        error(&mut db, "INSERT INTO p VALUES (6.5, 1)"),
        "datatype mismatch"
    );
    assert_eq!(
        error(&mut db, "INSERT INTO p VALUES ('abc', 1)"),
        "datatype mismatch"
    );
    assert_eq!(
        error(&mut db, "INSERT INTO p VALUES (5, 'again')"),
        "UNIQUE constraint failed: p.x"
    );
    assert_eq!(
        error(&mut db, "UPDATE p SET x = 6 WHERE x = 5"),
        "UNIQUE constraint failed: p.x"
    );
    assert_eq!(
        error(&mut db, "UPDATE p SET x = NULL WHERE x = 5"),
        "datatype mismatch"
    );
    // Changing the key moves the row.
    run(&mut db, "UPDATE p SET x = 9 WHERE x = 6");
    assert_eq!(
        rows(&mut db, "SELECT x, y FROM p"),
        [
            vec![int(2), text("real two")],
            vec![int(5), text("text five")],
            vec![int(9), text("next")],
        ]
    );
    db.integrity_check().unwrap();
}

#[test]
fn unique_allows_many_nulls_and_compares_like_equals() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE t(a UNIQUE);
         INSERT INTO t VALUES (3), (NULL), (NULL), ('3');",
    );
    // 3.0 = 3, but the text '3' in an untyped column is a different value.
    assert_eq!(
        error(&mut db, "INSERT INTO t VALUES (3.0)"),
        "UNIQUE constraint failed: t.a"
    );
    run(
        &mut db,
        "CREATE TABLE q(a TEXT PRIMARY KEY, b, UNIQUE (a, b));
         INSERT INTO q VALUES (NULL, 1), (NULL, 1), ('k', 1);",
    );
    assert_eq!(
        error(&mut db, "INSERT INTO q VALUES ('k', 2)"),
        "UNIQUE constraint failed: q.a"
    );
    assert_eq!(rows(&mut db, "SELECT count(*) FROM q"), [vec![int(3)]]);
    db.integrity_check().unwrap();
}

#[test]
fn a_failing_statement_leaves_no_trace() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE u(id INTEGER PRIMARY KEY, a INTEGER UNIQUE, note TEXT NOT NULL);
         CREATE INDEX u_note ON u(note);
         INSERT INTO u VALUES (1, 1, 'one'), (2, 2, 'two'), (3, 3, 'three');",
    );
    let before = rows(&mut db, "SELECT * FROM u");

    // The third row breaks UNIQUE after two new rows went in.
    let e = error(
        &mut db,
        "INSERT INTO u VALUES (10, 10, 'x'), (11, 11, 'y'), (12, 1, 'z')",
    );
    assert_eq!(e, "UNIQUE constraint failed: u.a");
    // Row 1 becomes 2 while row 2 still holds 2, as in SQLite.
    let e = error(&mut db, "UPDATE u SET a = a + 1");
    assert_eq!(e, "UNIQUE constraint failed: u.a");
    // The last row breaks NOT NULL after two rows were already changed.
    let e = error(
        &mut db,
        "UPDATE u SET note = CASE id WHEN 3 THEN NULL ELSE 'n' END",
    );
    assert_eq!(e, "NOT NULL constraint failed: u.note");

    assert_eq!(rows(&mut db, "SELECT * FROM u"), before);
    assert!(rows(&mut db, "SELECT id FROM u WHERE note = 'n'").is_empty());
    db.integrity_check().unwrap();
    // Going the other way, each row moves into a free value.
    run(&mut db, "UPDATE u SET a = a - 1");
    assert_eq!(rows(&mut db, "SELECT a FROM u WHERE a < 1"), [vec![int(0)]]);
    db.integrity_check().unwrap();
}

#[test]
fn insert_or_rules_resolve_conflicts_like_sqlite() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b UNIQUE, c NOT NULL);
         INSERT INTO t VALUES (1, 'x', 1), (2, 'y', 2), (3, 'z', 3);
         INSERT OR REPLACE INTO t VALUES (2, 'x', 9);",
    );
    let pairs = |db: &mut Database| -> Vec<(i64, String)> {
        rows(db, "SELECT a, b FROM t")
            .into_iter()
            .map(|r| (r[0].to_i64_lossy(), r[1].to_string()))
            .collect()
    };
    // Rows 1 (b = 'x') and 2 (a = 2) both clashed and are gone.
    assert_eq!(pairs(&mut db), [(2, "x".into()), (3, "z".into())]);

    let result = db
        .execute(
            "INSERT OR IGNORE INTO t VALUES (3, 'q', 1), (4, 'z', 1), (5, 'w', NULL), (6, 'v', 6)",
        )
        .unwrap();
    assert!(matches!(result, StatementResult::Insert { rows: 1, .. }));

    // OR FAIL keeps the rows before the failing one.
    let e = error(
        &mut db,
        "INSERT OR FAIL INTO t VALUES (7, 'u', 7), (8, 'v', 8), (9, 't', 9)",
    );
    assert_eq!(e, "UNIQUE constraint failed: t.b");
    // REPLACE deletes row 6 (b = 'v'), then NOT NULL fails and row 6 returns.
    let e = error(&mut db, "REPLACE INTO t VALUES (10, 'v', NULL)");
    assert_eq!(e, "NOT NULL constraint failed: t.c");
    assert_eq!(
        pairs(&mut db),
        [
            (2, "x".into()),
            (3, "z".into()),
            (6, "v".into()),
            (7, "u".into())
        ]
    );
    db.integrity_check().unwrap();
}

#[test]
fn create_index_fills_from_existing_rows_and_checks_uniqueness() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE d(a, b);
         INSERT INTO d VALUES (1, 'x'), (1, 'y'), (NULL, 'z'), (NULL, 'w');
         CREATE INDEX d_a ON d(a);
         CREATE UNIQUE INDEX d_b ON d(b);",
    );
    assert_eq!(
        error(&mut db, "CREATE UNIQUE INDEX d_a2 ON d(a)"),
        "UNIQUE constraint failed: d.a"
    );
    // The failed index is gone, name and all.
    run(&mut db, "CREATE INDEX d_a2 ON d(a, b DESC)");
    assert_eq!(
        error(&mut db, "INSERT INTO d VALUES (5, 'x')"),
        "UNIQUE constraint failed: d.b"
    );
    db.integrity_check().unwrap();
}

#[test]
fn index_names_follow_sqlite_rules() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE r(a INT PRIMARY KEY, b);
         CREATE INDEX i ON r(b);
         CREATE INDEX IF NOT EXISTS i ON r(a);",
    );
    assert_eq!(
        error(&mut db, "CREATE INDEX i ON r(a)"),
        "index i already exists"
    );
    assert_eq!(
        error(&mut db, "CREATE TABLE i(x)"),
        "there is already an index named i"
    );
    assert_eq!(
        error(&mut db, "CREATE INDEX r ON r(a)"),
        "there is already a table named r"
    );
    assert_eq!(
        error(&mut db, "CREATE INDEX j ON r(nope)"),
        "no such column: nope"
    );
    assert_eq!(
        error(&mut db, "CREATE INDEX j ON nope(a)"),
        "no such table: nope"
    );
    assert_eq!(
        error(&mut db, "CREATE INDEX sqlite_x ON r(a)"),
        "object name reserved for internal use: sqlite_x"
    );
    assert_eq!(
        error(&mut db, "DROP INDEX sqlite_autoindex_r_1"),
        "index associated with UNIQUE or PRIMARY KEY constraint cannot be dropped"
    );
    run(&mut db, "DROP INDEX i; DROP INDEX IF EXISTS i");
    assert_eq!(error(&mut db, "DROP INDEX i"), "no such index: i");
    assert_eq!(
        error(&mut db, "INSERT INTO r VALUES (1, 1), (1, 2)"),
        "UNIQUE constraint failed: r.a"
    );
    // Dropping the table drops its indexes, freeing their names.
    run(
        &mut db,
        "CREATE INDEX k ON r(b); DROP TABLE r; CREATE TABLE k(x)",
    );
    assert!(matches!(
        db.execute("CREATE TABLE z(a INTEGER PRIMARY KEY, b PRIMARY KEY)"),
        Err(Error::Catalog(_))
    ));
}

#[test]
fn where_terms_on_an_indexed_column_use_an_index_scan() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE tab(pk INTEGER PRIMARY KEY, a INTEGER, b TEXT);
         CREATE INDEX tab_a ON tab(a);
         INSERT INTO tab VALUES (1, 10, 'x'), (2, 20, 'y'), (3, 30, 'z'), (4, 20, NULL), (5, NULL, 'w');",
    );
    let cases = [
        (
            "SELECT pk FROM tab WHERE a = '20'",
            "tab USING INDEX tab_a (a = 20)",
            vec![2, 4],
        ),
        (
            "SELECT pk FROM tab WHERE a < 25 AND 15 < a AND a <= 30",
            "tab USING INDEX tab_a (a > 15 AND a < 25)",
            vec![2, 4],
        ),
        (
            "SELECT pk FROM tab WHERE b = 'z' AND pk BETWEEN 2 AND 3",
            "tab USING INTEGER PRIMARY KEY (pk >= 2 AND pk <= 3)",
            vec![3],
        ),
        (
            "SELECT pk FROM tab WHERE a IN (30, 10, 10.0, NULL)",
            "tab USING INDEX tab_a (a IN (10, 30))",
            vec![1, 3],
        ),
        (
            "SELECT pk FROM tab WHERE pk = 4",
            "tab USING INTEGER PRIMARY KEY (pk = 4)",
            vec![4],
        ),
        (
            "SELECT pk FROM tab WHERE a < 15",
            "tab USING INDEX tab_a (a < 15)",
            vec![1],
        ),
    ];
    for (sql, detail, expected) in cases {
        let (found, plan) = query(&mut db, sql);
        let scan = scan(&plan);
        assert_eq!(
            (scan.operator, scan.detail.as_str()),
            ("IndexScan", detail),
            "{sql}"
        );
        assert!(
            scan.reason
                .as_ref()
                .is_some_and(|r| r.contains("the WHERE term"))
        );
        let found: Vec<i64> = found.iter().map(|r| r[0].to_i64_lossy()).collect();
        assert_eq!(found, expected, "{sql}");
    }

    // The index read only the two matching rows, not all five.
    let (_, plan) = query(&mut db, "SELECT pk FROM tab WHERE a = 20");
    assert_eq!(scan(&plan).rows_out, 2);

    for sql in [
        "SELECT pk FROM tab WHERE b = 'x'",
        "SELECT pk FROM tab WHERE a = 10 OR a = 20",
        "SELECT pk FROM tab WHERE a + 0 = 10",
        "SELECT pk FROM tab WHERE a = pk",
        "SELECT pk FROM tab WHERE a = NULL",
        "SELECT pk FROM tab",
    ] {
        let (_, plan) = query(&mut db, sql);
        assert_eq!(scan(&plan).operator, "SeqScan", "{sql}");
    }
}

#[test]
fn insert_select_copies_rows_through_the_constraints() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE src(a, b);
         INSERT INTO src VALUES (1, 'x'), (2, 'y'), (3, 'x');
         CREATE TABLE dst(id INTEGER PRIMARY KEY, b UNIQUE);",
    );
    assert_eq!(
        error(&mut db, "INSERT INTO dst SELECT a, b FROM src"),
        "UNIQUE constraint failed: dst.b"
    );
    assert!(rows(&mut db, "SELECT * FROM dst").is_empty());
    run(&mut db, "INSERT INTO dst SELECT a, b FROM src WHERE a < 3");
    assert_eq!(
        rows(&mut db, "SELECT id FROM dst WHERE b = 'y'"),
        [vec![int(2)]]
    );
}

#[test]
fn indexes_survive_reopening_the_file() {
    let path = std::env::temp_dir().join(format!("fishbowl-index-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::open(&path).unwrap();
        run(
            &mut db,
            "CREATE TABLE t(id INTEGER PRIMARY KEY, code TEXT UNIQUE, n INTEGER);
             CREATE INDEX t_n ON t(n DESC);
             CREATE INDEX gone ON t(code, n);
             INSERT INTO t VALUES (1, 'a', 5), (2, 'b', 6), (3, 'c', 5);
             DROP INDEX gone;
             DELETE FROM t WHERE id = 2;",
        );
    }
    let mut db = Database::open(&path).unwrap();
    db.integrity_check().unwrap();
    let (found, plan) = query(&mut db, "SELECT id FROM t WHERE n = 5");
    assert_eq!(found, [vec![int(1)], vec![int(3)]]);
    assert_eq!(scan(&plan).detail, "t USING INDEX t_n (n = 5)");
    assert_eq!(
        error(&mut db, "INSERT INTO t VALUES (9, 'a', 1)"),
        "UNIQUE constraint failed: t.code"
    );
    assert_eq!(
        error(&mut db, "INSERT INTO t VALUES (3, 'z', 1)"),
        "UNIQUE constraint failed: t.id"
    );
    let names: Vec<String> = db.tables()[0]
        .schema
        .indexes
        .iter()
        .map(|i| i.name.clone())
        .collect();
    assert_eq!(names, ["sqlite_autoindex_t_1", "t_n"]);
    drop(db);
    std::fs::remove_file(&path).unwrap();
}

/// Literals that test how comparisons convert types: numbers as text,
/// reals that equal integers, text that is not a number, and NULL.
fn literal() -> impl Strategy<Value = String> {
    prop_oneof![
        (-3i64..8).prop_map(|i| i.to_string()),
        (-3i64..8).prop_map(|i| format!("'{i}'")),
        (-3i64..8).prop_map(|i| format!("{i}.5")),
        (-3i64..8).prop_map(|i| format!("{i}.0")),
        prop::sample::select(vec!["'a'", "'b'", "''", "NULL", "9223372036854775807"])
            .prop_map(str::to_string),
    ]
}

/// A value to store: the same mix as [`literal`].
fn stored() -> impl Strategy<Value = String> {
    literal()
}

const COLUMNS: [&str; 5] = ["k", "a", "b", "c", "d"];

fn term() -> impl Strategy<Value = String> {
    let column = prop::sample::select(COLUMNS.to_vec());
    let op = prop::sample::select(vec!["=", "<", "<=", ">", ">="]);
    prop_oneof![
        (column.clone(), op.clone(), literal()).prop_map(|(c, op, v)| format!("{c} {op} {v}")),
        (literal(), op, column.clone()).prop_map(|(v, op, c)| format!("{v} {op} {c}")),
        (column.clone(), literal(), literal())
            .prop_map(|(c, lo, hi)| format!("{c} BETWEEN {lo} AND {hi}")),
        (column, prop::collection::vec(literal(), 0..4))
            .prop_map(|(c, list)| format!("{c} IN ({})", list.join(", "))),
    ]
}

fn predicate() -> impl Strategy<Value = String> {
    prop_oneof![
        4 => prop::collection::vec(term(), 1..4).prop_map(|terms| terms.join(" AND ")),
        1 => (term(), term()).prop_map(|(a, b)| format!("{a} OR {b}")),
    ]
}

#[derive(Debug, Clone)]
enum Write {
    Insert(Vec<[String; 3]>, &'static str),
    Update(&'static str, String, String),
    Delete(String),
}

fn write() -> impl Strategy<Value = Write> {
    // Small domains, so keys clash often.
    let key = prop_oneof![
        (0i64..6).prop_map(|i| i.to_string()),
        Just("NULL".to_string())
    ];
    let row = (key.clone(), key.clone(), stored()).prop_map(|(a, b, c)| [a, b, c]);
    let rule = prop::sample::select(vec!["", "OR REPLACE ", "OR IGNORE ", "OR FAIL "]);
    let column = prop::sample::select(vec!["id", "u", "v"]);
    let filter = (column.clone(), key.clone()).prop_map(|(c, k)| format!("{c} >= {k}"));
    prop_oneof![
        3 => (prop::collection::vec(row, 1..4), rule).prop_map(|(rows, rule)| Write::Insert(rows, rule)),
        2 => (column, prop_oneof![key.prop_map(|k| k), Just("v + 1".to_string()), Just("u - 1".to_string())], filter.clone())
            .prop_map(|(c, value, filter)| Write::Update(c, value, filter)),
        1 => filter.prop_map(Write::Delete),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Every index must hold exactly one entry per row, with the row's
    /// values, after any mix of writes. A statement that fails must leave
    /// the table as it found it (except `OR FAIL`, which keeps earlier rows).
    #[test]
    fn random_writes_keep_indexes_in_step(writes in prop::collection::vec(write(), 1..40)) {
        let mut db = Database::new();
        run(&mut db, "CREATE TABLE t(id INTEGER PRIMARY KEY, u INTEGER UNIQUE, v);
                      CREATE INDEX t_v ON t(v);
                      CREATE UNIQUE INDEX t_vu ON t(v, u DESC)");
        for write in &writes {
            let sql = match write {
                Write::Insert(rows, rule) => {
                    let rows: Vec<String> = rows.iter().map(|r| format!("({})", r.join(", "))).collect();
                    format!("INSERT {rule}INTO t VALUES {}", rows.join(", "))
                }
                Write::Update(column, value, filter) => format!("UPDATE t SET {column} = {value} WHERE {filter}"),
                Write::Delete(filter) => format!("DELETE FROM t WHERE {filter}"),
            };
            let before = sorted(rows(&mut db, "SELECT * FROM t"));
            let result = db.execute(&sql);
            if let Err(e) = &result {
                prop_assert!(matches!(e, Error::Constraint(_) | Error::Runtime(_)), "{sql}: {e}");
                if !sql.contains("OR FAIL") {
                    prop_assert_eq!(sorted(rows(&mut db, "SELECT * FROM t")), before, "{}", sql);
                }
            }
            if let Err(e) = db.integrity_check() {
                return Err(TestCaseError::fail(format!("after {sql}: {e}")));
            }
        }
    }

    /// The same rows, the same WHERE clause: a table with indexes must give
    /// exactly the answer a table without them gives.
    #[test]
    fn queries_match_with_and_without_indexes(
        data in prop::collection::vec([stored(), stored(), stored(), stored()], 0..30),
        predicates in prop::collection::vec(predicate(), 1..12),
    ) {
        let mut plain = Database::new();
        let mut indexed = Database::new();
        run(&mut plain, "CREATE TABLE t(k INTEGER, a INTEGER, b REAL, c TEXT, d)");
        run(&mut indexed, "CREATE TABLE t(k INTEGER PRIMARY KEY, a INTEGER, b REAL, c TEXT, d);
                           CREATE INDEX t_a ON t(a);
                           CREATE INDEX t_b ON t(b DESC);
                           CREATE INDEX t_c ON t(c);
                           CREATE INDEX t_d ON t(d);
                           CREATE INDEX t_ca ON t(c, a)");
        for (i, row) in data.iter().enumerate() {
            let sql = format!("INSERT INTO t VALUES ({}, {})", i + 1, row.join(", "));
            run(&mut plain, &sql);
            run(&mut indexed, &sql);
        }
        indexed.integrity_check().unwrap();
        for predicate in &predicates {
            let sql = format!("SELECT k, a, b, c, d FROM t WHERE {predicate}");
            prop_assert_eq!(
                sorted(rows(&mut indexed, &sql)),
                sorted(rows(&mut plain, &sql)),
                "{}", sql
            );
        }
    }
}
