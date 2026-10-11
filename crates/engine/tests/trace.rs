//! M7: the row-level execution trace and the page inspector.

use fishbowl::trace::{Event, EventKind};
use fishbowl::{Database, PlanProfile, QueryResult, StatementResult, Trace, Value};

const SETUP: &[&str] = &[
    "CREATE TABLE tank(id INTEGER PRIMARY KEY, name TEXT, litres INTEGER)",
    "INSERT INTO tank VALUES (1, 'reef', 300), (2, 'pond', 2000), (3, 'bowl', 4)",
    "CREATE TABLE fish(id INTEGER PRIMARY KEY, name TEXT, species TEXT, weight REAL, tank INTEGER)",
    "INSERT INTO fish VALUES (1, 'goldie', 'goldfish', 0.2, 3), (2, 'bubbles', 'koi', 1.5, 2), \
     (3, 'nemo', 'clownfish', NULL, 1), (4, 'dory', 'tang', 0.6, 1), \
     (5, 'wanda', 'goldfish', 0.3, 3), (6, 'koko', 'koi', 2.4, 2), \
     (7, 'finn', 'betta', 0.1, 3), (8, 'pearl', 'koi', NULL, 2)",
    "CREATE INDEX fish_species ON fish(species)",
    "CREATE VIEW heavy AS SELECT name, weight FROM fish WHERE weight > 1",
];

/// Queries that use every kind of step.
const QUERIES: &[&str] = &[
    "SELECT name, weight * 1000 FROM fish WHERE species = 'koi' AND weight > 1",
    "SELECT * FROM fish",
    "SELECT name FROM fish WHERE id BETWEEN 3 AND 5",
    "SELECT name FROM fish WHERE species IN ('koi', 'tang', 'shark')",
    "SELECT name FROM fish WHERE weight IS NULL OR weight < 0.25",
    "SELECT species, count(*), avg(weight) FROM fish GROUP BY species HAVING count(*) > 1 ORDER BY 2 DESC",
    "SELECT name, weight FROM fish ORDER BY weight DESC LIMIT 3",
    "SELECT name FROM fish ORDER BY id LIMIT 2 OFFSET 3",
    "SELECT DISTINCT species FROM fish",
    "SELECT 7 / 2, 'gold' || 'fish', NULL + 1",
    "SELECT fish.name, tank.name FROM fish JOIN tank ON fish.tank = tank.id",
    "SELECT tank.name, fish.name FROM tank LEFT JOIN fish ON fish.tank = tank.id AND fish.weight > 2",
    "SELECT a.name, b.name FROM fish AS a JOIN fish AS b ON a.species = b.species AND a.id < b.id",
    "SELECT name FROM fish AS f WHERE weight = (SELECT max(weight) FROM fish WHERE species = f.species)",
    "SELECT name FROM fish WHERE weight > (SELECT avg(weight) FROM fish)",
    "SELECT name FROM fish WHERE tank IN (SELECT id FROM tank WHERE litres > 100)",
    "SELECT name FROM tank WHERE EXISTS (SELECT 1 FROM fish WHERE fish.tank = tank.id AND weight > 2)",
    "SELECT name FROM fish WHERE id < 3 UNION SELECT name FROM tank",
    "SELECT species FROM fish UNION ALL SELECT name FROM tank",
    "SELECT species FROM fish EXCEPT SELECT 'koi'",
    "SELECT * FROM heavy",
    "SELECT n FROM (SELECT name AS n, weight FROM fish WHERE weight < 1) WHERE n > 'e'",
];

fn database() -> Database {
    let mut db = Database::new();
    for sql in SETUP {
        db.execute(sql).unwrap();
    }
    db
}

fn query(db: &mut Database, sql: &str) -> QueryResult {
    match db.execute(sql) {
        Ok(StatementResult::Query(result)) => result,
        other => panic!("{sql}: expected rows, got {other:?}"),
    }
}

