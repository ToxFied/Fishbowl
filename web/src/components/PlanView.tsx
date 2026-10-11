import { useState } from "react";
import type { PlanProfile } from "../engine";

// Plain-English notes shown for the selected step.
const OPERATORS: Record<string, string> = {
  SeqScan: "Reads every row of the table, one at a time, in storage order.",
  Filter: "Passes on only the rows where the condition is true. NULL counts as not true.",
  Project: "Computes the output columns from each row it receives.",
  OneRow: "Produces one empty row, so a SELECT without FROM has something to compute on.",
  Sort: "Reads every row it is given, then hands them up in order. It cannot return its first row before it has seen the last.",
  Aggregate:
    "Sorts rows into groups with equal GROUP BY values and computes count(), sum() and the rest for each group. One row comes out per group.",
  Distinct: "Passes on each row only the first time it sees it. NULL counts as equal to NULL here.",
  Limit:
    "Skips the OFFSET rows, passes on rows until it has enough, then stops asking its input for more.",
  NestedLoopJoin:
    "For each row from its first input, reads all of its second input again and passes on the pairs that meet the condition. A LEFT join also passes on unmatched left rows, padded with NULLs.",
  Materialize:
    "Reads its input once and keeps the rows, so the join above can read them again for every left row without scanning the table again.",
  Subquery:
    "A query inside an expression. Its count is how many times it ran: once if it reads nothing from the outer row, once per row if it does.",
  SubqueryScan:
    "Passes on the rows of a view or a subquery in FROM, under the name the query uses.",
  Union: "Reads both inputs and returns each distinct row once, sorted.",
  UnionAll: "Passes on every row of its first input, then every row of its second.",
  Intersect: "Returns the distinct rows that appear in both inputs, sorted.",
  Except: "Returns the distinct rows of its first input that are not in its second, sorted.",
};

interface Line {
  node: PlanProfile;
  depth: number;
}

/** The tree top-down, the way it is drawn. */
function drawOrder(node: PlanProfile, depth = 0): Line[] {
  return [{ node, depth }, ...node.children.flatMap((child) => drawOrder(child, depth + 1))];
}

/** Inputs before the step that consumes them: the order rows start flowing. */
function runOrder(node: PlanProfile): PlanProfile[] {
  return [...node.children.flatMap(runOrder), node];
}

/** Rows that flowed in. A subquery hands a step values, not rows. */
function rowsIn(node: PlanProfile): number | null {
  const inputs = node.children.filter((child) => child.operator !== "Subquery");
  if (inputs.length === 0) return null;
  return inputs.reduce((sum, child) => sum + child.rows_out, 0);
}

export function PlanView({ plan }: { plan: PlanProfile }) {
  const lines = drawOrder(plan);
  const steps = runOrder(plan);
  const [step, setStep] = useState(0);
  const current = steps[step] ?? plan;
  const widest = Math.max(1, ...lines.map((line) => line.node.rows_out));
  const input = rowsIn(current);

  return (
    <div className="flex flex-col gap-3">
      <div className="font-mono">
        {lines.map(({ node, depth }) => {
          const active = node.id === current.id;
          return (
            <button
              key={node.id}
              type="button"
              onClick={() => setStep(steps.indexOf(node))}
              className="grid w-full grid-cols-[1fr_4rem_6rem] items-center gap-3 py-0.5 text-left"
            >
              <span className="truncate" style={{ paddingLeft: `${depth * 1.25}rem` }}>
                {depth > 0 && <span className="text-white/34">└ </span>}
                <span className={active ? "text-fish" : "text-white"}>{node.operator}</span>
                {node.detail && <span className="text-white/60"> {node.detail}</span>}
              </span>
              <span className={`text-right tabular-nums ${active ? "text-fish" : ""}`}>
                {node.rows_out}
              </span>
              <span className="h-1 bg-white/15">
                <span
                  className={`block h-full ${active ? "bg-fish" : "bg-white"}`}
                  style={{ width: `${(node.rows_out / widest) * 100}%` }}
                />
              </span>
            </button>
          );
        })}
      </div>

      <div className="flex items-baseline gap-3 border-t border-white/15 pt-2">
        <button
          type="button"
          className="font-mono disabled:text-white/34"
          disabled={step === 0}
          onClick={() => setStep(step - 1)}
        >
          ← prev
        </button>
        <span className="font-mono tabular-nums">
          step {step + 1}/{steps.length}
        </span>
        <button
          type="button"
          className="font-mono disabled:text-white/34"
          disabled={step === steps.length - 1}
          onClick={() => setStep(step + 1)}
        >
          next →
        </button>
        <span className="ml-auto font-mono tabular-nums">
          {input === null ? "" : `${input} in → `}
          <span className="text-fish">{current.rows_out} out</span>
        </span>
      </div>
      <p>
        <span className="font-mono text-fish">{current.operator}</span>{" "}
        {OPERATORS[current.operator] ?? ""}
        {current.reason && <span className="block text-white/60">Why: {current.reason}.</span>}
      </p>
    </div>
  );
}
