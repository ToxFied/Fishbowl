//! WebAssembly bindings: the browser UI talks to the engine through this.
//!
//! Results cross the boundary as JSON strings. That keeps the interface to two
//! methods, and `web/src/engine.ts` holds the matching TypeScript types.

use fishbowl::{Database, StatementResult, TableInfo};
use serde::Serialize;
use wasm_bindgen::prelude::*;

/// Keeps one runaway query from freezing the browser tab.
const ROW_BUDGET: u64 = 5_000_000;

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
    /// that exist afterwards, as JSON.
    pub fn run(&mut self, sql: &str) -> String {
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
             CREATE VIEW v AS SELECT a FROM t; DROP VIEW v; SELECT b FROM t",
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
        assert_eq!(scan["detail"], "t USING INDEX t_a (a = 3)");
        assert!(scan["reason"].is_string());
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

        assert_eq!(statements[14]["status"], "error");
        assert_eq!(statements[14]["message"], "no such column: b");
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
}
