//! Runs sqllogictest files against Fishbowl.
//!
//! The `sqllogictest` crate parses the files. Evaluation is done here, written
//! to match SQLite's reference runner (`sqllogictest.c` and `slt_sqlite.c`):
//! values are formatted by the column type the test declares, sorted per the
//! sort mode, and hashed with MD5 above the hash threshold. Doing it ourselves
//! keeps those rules visible and testable in one place.

pub mod corpus;
pub mod report;

use std::panic::{self, AssertUnwindSafe};
use std::path::Path;

use fishbowl::{Database, Error, StatementResult, Value};
use serde::Serialize;
use sqllogictest::{Condition, DefaultColumnType, QueryExpect, Record, SortMode, StatementExpect};

/// The labels Fishbowl answers to in `skipif` / `onlyif` lines. The expected
/// results in SQLite's suite are SQLite's own answers, and Fishbowl targets
/// SQLite semantics, so it runs the records marked for SQLite.
pub const ENGINE_LABELS: [&str; 1] = ["sqlite"];

/// The most rows one statement may process before it is failed. This keeps a
/// pathological query from hanging the run, and fails it the same way on every
/// machine, unlike a wall-clock timeout.
pub const ROW_BUDGET: u64 = 20_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordKind {
    Statement,
    Query,
}

/// Why a record failed. `reason` is a short, stable bucket name used to group
/// failures in the report; `message` is the specific detail.
#[derive(Debug, Clone, Serialize)]
pub struct Failure {
    pub line: u32,
    pub kind: RecordKind,
    pub reason: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
pub struct Counts {
    pub passed: u64,
    pub failed: u64,
}

impl Counts {
    pub fn total(&self) -> u64 {
        self.passed + self.failed
    }

