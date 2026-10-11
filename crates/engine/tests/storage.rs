//! UPDATE, DELETE and DROP TABLE, and data that outlives the process.

use fishbowl::{Database, Error, StatementResult, Value};
use proptest::prelude::*;

fn rows(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    match db.execute(sql).expect("query should run") {
        StatementResult::Query(result) => result.rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn int(i: i64) -> Value {
    Value::Integer(i)
}

fn text(s: &str) -> Value {
    Value::Text(s.into())
}

fn fish() -> Database {
    let mut db = Database::new();
    db.execute("CREATE TABLE fish(id INTEGER, name TEXT, weight INTEGER NOT NULL)")
        .unwrap();
    db.execute("INSERT INTO fish VALUES (1, 'goldie', 2), (2, 'bubbles', 15), (3, 'nemo', 1)")
        .unwrap();
    db
}

#[test]
fn update_changes_matching_rows_and_reports_the_count() {
    let mut db = fish();
    let result = db
        .execute("UPDATE fish SET weight = weight * 10, name = 'big ' || name WHERE weight < 5")
        .unwrap();
    assert!(matches!(result, StatementResult::Update { rows: 2, .. }));
    assert_eq!(
        rows(&mut db, "SELECT name, weight FROM fish"),
        [
            vec![text("big goldie"), int(20)],
            vec![text("bubbles"), int(15)],
            vec![text("big nemo"), int(10)],
        ]
    );
}

#[test]
fn update_reads_every_value_from_the_old_row() {
    let mut db = Database::new();
    db.execute("CREATE TABLE t(a, b)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 2)").unwrap();
    db.execute("UPDATE t SET a = b, b = a").unwrap();
    assert_eq!(rows(&mut db, "SELECT a, b FROM t"), [vec![int(2), int(1)]]);
}

#[test]
fn update_applies_affinity_and_constraints() {
    let mut db = fish();
    db.execute("UPDATE fish SET id = '40' WHERE id = 3")
        .unwrap();
    assert_eq!(
        rows(&mut db, "SELECT id FROM fish WHERE id = 40"),
        [vec![int(40)]]
    );

    // One bad row fails the whole statement, and no row changes.
    let err = db
        .execute("UPDATE fish SET weight = NULL WHERE id > 1")
        .unwrap_err();
    assert!(matches!(err, Error::Constraint(_)));
    assert_eq!(
        rows(&mut db, "SELECT weight FROM fish WHERE weight IS NULL").len(),
        0
    );
}

#[test]
fn update_and_delete_reject_unknown_names() {
    let mut db = fish();
    for sql in [
        "UPDATE fish SET nope = 1",
        "UPDATE fish SET name = nope",
        "UPDATE nope SET a = 1",
        "DELETE FROM fish WHERE nope = 1",
        "DELETE FROM nope",
    ] {
        assert!(
            matches!(db.execute(sql), Err(Error::Catalog(_))),
            "{sql} should fail"
        );
    }
}

#[test]
fn delete_removes_matching_rows() {
    let mut db = fish();
    let result = db.execute("DELETE FROM fish WHERE weight > 1").unwrap();
    assert!(matches!(result, StatementResult::Delete { rows: 2, .. }));
    assert_eq!(rows(&mut db, "SELECT name FROM fish"), [vec![text("nemo")]]);
    assert_eq!(db.tables()[0].row_count, 1);

    db.execute("DELETE FROM fish").unwrap();
    assert_eq!(rows(&mut db, "SELECT * FROM fish").len(), 0);
    assert_eq!(db.tables()[0].row_count, 0);
}

#[test]
fn drop_table_removes_the_table_and_its_name() {
    let mut db = fish();
    db.execute("DROP TABLE fish").unwrap();
    assert!(db.tables().is_empty());
    assert!(matches!(
        db.execute("SELECT * FROM fish"),
        Err(Error::Catalog(_))
    ));
    assert!(matches!(
        db.execute("DROP TABLE fish"),
        Err(Error::Catalog(_))
    ));
    assert!(db.execute("DROP TABLE IF EXISTS fish").is_ok());

    // The name is free again, and the new table starts empty.
    db.execute("CREATE TABLE fish(x)").unwrap();
    assert_eq!(rows(&mut db, "SELECT * FROM fish").len(), 0);
}

#[test]
fn rows_larger_than_a_page_round_trip() {
    let mut db = Database::new();
    db.execute("CREATE TABLE notes(body TEXT)").unwrap();
    let long = "fish ".repeat(5_000);
    db.execute(&format!("INSERT INTO notes VALUES ('{long}'), ('short')"))
        .unwrap();
    assert_eq!(
        rows(&mut db, "SELECT body FROM notes"),
        [vec![text(&long)], vec![text("short")]]
    );
}

#[test]
fn many_rows_span_many_pages() {
    let mut db = Database::new();
    db.execute("CREATE TABLE t(n INTEGER, label TEXT)").unwrap();
    for chunk in 0..20 {
        let values: Vec<String> = (0..250)
            .map(|i| format!("({}, 'row number {}')", chunk * 250 + i, chunk * 250 + i))
            .collect();
        db.execute(&format!("INSERT INTO t VALUES {}", values.join(", ")))
            .unwrap();
    }
    let all = rows(&mut db, "SELECT n FROM t");
    assert_eq!(all.len(), 5_000);
    assert!(
        all.iter()
            .enumerate()
            .all(|(i, row)| row[0] == int(i as i64))
    );
    db.execute("DELETE FROM t WHERE n > 99").unwrap();
    assert_eq!(rows(&mut db, "SELECT n FROM t").len(), 100);
}

#[test]
fn data_survives_reopening_the_file() {
    let path = std::env::temp_dir().join(format!("fishbowl-test-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE fish(id INTEGER, name TEXT NOT NULL)")
            .unwrap();
        db.execute("CREATE TABLE gone(x)").unwrap();
        db.execute("INSERT INTO fish VALUES (1, 'goldie'), (2, 'nemo'), (3, 'dory')")
            .unwrap();
        db.execute("UPDATE fish SET name = 'bubbles' WHERE id = 2")
            .unwrap();
        db.execute("DELETE FROM fish WHERE id = 3").unwrap();
        db.execute("DROP TABLE gone").unwrap();
    }
    let mut db = Database::open(&path).unwrap();
    assert_eq!(
        rows(&mut db, "SELECT id, name FROM fish"),
        [vec![int(1), text("goldie")], vec![int(2), text("bubbles")]]
    );
    assert_eq!(db.tables().len(), 1);
    // The schema came back too, constraints included.
    assert!(matches!(
        db.execute("INSERT INTO fish VALUES (4, NULL)"),
        Err(Error::Constraint(_))
    ));
    drop(db);
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn a_file_that_is_not_a_database_is_rejected() {
    let path = std::env::temp_dir().join(format!("fishbowl-junk-{}.db", std::process::id()));
    std::fs::write(&path, vec![b'x'; 4096]).unwrap();
    assert!(matches!(Database::open(&path), Err(Error::Storage(_))));
    std::fs::remove_file(&path).unwrap();
}

#[derive(Debug, Clone)]
enum Dml {
    Insert(i64),
    Update { below: i64, add: i64 },
    Delete { above: i64 },
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Random INSERT, UPDATE and DELETE statements must leave the table
    /// holding exactly what a plain list would hold.
    #[test]
    fn dml_matches_a_list(statements in prop::collection::vec(
        prop_oneof![
            3 => (0i64..100).prop_map(Dml::Insert),
            1 => (0i64..100, -5i64..5).prop_map(|(below, add)| Dml::Update { below, add }),
            1 => (0i64..100).prop_map(|above| Dml::Delete { above }),
        ],
        0..80,
    )) {
        let mut db = Database::new();
        db.execute("CREATE TABLE t(v INTEGER, pad TEXT)").unwrap();
        let pad = "p".repeat(300);
        let mut model: Vec<i64> = Vec::new();
        for statement in &statements {
            match *statement {
                Dml::Insert(v) => {
                    db.execute(&format!("INSERT INTO t VALUES ({v}, '{pad}')")).unwrap();
                    model.push(v);
                }
                Dml::Update { below, add } => {
                    db.execute(&format!("UPDATE t SET v = v + {add} WHERE v < {below}")).unwrap();
                    model.iter_mut().filter(|v| **v < below).for_each(|v| *v += add);
                }
                Dml::Delete { above } => {
                    db.execute(&format!("DELETE FROM t WHERE v > {above}")).unwrap();
                    model.retain(|&v| v <= above);
                }
            }
        }
        let stored: Vec<i64> = rows(&mut db, "SELECT v FROM t")
            .into_iter()
            .map(|row| row[0].to_i64_lossy())
            .collect();
        prop_assert_eq!(stored, model);
    }
}