fn traced(db: &mut Database, sql: &str) -> (QueryResult, Trace) {
    db.set_trace(Some(usize::MAX));
    let mut result = query(db, sql);
    db.set_trace(None);
    let trace = result.trace.take().expect("tracing was on");
    (result, trace)
}

fn nodes(plan: &PlanProfile) -> Vec<&PlanProfile> {
    let mut out = vec![plan];
    for child in &plan.children {
        out.extend(nodes(child));
    }
    out
}

fn events(trace: &Trace, step: usize, kinds: &[EventKind]) -> Vec<Event> {
    trace
        .events
        .iter()
        .filter(|e| e.step == step && kinds.contains(&e.kind))
        .cloned()
        .collect()
}

#[test]
fn tracing_changes_no_result() {
    let mut db = database();
    for sql in QUERIES {
        let plain = query(&mut db, sql);
        assert!(plain.trace.is_none(), "{sql}: traced without asking");
        let (result, _) = traced(&mut db, sql);
        assert_eq!(plain.rows, result.rows, "{sql}");
        assert_eq!(plain.plan.render(), result.plan.render(), "{sql}");
    }
}

#[test]
fn every_step_passes_on_as_many_rows_as_its_count() {
    let mut db = database();
    for sql in QUERIES {
        let (result, trace) = traced(&mut db, sql);
        assert!(!trace.truncated);
        let steps = nodes(&result.plan);
        for node in &steps {
            // A subquery's count is how many times it ran.
            let kind = match node.operator {
                "Subquery" => EventKind::Run,
                _ => EventKind::Out,
            };
            let seen = events(&trace, node.id, &[kind]).len() as u64;
            assert_eq!(
                seen, node.rows_out,
                "{sql}: step {} {}",
                node.id, node.operator
            );
        }
        // The root's rows are the result, in order.
        let out: Vec<_> = events(&trace, result.plan.id, &[EventKind::Out])
            .into_iter()
            .map(|e| e.row)
            .collect();
        assert_eq!(out, result.rows, "{sql}");
        // Ids are the plan's own, numbered in pre-order.
        let ids: Vec<usize> = steps.iter().map(|n| n.id).collect();
        assert_eq!(ids, (0..steps.len()).collect::<Vec<_>>(), "{sql}");
        assert!(trace.events.iter().all(|e| e.step < steps.len()), "{sql}");
    }
}

#[test]
fn a_filter_rules_on_every_row_its_input_passes_on_in_order() {
    let mut db = database();
    for sql in QUERIES {
        let (result, trace) = traced(&mut db, sql);
        for node in nodes(&result.plan) {
            if node.operator != "Filter" {
                continue;
            }
            let input = &node.children[0];
            let read: Vec<_> = events(&trace, input.id, &[EventKind::Out])
                .into_iter()
                .map(|e| e.row)
                .collect();
            let ruled: Vec<_> = events(&trace, node.id, &[EventKind::Out, EventKind::Drop])
                .into_iter()
                .map(|e| e.row)
                .collect();
            assert_eq!(read, ruled, "{sql}");
        }
    }
}

