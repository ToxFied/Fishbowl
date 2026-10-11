//! Differential test: random scripts of writes and indexed queries run
//! through Fishbowl and through the real `sqlite3`, which must agree on
//! which statements fail and on every query's rows.
//!
//! This is where the finer points of SQLite's behavior were found: `OR FAIL`
//! keeps earlier rows only when a constraint breaks, and the order an UPDATE
//! visits rows decides whether a UNIQUE key clashes along the way.
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

fn statement() -> impl Strategy<Value = String> {
    let row =
        (value(), value(), value(), value()).prop_map(|(a, b, c, d)| format!("({a},{b},{c},{d})"));
    let rule = prop::sample::select(vec![
        "",
        "OR REPLACE ",
        "OR IGNORE ",
        "OR FAIL ",
        "OR ABORT ",
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
    ]
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
        let mut statements = vec![schema.to_string()];
        statements.extend(body);
        statements.push("SELECT * FROM t ORDER BY id, u, v, w".into());
        prop_assert_eq!(run_fishbowl(&statements), run_sqlite(&statements), "{:#?}", statements);
    }
}
