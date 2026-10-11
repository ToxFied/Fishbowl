//! Differential test: random scripts of writes, indexed queries, joins,
//! subqueries and compound SELECTs run through Fishbowl and through the real
//! `sqlite3`, which must agree on which statements fail and on every query's
//! rows. Each query either orders by all its columns or is a UNION,
//! INTERSECT or EXCEPT, whose rows come out sorted, so the two can be
//! compared row for row.
//!
//! This is where the finer points of SQLite's behavior were found: `OR FAIL`
//! keeps earlier rows only when a constraint breaks, and the order an UPDATE
//! visits rows decides whether a UNIQUE key clashes along the way. Scripts
//! also open, commit and roll back transactions, including by `OR
//! ROLLBACK`, and statements fail inside them.
//!
//! The test is skipped when `sqlite3` is not installed. Run more cases with
//! `PROPTEST_CASES=5000 cargo test --test sqlite_differential`.

use std::io::Write as _;
use std::process::{Command, Stdio};

use fishbowl::{Database, StatementResult, Value};
use proptest::prelude::*;

/// A value the way `sqlite3` prints it in `.mode quote`.
fn quote(value: &Value) -> String {
    match value {
        Value::Null => "NULL".into(),
        Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
        other => other.to_string(),
    }
}

/// What one statement did: `Err` if it failed, else its rows.
type Outcome = Result<Vec<String>, ()>;

fn sqlite_available() -> bool {
    Command::new("sqlite3").arg("-version").output().is_ok()
}

fn run_sqlite(statements: &[String]) -> Vec<Outcome> {
    // A marker row before each statement splits the output per statement.
    let mut script = String::from(".mode quote\n");
    for (i, sql) in statements.iter().enumerate() {
        script.push_str(&format!("SELECT '#{i}';\n{sql};\n"));
    }
    let mut child = Command::new("sqlite3")
        .arg(":memory:")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("sqlite3 runs");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();

    let mut outcomes: Vec<Outcome> = vec![Ok(Vec::new()); statements.len()];
    let mut current = None;
    for line in stdout.lines() {
        match line.strip_prefix("'#").and_then(|l| l.strip_suffix('\'')) {
            Some(n) => current = Some(n.parse::<usize>().unwrap()),
            None => outcomes[current.unwrap()]
                .as_mut()
                .unwrap()
                .push(line.into()),
        }
    }
    // "Error near line N": statement i is on line 3 + 2i.
    for line in stderr.lines() {
        let n = line
            .split("near line ")
            .nth(1)
            .expect("an sqlite3 error line");
        let n: usize = n.split(':').next().unwrap().parse().unwrap();
        outcomes[(n - 3) / 2] = Err(());
    }
    outcomes
}

fn run_fishbowl(statements: &[String]) -> Vec<Outcome> {
    let mut db = Database::new();
    let outcomes = statements
        .iter()
        .map(|sql| match db.execute(sql) {
            Ok(StatementResult::Query(result)) => Ok(result
                .rows
                .iter()
                .map(|row| row.iter().map(quote).collect::<Vec<_>>().join(","))
                .collect()),
            Ok(_) => Ok(Vec::new()),
            Err(_) => Err(()),
        })
        .collect();
    db.integrity_check().unwrap();
    outcomes
}

fn value() -> impl Strategy<Value = String> {
    prop::sample::select(vec![
        "NULL", "0", "1", "2", "3", "'1'", "'2'", "2.0", "1.5", "'a'", "'b'",
    ])
    .prop_map(str::to_string)
}

fn column() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec!["id", "u", "v", "w"])
}

fn filter() -> impl Strategy<Value = String> {
    let op = prop::sample::select(vec!["=", "<", ">=", "<>"]);
    (column(), op, value()).prop_map(|(c, op, v)| format!("{c} {op} {v}"))
}

/// A filter on the columns of `t` under the alias `table`.
fn filter_on(table: &'static str) -> impl Strategy<Value = String> {
    filter().prop_map(move |f| format!("{table}.{f}"))
}

