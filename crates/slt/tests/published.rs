//! The README and `sqllogictest/RESULTS.md` quote the pass rates by hand.
//! This test fails when a quoted row no longer matches the generated reports,
//! so a regenerated report forces the prose to follow.

use std::path::Path;

use serde_json::Value;

fn read(path: &str) -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::fs::read_to_string(root.join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// 5939879 -> "5,939,879".
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The table row both documents use for one corpus.
fn quoted_row(report_path: &str, label: &str) -> (String, String) {
    let report: Value = serde_json::from_str(&read(report_path)).expect("report is JSON");
    let totals = &report["totals"];
    let count = |key: &str, field: &str| totals[key][field].as_u64().expect("a count");
    let run = count("records", "passed") + count("records", "failed");
    let row = format!(
        "| {label} | {} | {} | {} | {} | **{:.2}%** | {:.2}% |",
        report["corpus"]["files"].as_u64().expect("a file count"),
        thousands(run),
        thousands(count("records", "failed")),
        thousands(totals["skipped"].as_u64().expect("a skipped count")),
        totals["pass_rate"].as_f64().expect("a rate"),
        totals["query_pass_rate"].as_f64().expect("a rate"),
    );
    let checkin = report["corpus"]["checkin"].as_str().expect("a check-in");
    (row, checkin.to_string())
}

#[test]
fn quoted_pass_rates_match_the_generated_reports() {
    for doc in ["README.md", "sqllogictest/RESULTS.md"] {
        let text = read(doc);
        for (report, label) in [
            ("sqllogictest/full.json", "Full suite"),
            ("sqllogictest/sample.json", "29-file sample"),
        ] {
            let (row, checkin) = quoted_row(report, label);
            assert!(
                text.contains(&row),
                "{doc} should contain this row from {report}:\n{row}"
            );
            assert!(
                text.contains(&checkin),
                "{doc} should name check-in {checkin}"
            );
        }
    }
}

#[test]
fn thousands_separators() {
    assert_eq!(thousands(7), "7");
    assert_eq!(thousands(1000), "1,000");
    assert_eq!(thousands(5_939_879), "5,939,879");
}
