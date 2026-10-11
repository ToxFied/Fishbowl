//! BEGIN, COMMIT and ROLLBACK, autocommit, and what survives reopening a
//! database file.

use fishbowl::{Database, Error, StatementResult, Value};

fn rows(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    match db.execute(sql).expect("query should run") {
        StatementResult::Query(result) => result.rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn ids(db: &mut Database) -> Vec<i64> {
    rows(db, "SELECT id FROM t")
        .into_iter()
        .map(|row| row[0].to_i64_lossy())
        .collect()
}

fn run(db: &mut Database, script: &str) {
    for entry in db.run_script(script) {
        entry
            .result
            .unwrap_or_else(|e| panic!("{}: {e}", entry.sql));
    }
}

fn error(db: &mut Database, sql: &str) -> String {
    db.execute(sql).unwrap_err().to_string()
}

fn table() -> Database {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT UNIQUE);
         INSERT INTO t VALUES (1, 'a'), (2, 'b');",
    );
    db
}

#[test]
fn commit_keeps_changes_and_reads_see_them_before_it() {
    let mut db = table();
    run(
        &mut db,
        "BEGIN; INSERT INTO t VALUES (3, 'c'); DELETE FROM t WHERE id = 1",
    );
    assert!(db.in_transaction());
    assert_eq!(ids(&mut db), [2, 3]);
    assert!(matches!(db.execute("COMMIT"), Ok(StatementResult::Commit)));
    assert!(!db.in_transaction());
    assert_eq!(ids(&mut db), [2, 3]);
}

#[test]
fn rollback_undoes_rows_tables_and_indexes() {
    let mut db = table();
    run(
        &mut db,
        "BEGIN;
         INSERT INTO t VALUES (3, 'c');
         UPDATE t SET name = 'z' WHERE id = 2;
         DELETE FROM t WHERE id = 1;
         CREATE TABLE extra(x);
         INSERT INTO extra VALUES (1);
         CREATE INDEX t_name ON t(name);
         ROLLBACK;",
    );
    assert_eq!(
        rows(&mut db, "SELECT id, name FROM t"),
        [
            vec![Value::Integer(1), Value::Text("a".into())],
            vec![Value::Integer(2), Value::Text("b".into())],
        ]
    );
    assert!(matches!(
        db.execute("SELECT * FROM extra"),
        Err(Error::Catalog(_))
    ));
    assert_eq!(db.tables().len(), 1);
    assert_eq!(db.tables()[0].row_count, 2);
    assert_eq!(
        db.tables()[0].schema.indexes.len(),
        1,
        "only the UNIQUE index"
    );
    db.integrity_check().unwrap();

    // A dropped table comes back with its rows.
    run(&mut db, "BEGIN; DROP TABLE t; ROLLBACK");
    assert_eq!(ids(&mut db), [1, 2]);
    db.integrity_check().unwrap();
}

#[test]
fn transaction_statements_follow_sqlite() {
    let mut db = table();
    assert_eq!(
        error(&mut db, "COMMIT"),
        "cannot commit - no transaction is active"
    );
    assert_eq!(
        error(&mut db, "ROLLBACK"),
        "cannot rollback - no transaction is active"
    );
    db.execute("BEGIN").unwrap();
    assert_eq!(
        error(&mut db, "BEGIN"),
        "cannot start a transaction within a transaction"
    );
    // END is COMMIT, and the transaction keywords are optional.
    run(&mut db, "INSERT INTO t VALUES (3, 'c'); END TRANSACTION");
    run(
        &mut db,
        "BEGIN DEFERRED TRANSACTION; INSERT INTO t VALUES (4, 'd'); ROLLBACK TRANSACTION",
    );
    run(
        &mut db,
        "BEGIN IMMEDIATE; COMMIT TRANSACTION; BEGIN EXCLUSIVE; COMMIT",
    );
    assert_eq!(ids(&mut db), [1, 2, 3]);
    for sql in ["SAVEPOINT s", "BEGIN; ROLLBACK TO s"] {
        assert!(
            db.run_script(sql)
                .iter()
                .any(|e| matches!(e.result, Err(Error::Unsupported(_)))),
            "{sql}"
        );
    }
}

