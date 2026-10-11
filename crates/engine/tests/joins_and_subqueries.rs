//! M2: joins, subqueries, compound SELECTs and views.
//! Expected answers come from SQLite 3.54.

use fishbowl::{Database, Error, PlanProfile, StatementResult};

fn setup() -> Database {
    let mut db = Database::new();
    for sql in [
        "CREATE TABLE t1(a INTEGER, b TEXT)",
        "CREATE TABLE t2(a INTEGER, c)",
        "CREATE TABLE t3(x TEXT, y INTEGER)",
        "INSERT INTO t1 VALUES (3, 'x'), (1, 'y'), (2, NULL)",
        "INSERT INTO t2 VALUES (2, 'p'), (3, 'q'), (3, 'r')",
        "INSERT INTO t3 VALUES ('1', 1), ('2', 2)",
    ] {
        db.execute(sql).unwrap();
    }
    db
}

fn query(db: &mut Database, sql: &str) -> fishbowl::QueryResult {
    match db.execute(sql) {
        Ok(StatementResult::Query(result)) => result,
        other => panic!("{sql}: expected rows, got {other:?}"),
    }
}

/// Each row as its values joined by `|`, like the sqlite3 shell prints them.
fn rows(db: &mut Database, sql: &str) -> Vec<String> {
    query(db, sql)
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

fn error(db: &mut Database, sql: &str) -> String {
    match db.execute(sql) {
        Err(e) => e.to_string(),
        Ok(result) => panic!("{sql}: expected an error, got {result:?}"),
    }
}

/// The plan as `operator detail [rows]` lines, indented by depth.
fn plan_lines(plan: &PlanProfile) -> Vec<String> {
    fn walk(node: &PlanProfile, depth: usize, out: &mut Vec<String>) {
        let detail = if node.detail.is_empty() {
            String::new()
        } else {
            format!(" {}", node.detail)
        };
        out.push(format!(
            "{}{}{detail} [{}]",
            "  ".repeat(depth),
            node.operator,
            node.rows_out
        ));
        for child in &node.children {
            walk(child, depth + 1, out);
        }
    }
    let mut out = Vec::new();
    walk(plan, 0, &mut out);
    out
}

#[test]
fn joins_pair_rows_in_from_order() {
    let mut db = setup();
    assert_eq!(
        rows(&mut db, "SELECT t1.a, c FROM t1, t2 WHERE t1.a = t2.a"),
        ["3|q", "3|r", "2|p"]
    );
    assert_eq!(
        rows(
            &mut db,
            "SELECT b, c FROM t1 JOIN t2 ON t1.a = t2.a AND c <> 'q' ORDER BY c"
        ),
        ["NULL|p", "x|r"]
    );
    assert_eq!(
        rows(&mut db, "SELECT count(*) FROM t1 CROSS JOIN t2"),
        ["9"]
    );
    assert_eq!(
        rows(&mut db, "SELECT count(*) FROM ( t1 AS x CROSS JOIN t1 y )"),
        ["9"]
    );
    // An inner join's ON may name a table further right, like WHERE.
    assert_eq!(
        rows(
            &mut db,
            "SELECT b, c, x FROM t1 JOIN t2 ON t3.y = 1, t3 WHERE t1.a = 1"
        ),
        ["y|p|1", "y|q|1", "y|r|1"]
    );
}

#[test]
fn left_join_pads_rows_without_a_match_with_null() {
    let mut db = setup();
    assert_eq!(
        rows(&mut db, "SELECT * FROM t1 LEFT JOIN t2 ON t1.a = t2.a"),
        ["3|x|3|q", "3|x|3|r", "1|y|NULL|NULL", "2|NULL|2|p"]
    );
    // WHERE runs after the join, so it sees the NULLs.
    assert_eq!(
        rows(
            &mut db,
            "SELECT t1.a FROM t1 LEFT JOIN t2 ON t1.a = t2.a WHERE t2.c IS NULL"
        ),
        ["1"]
    );
    // ON decides which rows match; the left rows all stay.
    assert_eq!(
        rows(
            &mut db,
            "SELECT t1.a, c FROM t1 LEFT JOIN t2 ON t1.a = t2.a AND c = 'r'"
        ),
        ["3|r", "1|NULL", "2|NULL"]
    );
    assert_eq!(
        error(&mut db, "SELECT * FROM t1 LEFT JOIN t2 ON t3.y = 1, t3"),
        "parse error: ON clause references tables to its right"
    );
}

#[test]
fn names_must_say_which_table_they_mean() {
    let mut db = setup();
    assert_eq!(
        error(&mut db, "SELECT a FROM t1, t2"),
        "ambiguous column name: a"
    );
    assert_eq!(
        error(&mut db, "SELECT t1.a FROM t1 AS x"),
        "no such column: t1.a"
    );
    assert_eq!(
        query(&mut db, "SELECT * FROM t1, t3").columns,
        ["a", "b", "x", "y"]
    );
    assert_eq!(
        rows(&mut db, "SELECT t3.* FROM t1, t3 WHERE a = y"),
        ["1|1", "2|2"]
    );
}

#[test]
fn conditions_move_down_to_the_table_they_read() {
    let mut db = setup();
    let result = query(
        &mut db,
        "SELECT t1.a FROM t1, t2 WHERE t1.a = t2.a AND c = 'q' AND b IS NOT NULL",
    );
    // t2 is read once; Materialize hands its one matching row to each t1 row.
    assert_eq!(
        plan_lines(&result.plan),
        [
            "Project t1.a [1]",
            "  NestedLoopJoin INNER ON (t1.a = t2.a) [1]",
            "    Filter t1.b IS NOT NULL [2]",
            "      SeqScan t1 [3]",
            "    Materialize [2]",
            "      Filter (t2.c = 'q') [1]",
            "        SeqScan t2 [3]",
        ]
    );
}

#[test]
fn scalar_in_and_exists_subqueries() {
    let mut db = setup();
    assert_eq!(
        rows(
            &mut db,
            "SELECT a FROM t1 WHERE a > (SELECT avg(a) FROM t2)"
        ),
        ["3"]
    );
    assert_eq!(
        rows(
            &mut db,
            "SELECT (SELECT a FROM t1 ORDER BY a), (SELECT a FROM t1 WHERE 0)"
        ),
        ["1|NULL"]
    );
    assert_eq!(
        rows(&mut db, "SELECT b FROM t1 WHERE a IN (SELECT a FROM t2)"),
        ["x", "NULL"]
    );
    // IN follows three-valued logic, and an empty subquery holds nothing.
    assert_eq!(
        rows(
            &mut db,
            "SELECT 1 IN (SELECT NULL), 2 NOT IN (SELECT NULL UNION SELECT 1), \
             NULL IN (SELECT 1 WHERE 0), 1 NOT IN (SELECT 2)"
        ),
        ["NULL|NULL|0|1"]
    );
    // The subquery's column brings its affinity: TEXT turns 1 into '1'.
    assert_eq!(
        rows(&mut db, "SELECT x FROM t3 WHERE x IN (SELECT 1)"),
        ["1"]
    );
    assert_eq!(
        rows(
            &mut db,
            "SELECT y FROM t3 WHERE (SELECT x FROM t3 LIMIT 1) = 1"
        ),
        ["1", "2"]
    );
    assert_eq!(
        error(&mut db, "SELECT (SELECT a, b FROM t1)"),
        "parse error: sub-select returns 2 columns - expected 1"
    );
}

#[test]
fn correlated_subqueries_read_the_outer_row() {
    let mut db = setup();
    assert_eq!(
        rows(
            &mut db,
            "SELECT a, (SELECT count(*) FROM t2 WHERE t2.a = t1.a) FROM t1"
        ),
        ["3|2", "1|0", "2|1"]
    );
    assert_eq!(
        rows(
            &mut db,
            "SELECT a FROM t1 WHERE NOT EXISTS (SELECT * FROM t2 WHERE t2.a = t1.a)"
        ),
        ["1"]
    );
    // Two levels down, reading the outermost query.
    assert_eq!(
        rows(
            &mut db,
            "SELECT a, (SELECT (SELECT t1.a + t2.a) FROM t2 WHERE c = 'p') FROM t1"
        ),
        ["3|5", "1|3", "2|4"]
    );
    assert_eq!(
        rows(
            &mut db,
            "SELECT a FROM t1 GROUP BY a HAVING (SELECT count(*) FROM t2 WHERE t2.a = t1.a) > 0"
        ),
        ["2", "3"]
    );
}

#[test]
fn a_correlated_subquery_runs_per_row_and_another_runs_once() {
    let mut db = setup();
    let result = query(
        &mut db,
        "SELECT a FROM t1 WHERE EXISTS (SELECT 1 FROM t2 WHERE t2.a = t1.a) \
         AND a < (SELECT max(a) FROM t2) + 1",
    );
    let filter = &result.plan.children[0];
    let subqueries: Vec<(&str, u64)> = filter.children[1..]
        .iter()
        .map(|s| (s.detail.as_str(), s.rows_out))
        .collect();
    assert_eq!(subqueries, [("EXISTS, correlated", 3), ("scalar", 1)]);
}

#[test]
fn compound_selects_work_left_to_right() {
    let mut db = setup();
    // INTERSECT does not bind tighter than UNION in SQLite.
    assert_eq!(
        rows(&mut db, "SELECT 1 UNION SELECT 2 INTERSECT SELECT 2"),
        ["2"]
    );
    // UNION sorts and removes duplicates; UNION ALL then appends.
    assert_eq!(
        rows(
            &mut db,
            "SELECT 3 UNION SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 1"
        ),
        ["1", "3", "2", "1"]
    );
    assert_eq!(
        rows(&mut db, "SELECT 3 UNION ALL SELECT 1 EXCEPT SELECT 2"),
        ["1", "3"]
    );
    // Equal rows that print differently: within a side the first stays, a
    // UNION's right side replaces the left's, and INTERSECT keeps the left's.
    assert_eq!(rows(&mut db, "SELECT 1 UNION SELECT 1.0"), ["1.0"]);
    assert_eq!(
        rows(
            &mut db,
            "SELECT 1 UNION ALL SELECT 1.0 UNION SELECT 7 WHERE 0"
        ),
        ["1"]
    );
    assert_eq!(rows(&mut db, "SELECT 1 INTERSECT SELECT 1.0"), ["1"]);
}

#[test]
fn compound_order_by_names_a_result_column() {
    let mut db = setup();
    assert_eq!(
        rows(
            &mut db,
            "SELECT a AS z FROM t1 UNION SELECT c FROM t2 ORDER BY c"
        ),
        ["1", "2", "3", "p", "q", "r"]
    );
    assert_eq!(
        rows(
            &mut db,
            "SELECT a FROM t1 UNION SELECT a FROM t2 ORDER BY t1.a DESC"
        ),
        ["3", "2", "1"]
    );
    assert_eq!(
        error(
            &mut db,
            "SELECT a FROM t1 UNION SELECT a FROM t2 ORDER BY a + 1"
        ),
        "parse error: 1st ORDER BY term does not match any column in the result set"
    );
    assert_eq!(
        error(&mut db, "SELECT a FROM t1 UNION SELECT a, c FROM t2"),
        "parse error: SELECTs to the left and right of UNION do not have the same number of result columns"
    );
}

#[test]
fn views_are_named_queries() {
    let mut db = setup();
    db.execute("CREATE VIEW v AS SELECT a, a + 1, b FROM t1")
        .unwrap();
    assert_eq!(
        rows(&mut db, "SELECT * FROM v WHERE a > 1 ORDER BY 1"),
        ["2|3|NULL", "3|4|x"]
    );
    db.execute("CREATE VIEW w(p, q) AS SELECT a, b FROM t1")
        .unwrap();
    assert_eq!(
        rows(
            &mut db,
            "SELECT v.a, w.q FROM v, w WHERE v.a = w.p AND p < 3"
        ),
        ["1|y", "2|NULL"]
    );
    // IF NOT EXISTS keeps the view that is there.
    db.execute("CREATE VIEW IF NOT EXISTS w AS SELECT 1")
        .unwrap();
    assert_eq!(rows(&mut db, "SELECT count(*) FROM w"), ["3"]);
    db.execute("DROP VIEW w").unwrap();
    assert_eq!(error(&mut db, "SELECT * FROM w"), "no such table: w");

    // Like SQLite, a view is checked when it is used.
    db.execute("CREATE VIEW short(p, q) AS SELECT a FROM t1")
        .unwrap();
    assert_eq!(
        error(&mut db, "SELECT * FROM short"),
        "expected 2 columns for 'short' but got 1"
    );
    db.execute("CREATE VIEW c1 AS SELECT * FROM c2").unwrap();
    db.execute("CREATE VIEW c2 AS SELECT * FROM c1").unwrap();
    assert_eq!(
        error(&mut db, "SELECT * FROM c1"),
        "view c1 is circularly defined"
    );
}

#[test]
fn views_and_tables_keep_to_their_own_statements() {
    let mut db = setup();
    db.execute("CREATE VIEW v AS SELECT a FROM t1").unwrap();
    for (sql, message) in [
        ("CREATE TABLE v(x)", "view v already exists"),
        ("CREATE VIEW t1 AS SELECT 1", "table t1 already exists"),
        ("DROP TABLE v", "use DROP VIEW to delete view v"),
        ("DROP VIEW t1", "use DROP TABLE to delete table t1"),
        ("DROP VIEW nope", "no such view: nope"),
        (
            "INSERT INTO v VALUES (1)",
            "cannot modify v because it is a view",
        ),
        ("DELETE FROM v", "cannot modify v because it is a view"),
    ] {
        assert_eq!(error(&mut db, sql), message, "{sql}");
    }
    db.execute("DROP VIEW IF EXISTS nope").unwrap();
}

#[test]
fn subqueries_in_from_name_repeated_columns_apart() {
    let mut db = setup();
    let result = query(&mut db, "SELECT * FROM (SELECT a, a FROM t1)");
    assert_eq!(result.columns, ["a", "a:1"]);
    assert_eq!(
        rows(
            &mut db,
            "SELECT * FROM (SELECT a, count(*) AS n FROM t2 GROUP BY a) WHERE n > 1"
        ),
        ["3|2"]
    );
}

#[test]
fn unsupported_joins_and_subqueries_are_rejected_not_guessed() {
    let mut db = setup();
    for sql in [
        "SELECT * FROM t1 RIGHT JOIN t2 ON t1.a = t2.a",
        "SELECT * FROM t1 JOIN t2 USING (a)",
        "SELECT * FROM t1 NATURAL JOIN t2",
        "SELECT * FROM t3 LEFT JOIN (t1 JOIN t2 ON t1.a = t2.a) ON y = t1.a",
        // SQLite computes this max() over t1, in the outer query.
        "SELECT (SELECT max(t1.a) FROM t2) FROM t1",
        "DELETE FROM t1 WHERE a IN (SELECT a FROM t2)",
    ] {
        assert!(
            matches!(db.execute(sql), Err(Error::Unsupported(_))),
            "{sql} should be unsupported"
        );
    }
}

#[test]
fn views_survive_reopening_the_file_but_temp_views_do_not() {
    let path = std::env::temp_dir().join(format!("fishbowl-views-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::open(&path).unwrap();
        for sql in [
            "CREATE TABLE t(a INTEGER, b TEXT)",
            "INSERT INTO t VALUES (1, 'x'), (2, 'y')",
            "CREATE VIEW kept(p, q) AS SELECT a, b FROM t WHERE a > 1",
            "CREATE TEMP VIEW scratch AS SELECT a FROM t",
            "CREATE VIEW gone AS SELECT 1",
            "DROP VIEW gone",
        ] {
            db.execute(sql).unwrap();
        }
    }
    let mut db = Database::open(&path).unwrap();
    assert_eq!(rows(&mut db, "SELECT q FROM kept"), ["y"]);
    assert_eq!(
        error(&mut db, "SELECT * FROM scratch"),
        "no such table: scratch"
    );
    assert_eq!(error(&mut db, "SELECT * FROM gone"), "no such table: gone");
    drop(db);
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn inner_joins_start_from_a_filtered_table_and_follow_conditions() {
    let mut db = setup();
    let sql = "SELECT * FROM t3, t2, t1 WHERE t2.a = t1.a AND t1.b = 'x' ORDER BY 1, 4";
    let result = query(&mut db, sql);
    // `*` keeps FROM order even though t1 is joined first.
    assert_eq!(result.columns, ["x", "y", "a", "c", "a", "b"]);
    let scans: Vec<String> = plan_lines(&result.plan)
        .into_iter()
        .filter(|line| line.contains("SeqScan"))
        .map(|line| line.trim().to_string())
        .collect();
    assert_eq!(
        scans,
        ["SeqScan t1 [3]", "SeqScan t2 [3]", "SeqScan t3 [2]"]
    );
    assert_eq!(
        rows(&mut db, sql),
        ["1|1|3|q|3|x", "1|1|3|r|3|x", "2|2|3|q|3|x", "2|2|3|r|3|x"]
    );
}