    pub fn add(&mut self, other: Counts) {
        self.passed += other.passed;
        self.failed += other.failed;
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct FileResult {
    pub path: String,
    pub statements: Counts,
    pub queries: Counts,
    /// Records not run because a `skipif`/`onlyif` line excludes SQLite.
    pub skipped: u64,
    #[serde(skip)]
    pub failures: Vec<Failure>,
}

impl FileResult {
    pub fn records(&self) -> Counts {
        let mut all = self.statements;
        all.add(self.queries);
        all
    }

    fn record(&mut self, kind: RecordKind, line: u32, outcome: Result<(), (String, String)>) {
        let counts = match kind {
            RecordKind::Statement => &mut self.statements,
            RecordKind::Query => &mut self.queries,
        };
        match outcome {
            Ok(()) => counts.passed += 1,
            Err((reason, message)) => {
                counts.failed += 1;
                self.failures.push(Failure {
                    line,
                    kind,
                    reason,
                    message,
                });
            }
        }
    }
}

/// Runs one test file against a fresh database. A file that cannot be read or
/// parsed is an error: the caller must not silently count it as zero records.
pub fn run_file(path: &Path, display_name: &str) -> Result<FileResult, String> {
    let script = std::fs::read_to_string(path).map_err(|e| format!("{display_name}: {e}"))?;
    run_script(&script, display_name)
}

pub fn run_script(script: &str, name: &str) -> Result<FileResult, String> {
    let script = strip_condition_comments(script);
    let records = sqllogictest::parse_with_name::<DefaultColumnType>(&script, name)
        .map_err(|e| format!("{name}: {e}"))?;

    let mut db = Database::new();
    db.set_row_budget(Some(ROW_BUDGET));
    let mut result = FileResult {
        path: name.to_string(),
        ..FileResult::default()
    };
    let mut hash_threshold: u64 = 0;
    // Conditions apply to the record that follows them in the same block.
    // The crate attaches them to statements and queries but not to `halt`,
    // and leaks a halt's conditions into the next record, so track them here.
    let mut conditions: Vec<Condition> = Vec::new();

    for record in records {
        let pending = match &record {
            Record::Condition(c) => {
                conditions.push(c.clone());
                continue;
            }
            Record::Comment(_) => continue,
            _ => std::mem::take(&mut conditions),
        };
        match record {
            Record::Statement {
                loc, sql, expected, ..
            } => {
                if skipped(&pending) {
                    result.skipped += 1;
                    continue;
                }
                let outcome = check_statement(run_guarded(&mut db, &sql), &expected);
                result.record(RecordKind::Statement, loc.line(), outcome);
            }
            Record::Query {
                loc, sql, expected, ..
            } => {
                if skipped(&pending) {
                    result.skipped += 1;
                    continue;
                }
                let outcome = check_query(run_guarded(&mut db, &sql), &expected, hash_threshold);
                result.record(RecordKind::Query, loc.line(), outcome);
            }
            Record::HashThreshold { threshold, .. } => hash_threshold = threshold,
            Record::Halt { .. } if !skipped(&pending) => break,
            Record::Halt { .. } | Record::Newline | Record::Control(_) => {}
            other => return Err(format!("{name}: unsupported directive {other:?}")),
        }
    }
    Ok(result)
}

/// SQLite's suite writes notes after conditions (`skipif mysql # not
/// compatible`). The reference runner reads only the label; the `sqllogictest`
/// crate rejects the line. Dropping the note keeps every line number intact.
fn strip_condition_comments(script: &str) -> String {
    let mut out = String::with_capacity(script.len());
    for line in script.lines() {
        let is_condition = line.starts_with("skipif ") || line.starts_with("onlyif ");
        match line.split_once(" #") {
            Some((directive, _)) if is_condition => out.push_str(directive.trim_end()),
            _ => out.push_str(line),
        }
        out.push('\n');
    }
    out
}

fn skipped(conditions: &[Condition]) -> bool {
    conditions.iter().any(|c| match c {
        Condition::OnlyIf { label } => !ENGINE_LABELS.contains(&label.as_str()),
        Condition::SkipIf { label } => ENGINE_LABELS.contains(&label.as_str()),
    })
}

/// What happened when the engine ran one SQL string. A panic is captured as
/// its own outcome so it fails one record instead of ending the run.
enum RunOutcome {
    Ok(StatementResult),
    Err(Error),
    Panic(String),
}

fn run_guarded(db: &mut Database, sql: &str) -> RunOutcome {
    match panic::catch_unwind(AssertUnwindSafe(|| db.execute(sql))) {
        Ok(Ok(result)) => RunOutcome::Ok(result),
        Ok(Err(e)) => RunOutcome::Err(e),
        Err(payload) => {
            let message = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "unknown panic".into());
            RunOutcome::Panic(message)
        }
    }
}

type Outcome = Result<(), (String, String)>;

fn fail(reason: impl Into<String>, message: impl Into<String>) -> Outcome {
    Err((reason.into(), message.into()))
}

/// Groups an engine error into a report bucket.
fn error_reason(e: &Error) -> String {
    match e {
        Error::Unsupported(what) => format!("unsupported: {what}"),
        Error::Parse(_) => "parse error".into(),
        Error::Catalog(msg) => msg.split(':').next().unwrap_or(msg).to_string(),
        Error::Constraint(_) => "constraint failed".into(),
        Error::Runtime(msg) => msg.clone(),
        Error::BudgetExceeded(_) => "row budget exceeded".into(),
    }
}

/// Only errors that mean "this SQL is wrong" satisfy `statement error`.
/// "Not supported yet" and "ran out of budget" are Fishbowl's shortcomings, not
/// a correct rejection, so they must never count as a pass.
fn is_genuine_rejection(e: &Error) -> bool {
    matches!(
        e,
        Error::Parse(_) | Error::Catalog(_) | Error::Constraint(_) | Error::Runtime(_)
    )
}

fn check_statement(outcome: RunOutcome, expected: &StatementExpect) -> Outcome {
    match (outcome, expected) {
        (RunOutcome::Panic(msg), _) => fail("engine panic", msg),
        (RunOutcome::Ok(_), StatementExpect::Ok) => Ok(()),
        (RunOutcome::Ok(result), StatementExpect::Count(n)) => match result {
            StatementResult::Insert { rows, .. } if rows == *n => Ok(()),
            other => fail(
                "wrong row count",
                format!("expected {n} rows, got {other:?}"),
            ),
        },
        (RunOutcome::Ok(_), StatementExpect::Error(_)) => fail(
            "accepted invalid statement",
            "expected an error, statement succeeded",
        ),
        (RunOutcome::Err(e), StatementExpect::Error(_)) if is_genuine_rejection(&e) => Ok(()),
        (RunOutcome::Err(e), _) => fail(error_reason(&e), e.to_string()),
    }
}

fn check_query(
    outcome: RunOutcome,
    expected: &QueryExpect<DefaultColumnType>,
    hash_threshold: u64,
) -> Outcome {
    let result = match (outcome, expected) {
        (RunOutcome::Panic(msg), _) => return fail("engine panic", msg),
        (RunOutcome::Err(e), QueryExpect::Error(_)) if is_genuine_rejection(&e) => return Ok(()),
        (RunOutcome::Err(e), _) => return fail(error_reason(&e), e.to_string()),
        (RunOutcome::Ok(_), QueryExpect::Error(_)) => {
            return fail(
                "accepted invalid query",
                "expected an error, query succeeded",
            );
        }
        (RunOutcome::Ok(StatementResult::Query(result)), QueryExpect::Results { .. }) => result,
        (RunOutcome::Ok(_), QueryExpect::Results { .. }) => {
            return fail("no result set", "query did not return rows");
        }
    };
    let QueryExpect::Results {
        types,
        sort_mode,
        results: expected_lines,
        ..
    } = expected
    else {
        unreachable!("error expectations are handled above")
    };

    if result.columns.len() != types.len() {
        return fail(
            "wrong column count",
            format!(
                "expected {} columns, got {}",
                types.len(),
                result.columns.len()
            ),
        );
    }
    let mut rows: Vec<Vec<String>> = result
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .zip(types)
                .map(|(value, ty)| format_value(value, ty))
                .collect()
        })
        .collect();

