//! WebAssembly bindings: the browser UI talks to the engine through this.
//!
//! Results cross the boundary as JSON strings. That keeps the interface to a
//! few methods, and `web/src/engine.ts` holds the matching TypeScript types.

use fishbowl::{Database, StatementResult, TableInfo};
use serde::Serialize;
use wasm_bindgen::prelude::*;

/// Keeps one runaway query from freezing the browser tab.
const ROW_BUDGET: u64 = 5_000_000;

/// Events kept per traced query. Far more than anyone steps through, and it
/// keeps a big join from sending megabytes of JSON.
const TRACE_LIMIT: usize = 5_000;

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
enum Entry {
    Ok {
        sql: String,
        result: StatementResult,
    },
    Error {
        sql: String,
        message: String,
    },
}

#[derive(Serialize)]
struct RunOutput {
    statements: Vec<Entry>,
    tables: Vec<TableInfo>,
}

#[wasm_bindgen]
pub struct Fishbowl {
    db: Database,
}

impl Default for Fishbowl {
    fn default() -> Self {
        Self::new()
    }
}

#[wasm_bindgen]
impl Fishbowl {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Fishbowl {
        let mut db = Database::new();
        db.set_row_budget(Some(ROW_BUDGET));
        Fishbowl { db }
    }

    /// Runs a script and returns every statement's outcome plus the tables
    /// that exist afterwards, as JSON. With `trace`, every query also
    /// returns what each step did with each row.
    pub fn run(&mut self, sql: &str, trace: bool) -> String {
        self.db.set_trace(trace.then_some(TRACE_LIMIT));
        let statements = self
            .db
            .run_script(sql)
            .into_iter()
            .map(|entry| match entry.result {
                Ok(result) => Entry::Ok {
                    sql: entry.sql,
                    result,
                },
                Err(e) => Entry::Error {
                    sql: entry.sql,
                    message: e.to_string(),
                },
            })
            .collect();
        let output = RunOutput {
            statements,
            tables: self.db.tables(),
        };
        serde_json::to_string(&output).unwrap_or_else(|e| {
            let message = format!("could not encode result: {e}");
            serde_json::json!({
                "statements": [{"status": "error", "sql": "", "message": message}],
                "tables": [],
            })
            .to_string()
        })
    }

