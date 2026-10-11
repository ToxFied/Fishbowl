//! The first end-to-end slice: CREATE TABLE, INSERT, SELECT ... WHERE.

use fishbowl::{Database, Error, StatementResult, Value};

fn query(db: &mut Database, sql: &str) -> fishbowl::QueryResult {
    match db.execute(sql).expect("query should run") {
        StatementResult::Query(result) => result,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn setup() -> Database {
    let mut db = Database::new();
    db.execute("CREATE TABLE fish(id INTEGER, name TEXT, weight REAL)")
        .unwrap();
    db.execute(
        "INSERT INTO fish VALUES (1, 'goldie', 0.2), (2, 'bubbles', 1.5), (3, 'nemo', NULL)",
    )
    .unwrap();
    db
}

#[test]
fn select_with_where_filters_rows_and_reports_the_plan() {
    let mut db = setup();
    let result = query(
        &mut db,
        "SELECT name, weight * 2 AS double FROM fish WHERE id >= 2",
    );

    assert_eq!(result.columns, ["name", "double"]);
    assert_eq!(
        result.rows,
        [
            vec![Value::Text("bubbles".into()), Value::Real(3.0)],
            vec![Value::Text("nemo".into()), Value::Null],
        ]
    );
    let plan = &result.plan;
    assert_eq!(plan.operator, "Project");
    assert_eq!(plan.children[0].operator, "Filter");
    assert_eq!(plan.children[0].detail, "(id >= 2)");
    let scan = &plan.children[0].children[0];
    assert_eq!((scan.operator, scan.rows_out), ("SeqScan", 3));
    assert_eq!(plan.children[0].rows_out, 2);
}

#[test]
fn where_treats_null_as_not_true() {
    let mut db = setup();
    let result = query(
        &mut db,
        "SELECT id FROM fish WHERE weight > 0 OR weight IS NULL AND id = 3",
    );
    assert_eq!(result.rows.len(), 3);
    let result = query(&mut db, "SELECT id FROM fish WHERE weight > 1");
    assert_eq!(result.rows, [vec![Value::Integer(2)]]);
}

#[test]
fn insert_applies_column_affinity_and_fills_missing_columns_with_null() {
    let mut db = setup();
    db.execute("INSERT INTO fish(name, id) VALUES ('dory', '4')")
        .unwrap();
    let result = query(&mut db, "SELECT * FROM fish WHERE name = 'dory'");
    assert_eq!(
        result.rows,
        [vec![
            Value::Integer(4),
            Value::Text("dory".into()),
            Value::Null
        ]]
    );
}

#[test]
fn failed_insert_changes_nothing() {
    let mut db = Database::new();
    db.execute("CREATE TABLE t(a INTEGER NOT NULL)").unwrap();
    let err = db.execute("INSERT INTO t VALUES (1), (NULL)").unwrap_err();
    assert!(matches!(err, Error::Constraint(_)));
    assert_eq!(query(&mut db, "SELECT a FROM t").rows.len(), 0);
}

#[test]
fn unsupported_sql_is_rejected_not_ignored() {
    let mut db = setup();
    for sql in [
        "SELECT fish.id FROM fish RIGHT JOIN fish AS other ON 1",
        "SELECT printf('%d', id) FROM fish",
        "CREATE TABLE p(a INTEGER PRIMARY KEY AUTOINCREMENT)",
    ] {
        assert!(
            matches!(db.execute(sql), Err(Error::Unsupported(_))),
            "{sql} should be unsupported"
        );
    }
}

#[test]
fn unknown_names_are_errors() {
    let mut db = setup();
    assert!(matches!(
        db.execute("SELECT nope FROM fish"),
        Err(Error::Catalog(_))
    ));
    assert!(matches!(
        db.execute("SELECT * FROM nope"),
        Err(Error::Catalog(_))
    ));
    assert!(matches!(
        db.execute("CREATE TABLE fish(a)"),
        Err(Error::Catalog(_))
    ));
}

#[test]
fn row_budget_stops_runaway_statements() {
    let mut db = setup();
    db.set_row_budget(Some(2));
    assert!(matches!(
        db.execute("SELECT id FROM fish"),
        Err(Error::BudgetExceeded(2))
    ));
}

#[test]
fn scripts_stop_at_the_first_error() {
    let mut db = Database::new();
    let entries = db.run_script("CREATE TABLE a(x); SELECT y FROM a; CREATE TABLE b(x)");
    assert_eq!(entries.len(), 2);
    assert!(entries[1].result.is_err());
    assert_eq!(db.tables().len(), 1);
}

/// The parser's nesting limit must cover SQLite's test suite (its deepest
/// WHERE clause needs 65), and planning and running a query at the limit must
/// fit in a 1 MiB stack, the WebAssembly default. Debug builds use bigger
/// frames, so they get more room.
#[test]
fn nesting_up_to_the_parser_limit_runs_in_a_small_stack() {
    let stack = if cfg!(debug_assertions) {
        4 << 20
    } else {
        1 << 20
    };
    let shapes: [fn(usize) -> String; 3] = [
        |n| format!("SELECT {}id{} FROM fish", "(".repeat(n), ")".repeat(n)),
        |n| format!("SELECT {}id{} FROM fish", "abs(".repeat(n), ")".repeat(n)),
        |n| {
            let open = "id IN (SELECT id FROM fish WHERE ".repeat(n);
            format!("SELECT name FROM fish WHERE {open}1{}", ")".repeat(n))
        },
    ];
    std::thread::Builder::new()
        .stack_size(stack)
        .spawn(move || {
            for shape in shapes {
                let mut db = setup();
                let deepest = (1..)
                    .take_while(|&n| !matches!(db.execute(&shape(n)), Err(Error::Parse(_))))
                    .last()
                    .unwrap();
                // Nested subqueries count twice: once as a query, once as an expression.
                assert!(deepest >= 45, "{}", shape(deepest));
                let message = db.execute(&shape(deepest + 1)).unwrap_err().to_string();
                assert!(message.contains("recursion limit exceeded"), "{message}");
            }
        })
        .unwrap()
        .join()
        .unwrap();
}
