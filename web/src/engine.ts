// Typed wrapper around the WebAssembly engine. The JSON shapes below mirror
// crates/wasm/src/lib.rs, where a test pins them.
import init, { Fishbowl } from "./generated/fishbowl/fishbowl_wasm.js";

export type SqlValue =
  | { type: "null"; text: null }
  | { type: "integer" | "real" | "text" | "blob"; text: string };

/** The planner's guess at a step's rows and work, in rows read. */
export interface Estimate {
  rows: number;
  cost: number;
}

/** A plan the planner weighed and turned down. */
export interface Rejected extends Estimate {
  plan: string;
}

export interface PlanProfile {
  id: number;
  operator: string;
  detail: string;
  reason: string | null;
  /** Set where the planner compared costed plans: the one it chose, in one line. */
  plan?: string;
  estimate?: Estimate;
  /** The plans it turned down, cheapest first. */
  rejected?: Rejected[];
  rows_out: number;
  children: PlanProfile[];
}

export type Row = SqlValue[];

export interface TraceEvent {
  /** The plan step's `id`. */
  step: number;
  /** out: passed the row on. drop: read it and threw it away.
   * run: a subquery started for this outer row. reuse: it handed back its first answer. */
  kind: "out" | "drop" | "run" | "reuse";
  row: Row;
  /** Set when a scan read the row from its table. */
  rowid?: number;
}

export interface ScanRows {
  step: number;
  table: string;
  row_count: number;
  /** Every rowid in the table, in order. Empty for tables too big to draw. */
  rowids: number[];
  /** The rows, in the same order, for a table small enough to print. */
  rows: Row[];
}

export interface Trace {
  events: TraceEvent[];
  truncated: boolean;
  scans: ScanRows[];
}

export interface QueryResult {
  kind: "query";
  columns: string[];
  rows: Row[];
  plan: PlanProfile;
  trace?: Trace;
}

export type StatementResult =
  | { kind: "create_table"; table: string }
  | { kind: "create_index"; index: string; table: string }
  | { kind: "drop_index"; index: string }
  | { kind: "analyze"; tables: string[] }
  | { kind: "insert"; table: string; rows: number }
  | { kind: "update"; table: string; rows: number }
  | { kind: "delete"; table: string; rows: number }
  | { kind: "drop_table"; table: string }
  | { kind: "create_view"; view: string }
  | { kind: "drop_view"; view: string }
  | { kind: "begin" | "commit" | "rollback" | "reindex" }
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

export interface KeyInfo {
  /** An index key's values. Empty for a table, whose key is the rowid. */
  values: SqlValue[];
  rowid: number;
}

export interface PageInfo {
  id: number;
  /** 0 for the root. */
  level: number;
  kind: "leaf" | "interior";
  /** Bytes in use out of 4096. */
  used: number;
  keys: KeyInfo[];
  children: number[];
  next: number | null;
  overflow: { cell: number; bytes: number; pages: number[] }[];
}

export interface TreeInfo {
  name: string;
  kind: "table" | "index";
  root: number;
  depth: number;
  /** Level by level from the root, left to right. */
  pages: PageInfo[];
  truncated: boolean;
  file_pages: number;
  free_pages: number;
}

export const PAGE_SIZE = 4096;

export interface Engine {
  /** Runs a script. With `trace`, every query also says what each step did with each row. */
  run(sql: string, trace?: boolean): RunOutput;
  inspect(name: string): { tree: TreeInfo } | { error: string };
  reset(): void;
}

// Instantiate the module once. Two overlapping `init()` calls (React runs
// mount effects twice in development) would create two instances, and an
// engine made in one would then be called through the other's memory.
let loaded: ReturnType<typeof init> | undefined;

export async function createEngine(): Promise<Engine> {
  loaded ??= init();
  await loaded;
  let db = new Fishbowl();
  return {
    run(sql, trace = false) {
      const output: RunOutput = JSON.parse(db.run(sql, trace));
      return output;
    },
    inspect(name) {
      return JSON.parse(db.inspect(name));
    },
    reset() {
      db.free();
      db = new Fishbowl();
    },
  };
}
