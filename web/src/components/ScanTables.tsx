import type { ScanRows } from "../engine";
import { type Frame, formatValue, type Step } from "../trace";

interface Props {
  steps: Step[];
  scans: ScanRows[];
  frame: Frame;
  /** Table rows register here, so a row can rise out of its table into the plan. */
  register: (key: string, el: HTMLElement | null) => void;
}

function insideSubquery(step: Step): boolean {
  for (let up = step.parent; up; up = up.parent) {
    if (up.node.operator === "Subquery") return true;
  }
  return false;
}

/** What a scan read, and what it never touched, with an index or without one. */
function Summary({ step, scan, frame }: { step: Step; scan: ScanRows; frame: Frame }) {
  const distinct = frame.read.get(scan.step)?.size ?? 0;
  const reads = frame.steps[scan.step]?.out ?? 0;
  const done = frame.position > 0 && reads === step.node.rows_out;
  const skipped = scan.row_count - distinct;
  return (
    <span className="tabular-nums text-white/60">
      read {distinct} of {scan.row_count}
      {reads > distinct && ` (${reads} reads)`}
      {done && skipped > 0 && (
        <span className="text-white">
          {" "}
          · never touched {skipped}
          {step.node.operator === "IndexScan" && `, a SeqScan would read all ${scan.row_count}`}
        </span>
      )}
    </span>
  );
}

/**
 * The tables the scans read, at the bottom of the flow. Each row is dim until
 * a scan reads it, orange while it is being read, white once read. What stays
 * dim at the end is what an index let the query skip.
 */
export function ScanTables({ steps, scans, frame, register }: Props) {
  const current = frame.current;
  return (
    <div className="flex flex-col gap-4">
      {scans.map((scan) => {
        const step = steps[scan.step];
        if (!step) return null;
        const read = frame.read.get(scan.step);
        const now = current?.step === scan.step ? current.rowid : undefined;
        const tone = (rowid: number) =>
          rowid === now ? "text-fish" : read?.has(rowid) ? "text-white" : "text-white/34";
        return (
          <div key={scan.step} className="flex flex-col gap-1.5 font-mono">
            <p className="flex flex-wrap gap-x-3">
              <span className={current?.step === scan.step ? "text-fish" : ""}>
                {scan.table}
                {insideSubquery(step) && " (subquery)"}
              </span>
              <Summary step={step} scan={scan} frame={frame} />
            </p>
            {scan.rows.length > 0 ? (
              <div className="overflow-x-auto">
                <table className="border-collapse">
                  <tbody>
                    {scan.rows.map((row, i) => {
                      const rowid = scan.rowids[i] ?? i;
                      return (
                        <tr
                          key={rowid}
                          ref={(el) => register(`${scan.step}:${rowid}`, el)}
                          className={tone(rowid)}
                        >
                          <td className="pr-4 text-right tabular-nums">#{rowid}</td>
                          {row.map((value, c) => (
                            <td
                              // biome-ignore lint/suspicious/noArrayIndexKey: fixed column order
                              key={c}
                              className={`max-w-40 truncate pr-4 ${value.type === "text" ? "" : "text-right tabular-nums"}`}
                            >
                              {formatValue(value)}
                            </td>
                          ))}
                        </tr>
                      );
                    })}
                  </tbody>
                </table>
              </div>
            ) : (
              <div
                className="flex flex-wrap gap-[2px]"
                role="img"
                aria-label="rows read and skipped"
              >
                {scan.rowids.map((rowid) => (
                  <span
                    key={rowid}
                    title={`rowid ${rowid}`}
                    className={`h-2.5 w-1.5 ${
                      rowid === now ? "bg-fish" : read?.has(rowid) ? "bg-white" : "bg-white/34"
                    }`}
                  />
                ))}
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
}
