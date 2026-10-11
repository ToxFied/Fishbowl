// Playback of a row-level trace. A position `p` means the first `p` events
// have happened; event `p - 1` is the one happening now. Everything here is
// derived by replaying events from the start, which is cheap at the engine's
// 5,000-event cap and keeps stepping back exactly as correct as stepping on.
import type { PlanProfile, Row, SqlValue, Trace, TraceEvent } from "./engine";

export interface Step {
  node: PlanProfile;
  depth: number;
  parent: Step | null;
  /** The steps that pass rows up to this one. Subqueries hand over values, not rows. */
  inputs: Step[];
}

/** Steps in pre-order, so `steps[id]` is the step with that id. */
export function flatten(plan: PlanProfile): Step[] {
  const steps: Step[] = [];
  const visit = (node: PlanProfile, depth: number, parent: Step | null) => {
    const step: Step = { node, depth, parent, inputs: [] };
    steps[node.id] = step;
    for (const child of node.children) {
      const sub = visit(child, depth + 1, step);
      if (child.operator !== "Subquery") step.inputs.push(sub);
    }
    return step;
  };
  visit(plan, 0, null);
  return steps;
}

export interface StepState {
  out: number;
  dropped: number;
  /** Subquery runs, and answers handed back without running. */
  runs: number;
  reuses: number;
  last: TraceEvent | null;
  /** For a join: which left row it is on, and which right row it tried last. */
  pair: [number, number] | null;
  /** For a join: how many values of a pair come from the left row. */
  leftWidth: number;
  /** For a join: a right row arrived that has not been ruled on yet. */
  fresh: boolean;
  /** For a join: the last row out was a LEFT JOIN row padded with NULLs. */
  padded: boolean;
}

export interface Frame {
  position: number;
  current: TraceEvent | null;
  steps: StepState[];
  /** Rows the root has passed on: the result so far. */
  result: Row[];
  /** Rowids each scan step has read so far. */
  read: Map<number, Set<number>>;
}

export function frameAt(trace: Trace, steps: Step[], position: number): Frame {
  const state: StepState[] = steps.map(() => ({
    out: 0,
    dropped: 0,
    runs: 0,
    reuses: 0,
    last: null,
    pair: null,
    leftWidth: 0,
    fresh: false,
    padded: false,
  }));
  const result: Row[] = [];
  const read = new Map<number, Set<number>>();
  for (const event of trace.events.slice(0, position)) {
    const s = state[event.step];
    const step = steps[event.step];
    if (!s || !step) continue;
    s.last = event;
    if (event.kind === "out") s.out++;
    else if (event.kind === "drop") s.dropped++;
    else if (event.kind === "run") s.runs++;
    else s.reuses++;
    if (step.node.operator === "NestedLoopJoin") {
      s.padded = event.kind === "out" && !s.fresh;
      s.fresh = false;
    }
    if (event.kind === "out") {
      if (step.parent === null) result.push(event.row);
      if (event.rowid !== undefined) {
        const set = read.get(event.step) ?? new Set<number>();
        set.add(event.rowid);
        read.set(event.step, set);
      }
      // A join counts the rows its two inputs hand it.
      const parent = step.parent;
      const ps = parent ? state[parent.node.id] : undefined;
      if (parent && ps && parent.node.operator === "NestedLoopJoin") {
        const [left, right] = ps.pair ?? [0, 0];
        if (parent.inputs[0] === step) {
          ps.pair = [left + 1, 0];
          ps.leftWidth = event.row.length;
        } else {
          ps.pair = [left, right + 1];
          ps.fresh = true;
        }
      }
    }
  }
  const current = position > 0 ? (trace.events[position - 1] ?? null) : null;
  return { position, current, steps: state, result, read };
}

export function formatValue(value: SqlValue): string {
  if (value.type === "null") return "NULL";
  return value.type === "text" && value.text.length > 18
    ? `${value.text.slice(0, 17)}…`
    : value.text;
}

/** One short line saying what the current event is. */
export function narrate(frame: Frame, steps: Step[]): string {
  const event = frame.current;
  if (!event) return "";
  const step = steps[event.step];
  const s = frame.steps[event.step];
  if (!step || !s) return "";
  const op = step.node.operator;
  const toResult = step.parent === null ? ", into the result" : "";
  switch (event.kind) {
    case "run":
      return step.node.detail.includes("correlated")
        ? `Subquery runs again (run ${s.runs}) for this outer row`
        : "Subquery runs once; later rows reuse its answer";
    case "reuse":
      return "Subquery reuses its first answer";
    case "drop":
      switch (op) {
        case "Filter":
          return "Filter drops it: the condition is not true";
        case "NestedLoopJoin":
          return `Join tries left row ${s.pair?.[0]} with right row ${s.pair?.[1]}: no match`;
        case "Distinct":
          return "Distinct drops it: already seen";
        case "Limit":
          return "Limit skips it for OFFSET";
        default:
          return `${op} drops it`;
      }
    case "out":
      break;
  }
  switch (op) {
    case "SeqScan":
      return `SeqScan reads rowid ${event.rowid} of ${step.node.detail}`;
    case "IndexScan":
      return step.node.detail.includes("COVERING INDEX")
        ? `IndexScan reads rowid ${event.rowid} from the index alone; columns it does not hold stay NULL`
        : `IndexScan jumps to rowid ${event.rowid}`;
    case "Filter":
      return `Filter keeps it${toResult}`;
    case "Project":
      return `Project computes the output columns${toResult}`;
    case "NestedLoopJoin": {
      const pair = s.pair ? `left row ${s.pair[0]} with right row ${s.pair[1]}` : "";
      return s.padded
        ? `Join found no match for left row ${s.pair?.[0]}, so it pads it with NULLs`
        : `Join tries ${pair}: match${toResult}`;
    }
    case "Materialize": {
      const kept = Math.max(1, step.inputs[0]?.node.rows_out ?? 1);
      const row = ((s.out - 1) % kept) + 1;
      return s.out > kept
        ? `Materialize replays row ${row} from memory for the next left row`
        : `Materialize keeps every row in memory and hands out row ${row}`;
    }
    case "Sort":
      return `Sort has read every row and hands out number ${s.out} in order${toResult}`;
    case "Aggregate":
      return `Aggregate finishes a group${toResult}`;
    case "Distinct":
      return `Distinct sees it for the first time${toResult}`;
    case "Limit":
      return `Limit passes on row ${s.out}${toResult}`;
    case "OneRow":
      return "OneRow makes one empty row to compute on";
    default:
      return `${op} passes it on${toResult}`;
  }
}

/** Milliseconds per event, so short traces read slowly and long ones still finish. */
export function pace(events: number, fast: boolean): number {
  const base = Math.min(900, Math.max(110, 14000 / Math.max(1, events)));
  return fast ? Math.max(40, base / 4) : base;
}