#[test]
fn an_index_scan_reads_only_matching_rows_and_says_which() {
    let mut db = database();
    let (result, trace) = traced(
        &mut db,
        "SELECT name FROM fish WHERE species = 'koi' AND weight > 1",
    );
    let filter = &result.plan.children[0];
    let scan = &filter.children[0];
    assert_eq!(scan.operator, "IndexScan");
    let read: Vec<i64> = events(&trace, scan.id, &[EventKind::Out])
        .iter()
        .map(|e| e.rowid.unwrap())
        .collect();
    assert_eq!(read, [2, 6, 8]);
    // pearl weighs NULL, so the Filter drops her.
    let dropped = events(&trace, filter.id, &[EventKind::Drop]);
    assert_eq!(dropped.len(), 1);
    assert_eq!(dropped[0].row[1], Value::Text("pearl".into()));

    let [scan_rows] = trace.scans.as_slice() else {
        panic!("one scan step");
    };
    assert_eq!(
        (scan_rows.step, scan_rows.table.as_str()),
        (scan.id, "fish")
    );
    assert_eq!(scan_rows.row_count, 8);
    assert_eq!(scan_rows.rowids, (1..=8).collect::<Vec<_>>());
    assert_eq!(scan_rows.rows.len(), 8);
    assert_eq!(scan_rows.rows[5][1], Value::Text("koko".into()));

    // A big table is listed by rowid only.
    db.execute("CREATE TABLE big(x)").unwrap();
    db.execute("INSERT INTO big SELECT a.id FROM fish a, fish b")
        .unwrap();
    let (_, trace) = traced(&mut db, "SELECT x FROM big");
    assert_eq!(trace.scans[0].rowids.len(), 64);
    assert!(trace.scans[0].rows.is_empty());
}

#[test]
fn a_join_tries_every_pair_and_drops_those_that_do_not_match() {
    let mut db = database();
    // `+tank.id` is not a plain column, so no index can find the tank: the
    // join tries every pair, as SQLite would.
    let (result, trace) = traced(
        &mut db,
        "SELECT fish.name, tank.name FROM fish JOIN tank ON fish.tank = +tank.id",
    );
    let join = &result.plan.children[0];
    assert_eq!(join.operator, "NestedLoopJoin");
    let kept = events(&trace, join.id, &[EventKind::Out]).len();
    let dropped = events(&trace, join.id, &[EventKind::Drop]).len();
    // 8 fish, each tried against 3 tanks, each in exactly one.
    assert_eq!((kept, dropped), (8, 16));
    // The tanks are read from the table once and replayed from memory.
    let materialize = &join.children[1];
    assert_eq!(materialize.operator, "Materialize");
    assert_eq!(
        events(&trace, materialize.children[0].id, &[EventKind::Out]).len(),
        3
    );
    assert_eq!(events(&trace, materialize.id, &[EventKind::Out]).len(), 24);
}

#[test]
fn a_join_looks_up_matching_rows_by_index() {
    let mut db = database();
    let (result, trace) = traced(
        &mut db,
        "SELECT fish.name, tank.name FROM fish JOIN tank ON fish.tank = tank.id",
    );
    let join = &result.plan.children[0];
    assert_eq!(join.operator, "NestedLoopJoin");
    // Each fish finds its one tank by rowid, so no pair is tried and dropped.
    let lookup = &join.children[1];
    assert_eq!(
        (lookup.operator, lookup.detail.as_str()),
        (
            "IndexScan",
            "tank USING INTEGER PRIMARY KEY (id = fish.tank)"
        )
    );
    assert_eq!(events(&trace, join.id, &[EventKind::Out]).len(), 8);
    assert!(events(&trace, join.id, &[EventKind::Drop]).is_empty());
    assert_eq!(events(&trace, lookup.id, &[EventKind::Out]).len(), 8);
}

#[test]
fn a_correlated_subquery_runs_for_every_row_and_another_runs_once() {
    let mut db = database();
    let (result, trace) = traced(
        &mut db,
        "SELECT name FROM fish AS f WHERE weight = \
         (SELECT max(weight) FROM fish WHERE species = f.species)",
    );
    let subquery = nodes(&result.plan)
        .into_iter()
        .find(|n| n.operator == "Subquery")
        .unwrap();
    let runs = events(&trace, subquery.id, &[EventKind::Run]);
    assert_eq!(runs.len(), 8);
    // Each run is for the outer row the Filter is deciding.
    assert_eq!(runs[0].row[1], Value::Text("goldie".into()));
    assert!(events(&trace, subquery.id, &[EventKind::Reuse]).is_empty());

    let (result, trace) = traced(
        &mut db,
        "SELECT name FROM fish WHERE weight > (SELECT avg(weight) FROM fish)",
    );
    let subquery = nodes(&result.plan)
        .into_iter()
        .find(|n| n.operator == "Subquery")
        .unwrap();
    assert_eq!(events(&trace, subquery.id, &[EventKind::Run]).len(), 1);
    assert_eq!(events(&trace, subquery.id, &[EventKind::Reuse]).len(), 7);
}

