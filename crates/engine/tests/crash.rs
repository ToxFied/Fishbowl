//! The crash test: cut the power at every write a workload makes, and check
//! what the database holds when it comes back.
//!
//! The workload runs on a simulated disk ([`SimDisk`]) that keeps whatever
//! was synced and may keep, lose or tear anything written since. For each
//! operation the workload makes on the disk (every page write, log write,
//! log truncate and sync), the test runs the workload again from scratch
//! and cuts the power as that operation starts. Then it reboots the disk in
//! several ways:
//!
//! - lost: nothing written since each file's last sync survives;
//! - kept: everything written survives, but not the write in flight;
//! - torn: everything written survives, plus part of the write in flight,
//!   once for each way it can be torn ([`SimDisk::tears`]);
//! - random: each unsynced change survives on a coin flip, and the write in
//!   flight is torn at a random point or lost; four times per crash point.
//!
//! Each reboot opens the database, which runs recovery, and checks:
//!
//! 1. Durability: every transaction whose `COMMIT` returned is there.
//! 2. Atomicity: no other change is there, except that the transaction
//!    whose commit was running at the crash is there in full or not at all.
//! 3. Integrity: `integrity_check` passes. Every B-tree keeps its rules,
//!    every index matches its table, and every page has exactly one use.
//! 4. Life goes on: the recovered database takes a new transaction, which
//!    survives a reopen.
//!
//! Every rebooted disk whose log holds committed work is also crashed again
//! at every operation of its recovery, then rebooted and checked the same
//! way: recovery must survive a crash too.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use fishbowl::storage::fault::{Op, SimDisk, Tear};
use fishbowl::{Database, Options, StatementResult};

/// A small pool, so pages are evicted and read back from the log, and
/// frequent checkpoints, so the power also goes out during them.
const OPTIONS: Options = Options {
    pool_pages: 8,
    checkpoint_frames: 12,
};