/// Queries that join `t` to itself, nest subqueries or combine SELECTs.
fn m2_query() -> impl Strategy<Value = String> {
    let set_op = prop::sample::select(vec!["UNION", "UNION ALL", "INTERSECT", "EXCEPT"]);
    prop_oneof![
        (column(), column(), filter_on("a")).prop_map(|(x, y, f)| format!(
            "SELECT a.id, b.id FROM t AS a JOIN t AS b ON a.{x} = b.{y} WHERE {f} ORDER BY 1, 2"
        )),
        (column(), column(), filter_on("b")).prop_map(|(x, y, f)| format!(
            "SELECT a.id, b.id FROM t AS a LEFT JOIN t AS b ON a.{x} = b.{y} AND {f} \
             ORDER BY 1, 2"
        )),
        (column(), column(), filter()).prop_map(|(x, y, f)| format!(
            "SELECT id FROM t WHERE {x} IN (SELECT {y} FROM t WHERE {f}) ORDER BY 1"
        )),
        (column(), column()).prop_map(|(x, y)| format!(
            "SELECT id, (SELECT count(*) FROM t AS i WHERE i.{x} = o.{y}) FROM t AS o \
             ORDER BY 1, 2"
        )),
        (column(), column()).prop_map(|(x, y)| format!(
            "SELECT id FROM t AS o WHERE NOT EXISTS (SELECT 1 FROM t AS i WHERE i.{x} > o.{y}) \
             ORDER BY 1"
        )),
        (column(), filter(), set_op, column(), filter()).prop_map(|(x, f, op, y, g)| {
            // UNION ALL keeps scan order, which may differ; sort it.
            let order = if op == "UNION ALL" { " ORDER BY 1" } else { "" };
            format!("SELECT {x} FROM t WHERE {f} {op} SELECT {y} FROM t WHERE {g}{order}")
        }),
        (column(), filter()).prop_map(|(x, f)| format!(
            "SELECT * FROM (SELECT {x} AS k, count(*) AS n FROM t WHERE {f} GROUP BY 1) \
             WHERE n > 0 ORDER BY 1, 2"
        )),
        Just("SELECT * FROM v ORDER BY 1, 2".to_string()),
    ]
}

fn statement() -> impl Strategy<Value = String> {
    let row =
        (value(), value(), value(), value()).prop_map(|(a, b, c, d)| format!("({a},{b},{c},{d})"));
    let rule = prop::sample::select(vec![
        "",
        "OR REPLACE ",
        "OR IGNORE ",
        "OR FAIL ",
        "OR ABORT ",
        "OR ROLLBACK ",
    ]);
    let new_value = prop_oneof![
        value(),
        Just("u + 1".to_string()),
        Just("v || 'x'".to_string()),
        Just("id - 1".to_string()),
    ];
    prop_oneof![
        4 => (rule, prop::collection::vec(row, 1..4))
            .prop_map(|(rule, rows)| format!("INSERT {rule}INTO t VALUES {}", rows.join(","))),
        2 => (column(), new_value, filter())
            .prop_map(|(c, v, f)| format!("UPDATE t SET {c} = {v} WHERE {f}")),
        1 => filter().prop_map(|f| format!("DELETE FROM t WHERE {f}")),
        1 => (column(), any::<bool>()).prop_map(|(c, unique)| {
            let unique = if unique { "UNIQUE " } else { "" };
            format!("CREATE {unique}INDEX IF NOT EXISTS i_{c} ON t({c})")
        }),
        1 => column().prop_map(|c| format!("DROP INDEX IF EXISTS i_{c}")),
        3 => (filter(), filter())
            .prop_map(|(a, b)| format!("SELECT * FROM t WHERE {a} AND {b} ORDER BY id, u, v, w")),
        3 => m2_query(),
        2 => prop::sample::select(vec!["BEGIN", "COMMIT", "ROLLBACK"]).prop_map(str::to_string),
    ]
}

