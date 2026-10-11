import type { PlanProfile } from "../engine";
import type { Frame, Step } from "../trace";
import { Heading } from "./Heading";

/**
 * The planner's reason for each step where it had a choice. Where it weighed
 * costed plans, the one it chose sits above the ones it turned down, with the
 * rows and work it expected of each.
 */
export function WhyPanel({ steps, frame }: { steps: Step[]; frame: Frame }) {
  const explained = steps.filter((s) => s.node.reason);
  if (explained.length === 0) return null;
  return (
    <section className="flex flex-col gap-3">
      <Heading>Why this plan</Heading>
      {explained.map(({ node }) => (
        <div key={node.id} className="flex flex-col gap-1.5">
          <p>
            <span className={`font-mono ${frame.current?.step === node.id ? "text-fish" : ""}`}>
              {node.operator}
            </span>{" "}
            <span className="text-white/60">{node.reason}</span>
          </p>
          <Choices node={node} />
        </div>
      ))}
    </section>
  );
}

/** The chosen plan and the rejected ones, cheapest first, with their estimates. */
function Choices({ node }: { node: PlanProfile }) {
  const { estimate, plan } = node;
  if (!estimate || !plan || !node.rejected?.length) return null;
  const chosen = { plan, rows: estimate.rows, cost: estimate.cost, chosen: true };
  const rows = [chosen, ...(node.rejected ?? []).map((r) => ({ ...r, chosen: false }))];
  const best = estimate.cost;
  return (
    <table className="w-full table-fixed font-mono text-[12px] leading-5">
      <thead className="text-white/60">
        <tr>
          <th className="text-left font-normal">plan</th>
          <th className="w-20 text-right font-normal">est. rows</th>
          <th className="w-20 text-right font-normal">cost</th>
          <th className="w-14 text-right font-normal" />
        </tr>
      </thead>
      <tbody>
        {rows.map((r) => (
          <tr key={r.plan} className={r.chosen ? "text-fish" : "text-white/60"}>
            <td className="truncate pr-3" title={r.plan}>
              {r.plan}
            </td>
            <td className="text-right tabular-nums">{compact(r.rows)}</td>
            <td className="text-right tabular-nums">{compact(r.cost)}</td>
            <td className="text-right tabular-nums">
              {r.chosen ? "chosen" : best > 0 ? times(r.cost / best) : ""}
            </td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

/** 7, 2.5k, 3.1M: a count short enough for a narrow column. */
function compact(n: number): string {
  if (n < 1000) return n < 10 ? n.toFixed(n % 1 === 0 ? 0 : 1) : Math.round(n).toString();
  const units: [number, string][] = [
    [1e12, "T"],
    [1e9, "B"],
    [1e6, "M"],
    [1e3, "k"],
  ];
  const [size, unit] = units.find(([size]) => n >= size) ?? [1, ""];
  const value = n / size;
  return `${value < 10 ? value.toFixed(1) : Math.round(value)}${unit}`;
}

/** How many times the chosen plan's cost a rejected one would have been. */
function times(ratio: number): string {
  if (ratio >= 1000) return `×${compact(ratio)}`;
  return `×${ratio < 10 ? ratio.toFixed(1) : Math.round(ratio)}`;
}