    /// Every page of the B-tree behind a table or an index, as JSON:
    /// `{"tree": ...}` or `{"error": "..."}`.
    pub fn inspect(&self, name: &str) -> String {
        let output = match self.db.inspect_tree(name) {
            Ok(tree) => serde_json::json!({ "tree": tree }),
            Err(e) => serde_json::json!({ "error": e.to_string() }),
        };
        output.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    /// `web/src/engine.ts` depends on this exact shape.
    #[test]
    fn run_output_matches_the_typescript_contract() {
        let mut fishbowl = Fishbowl::new();
        let out: Value = serde_json::from_str(&fishbowl.run(
            "CREATE TABLE t(a INTEGER); INSERT INTO t VALUES (1), (2); SELECT a FROM t WHERE a > 1; \
             UPDATE t SET a = 3 WHERE a = 2; DELETE FROM t WHERE a = 1; \
             CREATE TABLE gone(x); DROP TABLE gone; \
             CREATE TABLE k(id INTEGER PRIMARY KEY, code TEXT UNIQUE); \
             CREATE INDEX t_a ON t(a DESC); SELECT a FROM t WHERE a = 3; \
             CREATE INDEX spare ON t(a); DROP INDEX spare; \
             CREATE VIEW v AS SELECT a FROM t; DROP VIEW v; \
             BEGIN; INSERT INTO t VALUES (9); ROLLBACK; BEGIN; COMMIT; ANALYZE t; SELECT b FROM t",
            false,
        ))
        .unwrap();

        let statements = out["statements"].as_array().unwrap();
        assert_eq!(
            statements[0]["result"],
            json!({"kind": "create_table", "table": "t"})
        );
        assert_eq!(
            statements[1]["result"],
            json!({"kind": "insert", "table": "t", "rows": 2})
        );

        let query = &statements[2]["result"];
        assert_eq!(query["kind"], "query");
        assert_eq!(query["columns"], json!(["a"]));
        assert_eq!(query["rows"], json!([[{"type": "integer", "text": "2"}]]));
        assert_eq!(query["plan"]["operator"], "Project");
        assert_eq!(query["plan"]["children"][0]["rows_out"], 1);
        assert!(query.get("trace").is_none());

        assert_eq!(
            statements[3]["result"],
            json!({"kind": "update", "table": "t", "rows": 1})
        );
        assert_eq!(
            statements[4]["result"],
            json!({"kind": "delete", "table": "t", "rows": 1})
        );
        assert_eq!(
            statements[6]["result"],
            json!({"kind": "drop_table", "table": "gone"})
        );

        assert_eq!(
            statements[8]["result"],
            json!({"kind": "create_index", "index": "t_a", "table": "t"})
        );
        let scan = &statements[9]["result"]["plan"]["children"][0]["children"][0];
        assert_eq!(scan["operator"], "IndexScan");
        assert_eq!(scan["detail"], "t USING COVERING INDEX t_a (a = 3)");
        assert!(scan["reason"].is_string());
        // The planner's estimate, and the plans it turned down.
        assert!(scan["estimate"]["rows"].is_number());
        assert!(scan["estimate"]["cost"].is_number());
        let rejected = &scan["rejected"][0];
        assert_eq!(rejected["plan"], "SeqScan t");
        assert!(rejected["rows"].is_number() && rejected["cost"].is_number());
        // Steps with nothing to choose leave both out.
        assert!(statements[9]["result"]["plan"].get("estimate").is_none());
        assert!(statements[9]["result"]["plan"].get("rejected").is_none());
        assert_eq!(
            statements[11]["result"],
            json!({"kind": "drop_index", "index": "spare"})
        );
        assert_eq!(
            statements[12]["result"],
            json!({"kind": "create_view", "view": "v"})
        );
        assert_eq!(
            statements[13]["result"],
            json!({"kind": "drop_view", "view": "v"})
        );

        assert_eq!(statements[14]["result"], json!({"kind": "begin"}));
        assert_eq!(statements[16]["result"], json!({"kind": "rollback"}));
        assert_eq!(statements[18]["result"], json!({"kind": "commit"}));

        assert_eq!(
            statements[19]["result"],
            json!({"kind": "analyze", "tables": ["t"]})
        );
        assert_eq!(statements[20]["status"], "error");
        assert_eq!(statements[20]["message"], "no such column: b");
        // Tables come sorted by name.
        let tables = out["tables"].as_array().unwrap();
        assert_eq!(tables.len(), 2);
        let t = &tables[1];
        assert_eq!(t["name"], "t");
        assert_eq!(t["row_count"], 1);
        assert_eq!(t["columns"][0]["affinity"], "integer");
        assert_eq!(t["columns"][0]["primary_key"], false);
        assert_eq!(
            t["indexes"],
            json!([{
                "name": "t_a",
                "columns": [{"name": "a", "descending": true}],
                "unique": false,
                "automatic": false,
            }])
        );
        let k = &tables[0];
        assert_eq!(k["columns"][0]["primary_key"], true);
        assert_eq!(k["indexes"][0]["name"], "sqlite_autoindex_k_1");
        assert_eq!(k["indexes"][0]["unique"], true);
        assert_eq!(k["indexes"][0]["automatic"], true);
    }

    #[test]
    fn traces_and_pages_match_the_typescript_contract() {
        let mut fishbowl = Fishbowl::new();
        fishbowl.run(
            "CREATE TABLE t(a INTEGER); INSERT INTO t VALUES (1), (2)",
            false,
        );
        let out: Value =
            serde_json::from_str(&fishbowl.run("SELECT a FROM t WHERE a > 1", true)).unwrap();
        let trace = &out["statements"][0]["result"]["trace"];
        assert_eq!(trace["truncated"], false);
        // The scan passes on both rows, the Filter drops the first.
        assert_eq!(
            trace["events"][0],
            json!({"step": 2, "kind": "out", "row": [{"type": "integer", "text": "1"}], "rowid": 1})
        );
        assert_eq!(
            trace["events"][1],
            json!({"step": 1, "kind": "drop", "row": [{"type": "integer", "text": "1"}]})
        );
        assert_eq!(
            trace["scans"],
            json!([{
                "step": 2,
                "table": "t",
                "row_count": 2,
                "rowids": [1, 2],
                "rows": [[{"type": "integer", "text": "1"}], [{"type": "integer", "text": "2"}]],
            }])
        );

        let pages: Value = serde_json::from_str(&fishbowl.inspect("t")).unwrap();
        let tree = &pages["tree"];
        assert_eq!(tree["kind"], "table");
        assert_eq!(tree["depth"], 1);
        assert_eq!(tree["truncated"], false);
        assert!(tree["file_pages"].is_number() && tree["free_pages"].is_number());
        let leaf = &tree["pages"][0];
        assert_eq!(leaf["id"], tree["root"]);
        assert_eq!(leaf["level"], 0);
        assert_eq!(leaf["kind"], "leaf");
        assert!(leaf["used"].is_number());
        assert_eq!(leaf["keys"][1], json!({"values": [], "rowid": 2}));
        assert_eq!(leaf["children"], json!([]));
        assert_eq!(leaf["next"], Value::Null);
        assert_eq!(leaf["overflow"], json!([]));

        let missing: Value = serde_json::from_str(&fishbowl.inspect("nope")).unwrap();
        assert_eq!(missing["error"], "no such table: nope");
    }
}