/// SQLite keeps an undo journal for a statement only when it expects the
/// statement could fail halfway, and it does not expect that of `INSERT OR
/// REPLACE`, `OR IGNORE`, `OR FAIL` or `OR ROLLBACK`. When such a statement fails anyway, on
/// an `INTEGER PRIMARY KEY` value that is not an integer, SQLite inside a
/// transaction keeps the rows it already wrote. Fishbowl undoes them, as it
/// does for every failed statement. This replaces those values with NULL
/// while a transaction may be open, so the scripts avoid that one case.
fn avoid_unjournaled_failures(statements: &mut [String]) {
    let mut maybe_open = false;
    for sql in statements {
        match sql.as_str() {
            "BEGIN" => maybe_open = true,
            "COMMIT" | "ROLLBACK" => maybe_open = false,
            _ => {}
        }
        let unjournaled = ["OR REPLACE ", "OR IGNORE ", "OR FAIL ", "OR ROLLBACK "]
            .iter()
            .any(|rule| sql.starts_with(&format!("INSERT {rule}")));
        if maybe_open && unjournaled {
            for bad in ["(1.5,", "('a',", "('b',"] {
                *sql = sql.replace(bad, "(NULL,");
            }
        }
    }
}

const SCHEMAS: [&str; 3] = [
    "CREATE TABLE t(id INTEGER PRIMARY KEY, u INTEGER UNIQUE, v, w TEXT, UNIQUE (v, w))",
    "CREATE TABLE t(id INT PRIMARY KEY, u INTEGER, v REAL UNIQUE, w TEXT NOT NULL)",
    "CREATE TABLE t(id INTEGER, u INTEGER, v, w TEXT, PRIMARY KEY (w, u))",
];

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn fishbowl_agrees_with_sqlite(
        schema in prop::sample::select(SCHEMAS.to_vec()),
        body in prop::collection::vec(statement(), 1..25),
    ) {
        if !sqlite_available() {
            eprintln!("sqlite3 is not installed; skipping the differential test");
            return Ok(());
        }
        let mut statements = vec![
            schema.to_string(),
            "CREATE VIEW v AS SELECT a.u, b.w FROM t AS a, t AS b WHERE a.id = b.u".to_string(),
        ];
        statements.extend(body);
        avoid_unjournaled_failures(&mut statements);
        statements.push("SELECT * FROM t ORDER BY id, u, v, w".into());
        prop_assert_eq!(run_fishbowl(&statements), run_sqlite(&statements), "{:#?}", statements);
    }
}

/// When `2` and `2.0` fall in one group, one DISTINCT set or one side of a
/// UNION, SQLite shows the one it reads first. So Fishbowl has to read rows
/// in SQLite's order: through a covering index, or through an index whose
/// order saves SQLite a sort for GROUP BY, DISTINCT or a UNION. These are
/// the cases the random test found, one per kind of plan.
#[test]
fn equal_values_show_the_one_sqlite_reads_first() {
    if !sqlite_available() {
        return;
    }
    let cases: &[(&str, &[&str])] = &[
        // A covering index on (v, w) puts w = NULL first.
        (
            SCHEMAS[0],
            &[
                "INSERT INTO t VALUES (NULL,1,2.0,2),(NULL,NULL,2,NULL)",
                "SELECT id FROM t WHERE id = NULL UNION SELECT v FROM t WHERE id <> 3",
                "SELECT DISTINCT v FROM t",
            ],
        ),
        // A covering index, read whole although a WHERE term reads w.
        (
            SCHEMAS[0],
            &[
                "INSERT INTO t VALUES (NULL,NULL,2.0,3),(NULL,NULL,2,2.0)",
                "SELECT v FROM t WHERE w >= 0 UNION SELECT id FROM t WHERE id = NULL",
            ],
        ),
        // GROUP BY v reads the (v, w) index rather than the range on u.
        (
            SCHEMAS[0],
            &[
                "INSERT INTO t VALUES (0,3,2.0,NULL),(1,2,2,NULL)",
                "SELECT v AS k, count(*) AS n FROM t WHERE u >= '1' GROUP BY 1",
            ],
        ),
        // A UNION side reads index i_v rather than the range on w.
        (
            SCHEMAS[2],
            &[
                "CREATE INDEX i_v ON t(v)",
                "INSERT INTO t VALUES (NULL,NULL,2,2),(NULL,NULL,2.0,0)",
                "SELECT v FROM t WHERE w < 'a' UNION SELECT id FROM t WHERE id = NULL",
            ],
        ),
    ];
    for (schema, body) in cases {
        let statements: Vec<String> = std::iter::once(*schema)
            .chain(body.iter().copied())
            .map(str::to_string)
            .collect();
        assert_eq!(
            run_fishbowl(&statements),
            run_sqlite(&statements),
            "{statements:#?}"
        );
    }
}
