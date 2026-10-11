//! M1: sorting, limits, DISTINCT, aggregates and the expressions around them.
//! Expected answers come from SQLite 3.54.

use fishbowl::{Database, Error, StatementResult};

fn setup() -> Database {
    let mut db = Database::new();
    db.execute("CREATE TABLE fish(id INTEGER, name TEXT, species TEXT, weight REAL)")
        .unwrap();
    db.execute(
        "INSERT INTO fish VALUES
           (1, 'goldie', 'goldfish', 0.2), (2, 'bubbles', 'koi', 1.5),
           (3, 'nemo', 'clownfish', NULL), (4, 'dory', 'tang', 0.6),
           (5, 'wanda', 'goldfish', 0.3), (6, 'koko', 'koi', 2.4)",
    )
    .unwrap();
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

#[test]
fn order_by_puts_null_first_and_keeps_ties_in_scan_order() {
    let mut db = setup();
    assert_eq!(
        rows(&mut db, "SELECT name FROM fish ORDER BY weight LIMIT 3"),
        ["nemo", "goldie", "wanda"]
    );
    assert_eq!(
        rows(
            &mut db,
            "SELECT name FROM fish ORDER BY weight DESC LIMIT 2"
        ),
        ["koko", "bubbles"]
    );
    assert_eq!(
        rows(&mut db, "SELECT name FROM fish ORDER BY species DESC, 1"),
        ["dory", "bubbles", "koko", "goldie", "wanda", "nemo"]
    );
}

#[test]
fn order_by_reads_positions_and_aliases() {
    let mut db = setup();
    // An alias wins over a table column with the same name.
    assert_eq!(
        rows(&mut db, "SELECT name AS id FROM fish ORDER BY id LIMIT 2"),
        ["bubbles", "dory"]
    );
    assert_eq!(
        rows(&mut db, "SELECT id, -id FROM fish ORDER BY 2 LIMIT 1"),
        ["6|-6"]
    );
    let err = db.execute("SELECT id FROM fish ORDER BY 2").unwrap_err();
    assert_eq!(
        err,
        Error::Parse("1st ORDER BY term out of range - should be between 1 and 1".into())
    );
}

#[test]
fn limit_and_offset_follow_sqlite() {
    let mut db = setup();
    let ids = |db: &mut Database, tail: &str| rows(db, &format!("SELECT id FROM fish {tail}"));
    assert_eq!(ids(&mut db, "LIMIT 2 OFFSET 3"), ["4", "5"]);
    // `LIMIT offset, count`, and a negative LIMIT means no limit.
    assert_eq!(ids(&mut db, "LIMIT 4, 10"), ["5", "6"]);
    assert_eq!(ids(&mut db, "LIMIT -1 OFFSET 5"), ["6"]);
    assert_eq!(ids(&mut db, "LIMIT '1'"), ["1"]);
    assert_eq!(
        db.execute("SELECT id FROM fish LIMIT 1.5").unwrap_err(),
        Error::Runtime("datatype mismatch".into())
    );
}

#[test]
fn limit_stops_reading_once_it_has_enough_rows() {
    let mut db = setup();
    let plan = query(&mut db, "SELECT name FROM fish LIMIT 2").plan;
    assert_eq!((plan.operator, plan.rows_out), ("Limit", 2));
    let scan = &plan.children[0].children[0];
    assert_eq!((scan.operator, scan.rows_out), ("SeqScan", 2));
}

#[test]
fn distinct_treats_nulls_as_equal() {
    let mut db = setup();
    db.execute("INSERT INTO fish VALUES (7, 'ghost', NULL, NULL), (8, 'shade', NULL, NULL)")
        .unwrap();
    assert_eq!(
        rows(&mut db, "SELECT DISTINCT species FROM fish ORDER BY 1"),
        ["NULL", "clownfish", "goldfish", "koi", "tang"]
    );
}

#[test]
fn group_by_with_having_and_aggregates() {
    let mut db = setup();
    assert_eq!(
        rows(
            &mut db,
            "SELECT species, count(*), sum(weight), avg(id), group_concat(name)
               FROM fish GROUP BY species HAVING count(*) > 1"
        ),
        [
            "goldfish|2|0.5|3.0|goldie,wanda",
            "koi|2|3.9|4.0|bubbles,koko"
        ]
    );
    assert_eq!(
        rows(
            &mut db,
            "SELECT count(*), count(weight), total(id), min(name), max(weight) FROM fish"
        ),
        ["6|5|21.0|bubbles|2.4"]
    );
}

#[test]
fn plain_columns_come_from_the_first_row_or_the_min_max_row() {
    let mut db = setup();
    assert_eq!(
        rows(&mut db, "SELECT species, name FROM fish GROUP BY species"),
        [
            "clownfish|nemo",
            "goldfish|goldie",
            "koi|bubbles",
            "tang|dory"
        ]
    );
    assert_eq!(
        rows(&mut db, "SELECT name, max(weight) FROM fish"),
        ["koko|2.4"]
    );
}

#[test]
fn aggregates_over_no_rows() {
    let mut db = setup();
    assert_eq!(
        rows(
            &mut db,
            "SELECT count(*), sum(id), total(id), avg(id), max(id) FROM fish WHERE 0"
        ),
        ["0|NULL|0.0|NULL|NULL"]
    );
    assert!(
        rows(
            &mut db,
            "SELECT count(*) FROM fish WHERE 0 GROUP BY species"
        )
        .is_empty()
    );
}

#[test]
fn group_by_sorts_in_the_order_by_direction() {
    let mut db = setup();
    // Every species ties on count(*) = 1 here. SQLite sorts the groups in the
    // ORDER BY's direction first, so the tie keeps that order.
    let sql = |dir: &str| {
        format!(
            "SELECT species FROM fish WHERE id IN (1, 2, 3, 4)
               GROUP BY species ORDER BY count(*) {dir}"
        )
    };
    assert_eq!(
        rows(&mut db, &sql("DESC")),
        ["tang", "koi", "goldfish", "clownfish"]
    );
    assert_eq!(
        rows(&mut db, &sql("ASC")),
        ["clownfish", "goldfish", "koi", "tang"]
    );
}

#[test]
fn comparisons_apply_affinity() {
    let mut db = Database::new();
    db.execute("CREATE TABLE t(i INTEGER, s TEXT, u)").unwrap();
    db.execute("INSERT INTO t VALUES (5, '5', 5)").unwrap();
    assert_eq!(
        rows(
            &mut db,
            "SELECT i = '5', +i = '5', s = 5, s = u, u = '5', i = s, s IN (5), 5 IN (s) FROM t"
        ),
        ["1|0|1|0|0|1|1|0"]
    );
}

#[test]
fn case_between_in_cast_and_functions() {
    let mut db = Database::new();
    assert_eq!(
        rows(
            &mut db,
            "SELECT CASE 2 WHEN 1 THEN 'one' WHEN 2 THEN 'two' END,
                    CASE WHEN NULL THEN 1 ELSE 0 END,
                    3 BETWEEN 1 AND 5, NULL IN (), 2 IN (NULL, 1),
                    CAST('12abc' AS INTEGER), CAST('1e3' AS NUMERIC), CAST(2.5 AS TEXT)"
        ),
        ["two|0|1|0|NULL|12|1000|2.5"]
    );
    assert_eq!(
        rows(
            &mut db,
            "SELECT coalesce(NULL, 2), nullif(1, 1), abs('-5'), round(2.5),
                    substr('fishbowl', -4), upper('koi'), length(1.50), typeof(1 / 2)"
        ),
        ["2|NULL|5.0|3.0|bowl|KOI|3|integer"]
    );
    assert_eq!(
        db.execute("SELECT abs(-9223372036854775808)").unwrap_err(),
        Error::Runtime("integer overflow".into())
    );
}

#[test]
fn bit_operators_match_sqlite() {
    let mut db = Database::new();
    // Shifts past the top, negative shifts, shifts of 64 or more, and
    // operands read as integers the way SQLite reads them.
    assert_eq!(
        rows(
            &mut db,
            "SELECT 1<<63, 1<<64, 1<<-1, 8>>-2, -8>>1, -8>>64, -8>>63, -1>>100,
                    5<<-100, -5<<-100, 3<<62, 2.9<<1, -2.9<<1, '12abc'<<1, 'abc'>>1,
                    '1e3'|0, 1e30|0, -1e30|0, NULL<<1, 1<<NULL, 6&3, 6|3, ~5, ~-1, ~NULL,
                    ~2.5, ~'7x', '9999999999999999999999'|0, typeof(2.0|0)"
        ),
        [
            "-9223372036854775808|0|0|32|-4|-1|-1|-1|0|-1|-4611686018427387904|4|-4|24|0|1|\
          9223372036854775807|-9223372036854775808|NULL|NULL|2|7|-6|0|NULL|-3|-8|\
          9223372036854775807|integer"
        ]
    );
    // One shared precedence for & | << >>, read left to right, between + and
    // the comparisons. ~ binds tighter than everything else.
    assert_eq!(
        rows(
            &mut db,
            "SELECT 4|6&3, 6&3|4, ~1*2, 1+2<<1, 1<<2+1, 1<2<<1, 5&1=1, 1 || 2 << 1,
                    ~1 || 'x', 1<<63>>63, 2 * 3 & 5"
        ),
        ["2|6|-4|6|8|1|1|24|-2x|-1|4"]
    );
    assert_eq!(
        db.execute("SELECT 1 ^ 2").unwrap_err(),
        Error::Parse("unrecognized token: \"^\"".into())
    );
}

#[test]
fn plan_shows_every_step_in_order() {
    let mut db = setup();
    let plan = query(
        &mut db,
        "SELECT DISTINCT species, count(*) FROM fish WHERE id > 1
           GROUP BY species HAVING count(*) > 0 ORDER BY 2 DESC LIMIT 2",
    )
    .plan;
    let mut steps = Vec::new();
    let mut node = Some(&plan);
    while let Some(n) = node {
        steps.push((n.operator, n.rows_out));
        node = n.children.first();
    }
    assert_eq!(
        steps,
        [
            ("Limit", 2),
            ("Distinct", 2),
            ("Project", 2),
            ("Sort", 2),
            ("Filter", 4),
            ("Aggregate", 4),
            ("Filter", 5),
            ("SeqScan", 6),
        ]
    );
}

#[test]
fn misused_clauses_are_rejected_like_sqlite() {
    let mut db = setup();
    for (sql, message) in [
        (
            "SELECT id FROM fish WHERE count(*) > 1",
            "misuse of aggregate function count()",
        ),
        (
            "SELECT id FROM fish HAVING id > 1",
            "HAVING clause on a non-aggregate query",
        ),
        (
            "SELECT sum(sum(id)) FROM fish",
            "misuse of aggregate function sum()",
        ),
        (
            "SELECT count(*) FROM fish GROUP BY 1",
            "aggregate functions are not allowed in the GROUP BY clause",
        ),
        (
            "SELECT coalesce(id) FROM fish",
            "wrong number of arguments to function coalesce()",
        ),
    ] {
        assert_eq!(db.execute(sql).unwrap_err(), Error::Parse(message.into()));
    }
    assert!(matches!(
        db.execute("SELECT nosuch(id) FROM fish"),
        Err(Error::Catalog(_))
    ));
    // SQLite has these, Fishbowl does not yet.
    assert!(matches!(
        db.execute("SELECT DISTINCT species FROM fish ORDER BY weight"),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        db.execute("SELECT hex(id) FROM fish"),
        Err(Error::Unsupported(_))
    ));
}