#[test]
fn a_failed_statement_undoes_only_itself() {
    let mut db = table();
    run(&mut db, "BEGIN; INSERT INTO t VALUES (3, 'c')");
    assert!(matches!(
        db.execute("INSERT INTO t VALUES (4, 'd'), (5, 'a')"),
        Err(Error::Constraint(_))
    ));
    assert!(db.in_transaction());
    run(&mut db, "COMMIT");
    assert_eq!(ids(&mut db), [1, 2, 3]);
}

#[test]
fn insert_or_rollback_ends_the_whole_transaction() {
    let mut db = table();
    run(&mut db, "BEGIN; INSERT INTO t VALUES (3, 'c')");
    assert_eq!(
        error(
            &mut db,
            "INSERT OR ROLLBACK INTO t VALUES (4, 'd'), (5, 'a')"
        ),
        "UNIQUE constraint failed: t.name"
    );
    assert!(!db.in_transaction());
    assert_eq!(ids(&mut db), [1, 2]);
    assert_eq!(
        error(&mut db, "COMMIT"),
        "cannot commit - no transaction is active"
    );

    // Outside a transaction it is ABORT: the statement leaves no trace.
    assert!(
        db.execute("INSERT OR ROLLBACK INTO t VALUES (6, 'f'), (7, 'a')")
            .is_err()
    );
    assert_eq!(ids(&mut db), [1, 2]);
    db.integrity_check().unwrap();
}

#[test]
fn autocommit_keeps_what_or_fail_wrote() {
    let mut db = table();
    assert!(
        db.execute("INSERT OR FAIL INTO t VALUES (3, 'c'), (4, 'a')")
            .is_err()
    );
    assert_eq!(ids(&mut db), [1, 2, 3]);
}

fn temp_path(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("fishbowl-{name}-{}.db", std::process::id()));
    remove(&path);
    path
}

fn wal(path: &std::path::Path) -> std::path::PathBuf {
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    wal.into()
}

fn remove(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(wal(path));
}

#[test]
fn a_file_keeps_committed_transactions_and_drops_open_ones() {
    let path = temp_path("txn");
    {
        let mut db = Database::open(&path).unwrap();
        run(
            &mut db,
            "CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT UNIQUE);
             BEGIN; INSERT INTO t VALUES (1, 'a'); COMMIT;
             INSERT INTO t VALUES (2, 'b');
             BEGIN; INSERT INTO t VALUES (3, 'c'); DELETE FROM t WHERE id = 1;",
        );
        // Dropped with the transaction open, as if the process died.
    }
    assert!(std::fs::metadata(wal(&path)).unwrap().len() > 0);
    let mut db = Database::open(&path).unwrap();
    // Creating the file, CREATE TABLE, the BEGIN block and the INSERT.
    assert_eq!(db.recovery().transactions, 4);
    assert_eq!(ids(&mut db), [1, 2]);
    db.integrity_check().unwrap();
    // Recovery copied the log into the file and emptied it.
    assert_eq!(std::fs::metadata(wal(&path)).unwrap().len(), 0);
    drop(db);
    remove(&path);
}

#[test]
fn a_torn_tail_on_the_log_file_is_ignored() {
    let path = temp_path("torn");
    {
        let mut db = Database::open(&path).unwrap();
        run(
            &mut db,
            "CREATE TABLE t(id INTEGER PRIMARY KEY); INSERT INTO t VALUES (1)",
        );
    }
    // Half a frame of garbage, as a crash during a commit can leave.
    let mut log = std::fs::read(wal(&path)).unwrap();
    let before = log.len();
    log.extend(std::iter::repeat_n(0xAB, 2000));
    std::fs::write(wal(&path), &log).unwrap();

    let mut db = Database::open(&path).unwrap();
    assert_eq!(db.recovery().discarded_bytes, (log.len() - before) as u64);
    assert_eq!(ids(&mut db), [1]);
    db.execute("INSERT INTO t VALUES (2)").unwrap();
    db.checkpoint().unwrap();
    drop(db);
    let mut db = Database::open(&path).unwrap();
    assert_eq!(db.recovery().transactions, 0);
    assert_eq!(ids(&mut db), [1, 2]);
    drop(db);
    remove(&path);
}