fn workload() -> Vec<String> {
    let pad = "~".repeat(300);
    let rows = |ids: std::ops::Range<i64>| {
        ids.map(|i| format!("({i}, 'fish{i}', {}, '{pad}')", i % 7))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let long = "a long note ".repeat(800);
    // Text that changes along its whole length, so a page full of it has
    // no run of zeros at its end that a torn write could match by luck.
    let text = |seed: i64| -> String {
        (0..900)
            .map(|i| char::from(b'a' + ((i * 7 + seed * 13) % 26) as u8))
            .collect()
    };
    let full_rows = (1..30)
        .map(|i| format!("({i}, '{}')", text(i)))
        .collect::<Vec<_>>()
        .join(", ");
    [
        "CREATE TABLE fish(id INTEGER PRIMARY KEY, name TEXT UNIQUE, weight INTEGER NOT NULL, \
         note TEXT)"
            .into(),
        "CREATE INDEX fish_weight ON fish(weight)".into(),
        format!("INSERT INTO fish VALUES {}", rows(1..4)),
        // Big enough to split leaves of the table and of both indexes.
        "BEGIN".into(),
        format!("INSERT INTO fish VALUES {}", rows(4..40)),
        "UPDATE fish SET weight = weight + 10 WHERE id < 20".into(),
        "COMMIT".into(),
        // None of this may ever be seen.
        "BEGIN".into(),
        "DELETE FROM fish WHERE id > 10".into(),
        format!("INSERT INTO fish VALUES {}", rows(100..130)),
        "ROLLBACK".into(),
        // The clash rolls back the whole transaction, the first INSERT too.
        "BEGIN".into(),
        format!("INSERT INTO fish VALUES {}", rows(40..60)),
        "INSERT OR ROLLBACK INTO fish VALUES (1, 'clash', 0, NULL)".into(),
        // Overflow pages, then DDL inside a transaction that frees them.
        "CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT)".into(),
        format!("INSERT INTO notes VALUES (1, '{long}'), (2, 'short')"),
        "BEGIN".into(),
        "CREATE TABLE scratch(x UNIQUE)".into(),
        "INSERT INTO scratch VALUES (1), (2), (3)".into(),
        "DROP TABLE notes".into(),
        "UPDATE fish SET note = 'heavy' WHERE weight > 12".into(),
        "COMMIT".into(),
        // Merges, then inserts that reuse the freed pages.
        "DELETE FROM fish WHERE weight < 4".into(),
        format!("INSERT INTO fish VALUES {}", rows(60..90)),
        "CREATE INDEX fish_note ON fish(note)".into(),
        "DROP INDEX fish_weight".into(),
        "UPDATE fish SET weight = weight * 2".into(),
        // Leaves about 90% full of text. The update's only changed page is
        // one of them, so a tear in its commit frame almost always cuts live
        // data rather than trailing zeros.
        "CREATE TABLE full(id INTEGER PRIMARY KEY, body TEXT)".into(),
        format!("INSERT INTO full VALUES {full_rows}"),
        format!("UPDATE full SET body = '{}' WHERE id = 2", text(99)),
    ]
    .into()
}

fn open(disk: &SimDisk) -> fishbowl::Result<Database> {
    Database::with_stores(disk.database(), disk.log(), OPTIONS)
}

/// Every table's name, its indexes and its rows, as text.
fn snapshot(db: &mut Database) -> String {
    let mut out = String::new();
    for table in db.tables() {
        let indexes: Vec<&str> = table
            .schema
            .indexes
            .iter()
            .map(|i| i.name.as_str())
            .collect();
        writeln!(out, "{} {indexes:?}", table.schema.name).unwrap();
        let sql = format!("SELECT * FROM {}", table.schema.name);
        let Ok(StatementResult::Query(result)) = db.execute(&sql) else {
            panic!("{sql} failed");
        };
        for row in result.rows {
            writeln!(out, "  {row:?}").unwrap();
        }
    }
    out
}

/// The workload run once without a crash.
struct Oracle {
    /// Whether each statement succeeds.
    ok: Vec<bool>,
    /// `committed[i]`: what is committed after the first `i` statements.
    committed: Vec<String>,
    /// Every operation the workload makes on the disk.
    ops: Vec<Op>,
}

fn oracle(workload: &[String]) -> Oracle {
    let disk = SimDisk::new();
    let mut db = open(&disk).unwrap();
    let mut committed = vec![snapshot(&mut db)];
    let mut ok = Vec::new();
    for sql in workload {
        ok.push(db.execute(sql).is_ok());
        let state = match db.in_transaction() {
            true => committed.last().unwrap().clone(),
            false => snapshot(&mut db),
        };
        committed.push(state);
    }
    // The workload must mean what its comments say.
    assert_eq!(
        ok.iter().filter(|ok| !**ok).count(),
        1,
        "only OR ROLLBACK fails"
    );
    assert!(committed.last().unwrap().contains("fish_note"));
    assert!(!committed.last().unwrap().contains("fish100"));
    Oracle {
        ok,
        committed,
        ops: disk.ops(),
    }
}

/// Runs the workload until the power goes out at operation `n`. Returns the
/// crashed disk and the states recovery may bring back: the last committed
/// one, and the next one too if a commit was running at the crash.
fn crash_at<'a>(workload: &[String], oracle: &'a Oracle, n: usize) -> (SimDisk, Vec<&'a str>) {
    let disk = SimDisk::new();
    disk.crash_at(n);
    let committed = &oracle.committed;
    let allowed = match open(&disk) {
        // The power went out while the new database was being created.
        Err(_) => vec![committed[0].as_str()],
        Ok(mut db) => {
            let mut allowed = None;
            for (i, sql) in workload.iter().enumerate() {
                let result = db.execute(sql);
                if disk.crashed() {
                    // A statement that returned Ok committed, even if a
                    // checkpoint after it was cut short.
                    allowed = Some(match result {
                        Ok(_) => vec![committed[i + 1].as_str()],
                        Err(_) => vec![committed[i].as_str(), committed[i + 1].as_str()],
                    });
                    break;
                }
                assert_eq!(result.is_ok(), oracle.ok[i], "{sql}");
            }
            allowed.unwrap_or_else(|| panic!("operation {n} is past the end of the workload"))
        }
    };
    assert_eq!(
        disk.ops(),
        oracle.ops[..=n],
        "the run must repeat the oracle's operations"
    );
    (disk, allowed)
}

/// splitmix64: a small seeded random number generator, so every run
/// makes the same choices.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// The ways a crashed disk can come back.
fn reboots(disk: &SimDisk, seed: u64) -> Vec<(String, SimDisk)> {
    let tears = disk.tears();
    let mut out = vec![
        ("lost".to_string(), disk.reboot(|_| false, None)),
        ("kept".to_string(), disk.reboot(|_| true, None)),
    ];
    for &tear in &tears {
        out.push((format!("torn, {tear:?}"), disk.reboot(|_| true, Some(tear))));
    }
    let mut rng = Rng(seed);
    for i in 0..4 {
        let tear: Option<Tear> = match rng.below(tears.len() + 1) {
            0 => None,
            t => Some(tears[t - 1]),
        };
        let rebooted = disk.reboot(|_| rng.next() % 2 == 0, tear);
        out.push((format!("random {i}, {tear:?}"), rebooted));
    }
    out
}

