//! `fishbowl-slt`: runs the pinned sqllogictest corpus and reports a pass rate.
//!
//! ```text
//! fishbowl-slt [--corpus DIR] [--manifest FILE] [--json FILE] [--markdown FILE]
//!              [--jobs N] [--verbose] [FILE...]
//! ```
//!
//! With no FILE arguments it runs every file in the manifest after checking its
//! SHA-256. Naming files skips that check and marks the report unpinned.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;
use std::time::Instant;

use fishbowl_slt::{FileResult, corpus, report, run_file};

struct Args {
    corpus: PathBuf,
    manifest: PathBuf,
    json: Option<PathBuf>,
    markdown: Option<PathBuf>,
    jobs: usize,
    verbose: bool,
    files: Vec<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        corpus: PathBuf::from("sqllogictest/test"),
        manifest: PathBuf::from("sqllogictest/manifest.txt"),
        json: None,
        markdown: None,
        jobs: std::thread::available_parallelism().map_or(1, |n| n.get()),
        verbose: false,
        files: Vec::new(),
    };
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        let mut value = || iter.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--corpus" => args.corpus = value()?.into(),
            "--manifest" => args.manifest = value()?.into(),
            "--json" => args.json = Some(value()?.into()),
            "--markdown" => args.markdown = Some(value()?.into()),
            "--jobs" => args.jobs = value()?.parse().map_err(|_| "--jobs needs a number")?,
            "--verbose" => args.verbose = true,
            flag if flag.starts_with("--") => return Err(format!("unknown flag {flag}")),
            file => args.files.push(file.to_string()),
        }
    }
    Ok(args)
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    let manifest_text = std::fs::read_to_string(&args.manifest)
        .map_err(|e| format!("{}: {e}", args.manifest.display()))?;
    let pinned = args.files.is_empty();

    let paths: Vec<(PathBuf, String)> = if pinned {
        let files = corpus::parse_manifest(&manifest_text)?;
        corpus::verify(&args.corpus, &files).map_err(|problems| {
            format!(
                "corpus does not match the manifest. Run scripts/fetch-sqllogictest.sh.\n{problems}"
            )
        })?;
        files
            .into_iter()
            .map(|f| (args.corpus.join(&f.path), f.path))
            .collect()
    } else {
        args.files
            .iter()
            .map(|f| (PathBuf::from(f), f.clone()))
            .collect()
    };

    let started = Instant::now();
    // Engine panics are caught and counted per record; keep their default
    // backtrace noise out of the report output.
    std::panic::set_hook(Box::new(|_| {}));
    let results = run_parallel(&paths, args.jobs.max(1))?;
    let _ = std::panic::take_hook();

    if args.verbose {
        for file in &results {
            for f in &file.failures {
                eprintln!("{}:{}: [{}] {}", file.path, f.line, f.reason, f.message);
            }
        }
    }

    let info = report::CorpusInfo {
        source: corpus::header(&manifest_text, "source").unwrap_or_default(),
        checkin: corpus::header(&manifest_text, "checkin").unwrap_or_default(),
        files: paths.len(),
        pinned,
    };
    let report = report::build(info, results);
    let t = &report.totals;
    println!(
        "{} files, {} records run, {} skipped, {:.1}s",
        report.corpus.files,
        t.records.total(),
        t.skipped,
        started.elapsed().as_secs_f64()
    );
    println!(
        "statements {}/{}  queries {}/{}  pass rate {:.2}% (queries {:.2}%)",
        t.statements.passed,
        t.statements.total(),
        t.queries.passed,
        t.queries.total(),
        t.pass_rate,
        t.query_pass_rate
    );
    for reason in report.failure_reasons.iter().take(8) {
        println!("  {:>7}  {}", reason.count, reason.reason);
    }

    if let Some(path) = &args.json {
        let json = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
        write(path, &(json + "\n"))?;
    }
    if let Some(path) = &args.markdown {
        write(path, &report::markdown(&report))?;
    }
    Ok(())
}

/// Runs files on `jobs` threads. Each file gets its own database, so the
/// order files finish in cannot change any result.
fn run_parallel(paths: &[(PathBuf, String)], jobs: usize) -> Result<Vec<FileResult>, String> {
    let queue = Mutex::new(paths.iter());
    let results = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..jobs.min(paths.len()) {
            scope.spawn(|| {
                loop {
                    let next = queue.lock().map(|mut q| q.next()).ok().flatten();
                    let Some((path, name)) = next else { break };
                    let result = run_file(path, name);
                    if let Ok(mut all) = results.lock() {
                        all.push(result);
                    }
                }
            });
        }
    });
    results
        .into_inner()
        .map_err(|_| "a worker thread failed".to_string())?
        .into_iter()
        .collect()
}

fn write(path: &Path, contents: &str) -> Result<(), String> {
    std::fs::write(path, contents).map_err(|e| format!("{}: {e}", path.display()))
}
