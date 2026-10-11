//! Row-level execution trace: what each step of a plan did with each row,
//! in the order it happened, so the UI can play a query back.
//!
//! Tracing is off unless asked for. The executor keeps an
//! `Option<Recorder>`, and with `None` each row costs one check of that
//! option and nothing else: no row is copied and nothing is stored. The
//! sqllogictest harness and the CLI never turn it on.
//!
//! A trace records what a step *did*, not what it was handed. A step reads a
//! row when its input passes one up, so "Filter read row r" is the event
//! "Scan passed r on", followed by the Filter's own verdict: passed on, or
//! dropped. Sort, Aggregate and Materialize read rows and pass nothing on
//! until their input runs out; their reads show up as their input's events.

use serde::Serialize;

use crate::storage::Row;

/// Everything one query did, row by row.
#[derive(Debug, Clone, Serialize)]
pub struct Trace {
    /// In the order they happened.
    pub events: Vec<Event>,
    /// More events happened than the recorder's limit; `events` holds the
    /// first ones.
    pub truncated: bool,
    /// For every scan step, the table's rows, so the UI can show which rows
    /// the scan read and which it never touched.
    pub scans: Vec<ScanRows>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    /// The step's id, as in [`PlanProfile::id`](crate::PlanProfile::id).
    pub step: usize,
    pub kind: EventKind,
    /// The row this is about. For `run` and `reuse`, the outer row the
    /// subquery runs for.
    pub row: Row,
    /// Set when a scan read the row from its table.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rowid: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    /// The step passed the row on to the step above it.
    Out,
    /// The step read the row and threw it away: a Filter whose condition was
    /// not true, a join pair that did not match, a repeat in Distinct, a row
    /// Limit skipped for OFFSET.
    Drop,
    /// A subquery started a run.
    Run,
    /// An uncorrelated subquery handed back the answer of its first run.
    Reuse,
}

/// A table a scan step reads, and every rowid in it.
#[derive(Debug, Clone, Serialize)]
pub struct ScanRows {
    pub step: usize,
    pub table: String,
    pub row_count: usize,
    /// In rowid order. Empty when the table has more than [`MAX_SCAN_ROWIDS`]
    /// rows; `row_count` still says how many.
    pub rowids: Vec<i64>,
    /// The rows themselves, in the same order, for a table of at most
    /// [`MAX_SCAN_ROWS`] rows. Empty for a bigger one.
    pub rows: Vec<Row>,
}

/// Above this many rows a table is drawn as a strip of marks, not rowids.
pub const MAX_SCAN_ROWIDS: usize = 2000;
/// Above this many rows a table is too big to print row by row.
pub const MAX_SCAN_ROWS: usize = 32;

/// Collects events while a query runs, up to a limit.
#[derive(Debug)]
pub struct Recorder {
    limit: usize,
    trace: Trace,
}

impl Recorder {
    pub fn new(limit: usize) -> Recorder {
        Recorder {
            limit,
            trace: Trace {
                events: Vec::new(),
                truncated: false,
                scans: Vec::new(),
            },
        }
    }

    pub fn record(
        &mut self,
        step: usize,
        kind: EventKind,
        row: &[crate::Value],
        rowid: Option<i64>,
    ) {
        if self.trace.events.len() >= self.limit {
            self.trace.truncated = true;
            return;
        }
        self.trace.events.push(Event {
            step,
            kind,
            row: row.to_vec(),
            rowid,
        });
    }

    pub fn add_scan(&mut self, scan: ScanRows) {
        self.trace.scans.push(scan);
    }

    pub fn finish(self) -> Trace {
        self.trace
    }
}
