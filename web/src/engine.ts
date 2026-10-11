// Typed wrapper around the WebAssembly engine. The JSON shapes below mirror
// crates/wasm/src/lib.rs, where a test pins them.
import init, { Fishbowl } from "./generated/fishbowl/fishbowl_wasm.js";

export type SqlValue =
  | { type: "null"; text: null }
  | { type: "integer" | "real" | "text"; text: string };

export interface PlanProfile {
  id: number;
  operator: string;
  detail: string;
  reason: string | null;
  rows_out: number;
  children: PlanProfile[];
}

export interface QueryResult {
  kind: "query";
  columns: string[];
  rows: SqlValue[][];
  plan: PlanProfile;
}

export type StatementResult =
  | { kind: "create_table"; table: string }
  | { kind: "create_index"; index: string; table: string }
  | { kind: "drop_index"; index: string }
  | { kind: "insert"; table: string; rows: number }
  | { kind: "update"; table: string; rows: number }
  | { kind: "delete"; table: string; rows: number }
  | { kind: "drop_table"; table: string }
  | { kind: "create_view"; view: string }
  | { kind: "drop_view"; view: string }
  | { kind: "begin" | "commit" | "rollback" }
  | QueryResult;

export type Entry =
  | { status: "ok"; sql: string; result: StatementResult }
  | { status: "error"; sql: string; message: string };

export interface Column {
  name: string;
  declared_type: string;
  affinity: string;
  not_null: boolean;
  primary_key: boolean;
}

export interface IndexInfo {
  name: string;
  columns: { name: string; descending: boolean }[];
  unique: boolean;
  /** Made by a PRIMARY KEY or UNIQUE constraint, not CREATE INDEX. */
  automatic: boolean;
}

export interface TableInfo {
  name: string;
  columns: Column[];
  indexes: IndexInfo[];
  row_count: number;
}

export interface RunOutput {
  statements: Entry[];
  tables: TableInfo[];
}

export interface Engine {
  run(sql: string): RunOutput;
  reset(): void;
}

export async function createEngine(): Promise<Engine> {
  await init();
  let db = new Fishbowl();
  return {
    run(sql) {
      const output: RunOutput = JSON.parse(db.run(sql));
      return output;
    },
    reset() {
      db.free();
      db = new Fishbowl();
    },
  };
}