#[test]
fn limit_and_distinct_drop_what_they_skip() {
    let mut db = database();
    let (result, trace) = traced(
        &mut db,
        "SELECT name FROM fish ORDER BY id LIMIT 2 OFFSET 3",
    );
    let limit = &result.plan;
    assert_eq!(limit.operator, "Limit");
    assert_eq!(events(&trace, limit.id, &[EventKind::Drop]).len(), 3);

    let (result, trace) = traced(&mut db, "SELECT DISTINCT species FROM fish");
    let distinct = &result.plan;
    assert_eq!(distinct.operator, "Distinct");
    // koi three times and goldfish twice: three repeats.
    assert_eq!(events(&trace, distinct.id, &[EventKind::Drop]).len(), 3);
}

#[test]
fn a_trace_stops_at_its_limit() {
    let mut db = database();
    db.set_trace(Some(5));
    let result = query(&mut db, "SELECT a.name FROM fish a, fish b");
    let trace = result.trace.unwrap();
    assert_eq!(trace.events.len(), 5);
    assert!(trace.truncated);
    assert_eq!(result.rows.len(), 64);
}

#[test]
fn the_inspector_shows_every_page_of_a_tree() {
    let mut db = database();
    db.execute("CREATE TABLE log(id INTEGER PRIMARY KEY, tag TEXT, note TEXT)")
        .unwrap();
    db.execute("CREATE INDEX log_tag ON log(tag)").unwrap();
    for i in 1..=400 {
        db.execute(&format!(
            "INSERT INTO log VALUES ({i}, 'tag {i}', 'feeding number {i}')"
        ))
        .unwrap();
    }
    let big = "fish ".repeat(1000);
    db.execute(&format!("INSERT INTO log VALUES (401, 'big', '{big}')"))
        .unwrap();

    let tree = db.inspect_tree("log").unwrap();
    assert_eq!((tree.kind, tree.depth), ("table", 2));
    assert!(!tree.truncated);
    let root = &tree.pages[0];
    assert_eq!((root.id, root.level, root.kind), (tree.root, 0, "interior"));
    let leaves: Vec<_> = tree.pages.iter().filter(|p| p.kind == "leaf").collect();
    assert_eq!(leaves.len(), root.children.len());
    assert_eq!(
        leaves.iter().map(|p| p.id).collect::<Vec<_>>(),
        root.children
    );
    // Every row once, in order, and the leaves linked left to right.
    let rowids: Vec<i64> = leaves
        .iter()
        .flat_map(|p| p.keys.iter().map(|k| k.rowid))
        .collect();
    assert_eq!(rowids, (1..=401).collect::<Vec<_>>());
    for pair in leaves.windows(2) {
        assert_eq!(pair[0].next, Some(pair[1].id));
    }
    assert!(tree.pages.iter().all(|p| p.used <= 4096 && p.used > 0));
    // The 5,000-byte note lives on two overflow pages.
    let last = leaves.last().unwrap();
    let [chain] = last.overflow.as_slice() else {
        panic!("one overflowing row");
    };
    assert_eq!((chain.bytes > 5000, chain.pages.len()), (true, 2));
    assert_eq!(last.keys[chain.cell].rowid, 401);

    let index = db.inspect_tree("log_tag").unwrap();
    assert_eq!(index.kind, "index");
    let first = &index.pages.iter().find(|p| p.kind == "leaf").unwrap().keys[0];
    assert_eq!(first.values, [Value::Text("big".into())]);
    assert_eq!(first.rowid, 401);
    assert!(db.inspect_tree("nope").is_err());
}
