//! `fishbowl`: a small SQL shell. Prints each query's rows and its plan tree.
//!
//! ```text
//! fishbowl                      interactive; statements end with `;`
//! fishbowl -c "SELECT 1 + 1"    run one script and exit
//! fishbowl < script.sql         run a file and exit
//! fishbowl --db fish.db ...     keep the data in a database file
//! ```

use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::process::ExitCode;

use fishbowl::{Database, QueryResult, StatementResult};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (mut db, args) = match args.as_slice() {
        [flag, path, rest @ ..] if flag == "--db" => match Database::open(path) {
            Ok(db) => (db, rest),
            Err(e) => {
                eprintln!("could not open {path}: {e}");
                return ExitCode::FAILURE;
            }
        },
        rest => (Database::new(), rest),
    };
    match args {
        [flag, sql] if flag == "-c" => run(&mut db, sql),
        [] if !io::stdin().is_terminal() => {
            let mut sql = String::new();
            if let Err(e) = io::stdin().read_to_string(&mut sql) {
                eprintln!("could not read stdin: {e}");
                return ExitCode::FAILURE;
            }
            run(&mut db, &sql)
        }
        [] => repl(&mut db),
        _ => {
            eprintln!("usage: fishbowl [--db FILE] [-c SQL]");
            ExitCode::from(2)
        }
    }
}

fn repl(db: &mut Database) -> ExitCode {
    println!(
        "Fishbowl {}. End statements with ';'. Ctrl-D exits.",
        env!("CARGO_PKG_VERSION")
    );
    let mut buffer = String::new();
    let stdin = io::stdin();
    loop {
        print!(
            "{}",
            if buffer.is_empty() {
                "fishbowl> "
            } else {
                "     ...> "
            }
        );
        io::stdout().flush().ok();
        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) | Err(_) => return ExitCode::SUCCESS,
            Ok(_) => {}
        }
        buffer.push_str(&line);
        if buffer.trim_end().ends_with(';') {
            run(db, &buffer);
            buffer.clear();
        }
    }
}

/// Runs a script, printing each statement's outcome. Fails on the first error.
fn run(db: &mut Database, sql: &str) -> ExitCode {
    for entry in db.run_script(sql) {
        match entry.result {
            Ok(StatementResult::CreateTable { table }) => println!("created table {table}"),
            Ok(StatementResult::Insert { table, rows }) => println!("inserted {rows} into {table}"),
            Ok(StatementResult::Update { table, rows }) => println!("updated {rows} in {table}"),
            Ok(StatementResult::Delete { table, rows }) => println!("deleted {rows} from {table}"),
            Ok(StatementResult::DropTable { table }) => println!("dropped table {table}"),
            Ok(StatementResult::Query(result)) => print_query(&result),
            Err(e) => {
                eprintln!("error: {e}\n  in: {}", entry.sql);
                return ExitCode::FAILURE;
            }
        }
    }
    ExitCode::SUCCESS
}

fn print_query(result: &QueryResult) {
    let cells: Vec<Vec<String>> = result
        .rows
        .iter()
        .map(|row| row.iter().map(ToString::to_string).collect())
        .collect();
    let widths: Vec<usize> = result
        .columns
        .iter()
        .enumerate()
        .map(|(i, name)| {
            cells
                .iter()
                .map(|row| row[i].chars().count())
                .chain([name.chars().count()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let line = |values: &[String]| {
        values
            .iter()
            .zip(&widths)
            .map(|(v, w)| format!("{v:<w$}"))
            .collect::<Vec<_>>()
            .join(" | ")
    };
    println!("{}", line(&result.columns));
    println!(
        "{}",
        widths
            .iter()
            .map(|w| "-".repeat(*w))
            .collect::<Vec<_>>()
            .join("-+-")
    );
    for row in &cells {
        println!("{}", line(row));
    }
    let noun = if cells.len() == 1 { "row" } else { "rows" };
    println!(
        "({} {noun})\n\nplan:\n{}",
        cells.len(),
        result.plan.render()
    );
}