#[derive(Default)]
struct Tally {
    crash_points: BTreeMap<String, usize>,
    recoveries: usize,
    replayed: usize,
    discarded: usize,
    double_crashes: usize,
    /// Crashes during a commit after which the commit was found, and after
    /// which it was not. Both must happen, or "either" was never tested.
    in_flight_kept: usize,
    in_flight_lost: usize,
}

/// Opens a rebooted disk and checks the guarantees. Returns whether the
/// state found is the newest one allowed.
fn check(disk: &SimDisk, allowed: &[&str], tally: &mut Tally, context: &str) -> bool {
    let mut db = open(disk).unwrap_or_else(|e| panic!("{context}: recovery failed: {e}"));
    tally.recoveries += 1;
    let recovery = db.recovery();
    tally.replayed += usize::from(recovery.transactions > 0);
    tally.discarded += usize::from(recovery.discarded_bytes > 0);
    db.integrity_check()
        .unwrap_or_else(|e| panic!("{context}: {e}"));
    let state = snapshot(&mut db);
    assert!(
        allowed.contains(&state.as_str()),
        "{context}: recovered a state that was never committed:\n{state}\nallowed:\n{}",
        allowed.join("\n---\n")
    );
    state == *allowed.last().unwrap()
}

/// The recovered database takes a new transaction, which survives a reopen.
fn check_life_goes_on(disk: &SimDisk, context: &str) {
    let mut db = open(disk).unwrap();
    db.execute("CREATE TABLE IF NOT EXISTS after_crash(x)")
        .unwrap();
    db.execute("INSERT INTO after_crash VALUES (42)").unwrap();
    drop(db);
    let mut db = open(&disk.duplicate()).unwrap();
    let Ok(StatementResult::Query(result)) = db.execute("SELECT count(*) FROM after_crash") else {
        panic!("{context}: the table made after recovery is gone");
    };
    assert_eq!(result.rows[0][0].to_string(), "1", "{context}");
    db.integrity_check()
        .unwrap_or_else(|e| panic!("{context}: after recovery: {e}"));
}

/// Cuts the power again at every operation of this disk's recovery, then
/// checks what a second reboot brings back.
fn check_crash_during_recovery(disk: &SimDisk, allowed: &[&str], tally: &mut Tally, context: &str) {
    let probe = disk.duplicate();
    open(&probe).unwrap();
    for n in 0..probe.ops().len() {
        let again = disk.duplicate();
        again.crash_at(n);
        assert!(open(&again).is_err(), "{context}: recovery ignored a crash");
        for (how, rebooted) in [
            ("lost", again.reboot(|_| false, None)),
            ("kept", again.reboot(|_| true, None)),
        ] {
            let context = format!("{context}, then crashed at recovery operation {n} ({how})");
            check(&rebooted, allowed, tally, &context);
            tally.double_crashes += 1;
        }
    }
}

#[test]
fn every_crash_point_recovers_to_a_committed_state() {
    let workload = workload();
    let oracle = oracle(&workload);
    let mut tally = Tally::default();
    for (n, op) in oracle.ops.iter().enumerate() {
        *tally.crash_points.entry(format!("{op:?}")).or_default() += 1;
        let (disk, allowed) = crash_at(&workload, &oracle, n);
        for (how, rebooted) in reboots(&disk, n as u64) {
            let context = format!("crash at operation {n} ({op:?}), {how}");
            let snapshot = rebooted.duplicate();
            let newest = check(&rebooted, &allowed, &mut tally, &context);
            if allowed.len() == 2 {
                match newest {
                    true => tally.in_flight_kept += 1,
                    false => tally.in_flight_lost += 1,
                }
            }
            check_life_goes_on(&rebooted, &context);
            if how == "kept" {
                check_crash_during_recovery(&snapshot, &allowed, &mut tally, &context);
            }
        }
    }

    let total: usize = tally.crash_points.values().sum();
    eprintln!(
        "crash points: {total} {:?}\nrecoveries checked: {} ({} replayed transactions, {} \
         discarded a torn or unfinished tail), {} after a second crash during recovery\n\
         commit in flight at the crash: kept {} times, lost {} times",
        tally.crash_points,
        tally.recoveries,
        tally.replayed,
        tally.discarded,
        tally.double_crashes,
        tally.in_flight_kept,
        tally.in_flight_lost,
    );
    // Guard the test itself: each kind of crash must really happen.
    for op in [
        "PageWrite",
        "PageSync",
        "LogWrite",
        "LogTruncate",
        "LogSync",
    ] {
        assert!(
            tally.crash_points.contains_key(op),
            "no crash during a {op}"
        );
    }
    assert!(tally.replayed > 0 && tally.discarded > 0 && tally.double_crashes > 0);
    assert!(tally.in_flight_kept > 0 && tally.in_flight_lost > 0);
}