    let values: Vec<String> = match sort_mode {
        Some(SortMode::RowSort) => {
            rows.sort();
            rows.into_iter().flatten().collect()
        }
        Some(SortMode::ValueSort) => {
            let mut values: Vec<String> = rows.into_iter().flatten().collect();
            values.sort();
            values
        }
        _ => rows.into_iter().flatten().collect(),
    };

    let actual = if hash_threshold > 0 && values.len() as u64 > hash_threshold {
        vec![hash_values(&values)]
    } else {
        values
    };
    if actual == *expected_lines {
        Ok(())
    } else {
        fail(
            "wrong result",
            format!(
                "expected [{}], got [{}]",
                preview(expected_lines),
                preview(&actual)
            ),
        )
    }
}

fn preview(lines: &[String]) -> String {
    let shown: Vec<&str> = lines.iter().take(6).map(String::as_str).collect();
    let more = lines.len().saturating_sub(shown.len());
    if more > 0 {
        format!("{} ... +{more}", shown.join(", "))
    } else {
        shown.join(", ")
    }
}

/// Formats a value as the reference runner does for the column's declared
/// type: `I` as a 64-bit integer, `R` with three decimals, `T` as text.
pub fn format_value(value: &Value, ty: &DefaultColumnType) -> String {
    if value.is_null() {
        return "NULL".into();
    }
    match ty {
        DefaultColumnType::Integer => value.to_i64_lossy().to_string(),
        DefaultColumnType::FloatingPoint => format!("{:.3}", value.to_f64_lossy()),
        DefaultColumnType::Text | DefaultColumnType::Any => {
            let text = value.to_string();
            if text.is_empty() {
                "(empty)".into()
            } else {
                text.chars()
                    .map(|c| if (' '..='~').contains(&c) { c } else { '@' })
                    .collect()
            }
        }
    }
}

/// The reference runner's hash: MD5 over every value followed by a newline.
pub fn hash_values(values: &[String]) -> String {
    let mut context = md5::Context::new();
    for value in values {
        context.consume(value.as_bytes());
        context.consume(b"\n");
    }
    format!(
        "{} values hashing to {:x}",
        values.len(),
        context.finalize()
    )
}
