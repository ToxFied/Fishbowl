import type { Row, SqlValue } from "../engine";

/** More rows than this are counted, not drawn. */
const SHOWN = 200;

function Cell({ value }: { value: SqlValue }) {
  if (value.type === "null") return <span className="opacity-34">NULL</span>;
  if (value.type === "text")
    return <>{value.text === "" ? <span className="opacity-34">''</span> : value.text}</>;
  return <>{value.text}</>;
}

interface Props {
  columns: string[];
  /** The rows produced so far. */
  rows: Row[];
  /** How many rows the query returns in the end. */
  total: number;
  /** The last row arrived just now. */
  fresh: boolean;
  /** Receives the last row's element, so the trace can move it into place. */
  lastRow?: (el: HTMLElement | null) => void;
}

export function ResultTable({ columns, rows, total, fresh, lastRow }: Props) {
  return (
    <div className="max-h-56 overflow-auto">
      <table className="min-w-64 border-collapse font-mono">
        <thead>
          <tr className="border-b border-white/15 text-left">
            {columns.map((column, i) => (
              // Columns can repeat (SELECT a, a), so position is the stable key.
              // biome-ignore lint/suspicious/noArrayIndexKey: see above
              <th key={i} className="py-1 pr-6 font-medium">
                {column}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.slice(0, SHOWN).map((row, r) => (
            <tr
              // Rows have no identity of their own yet; position is all there is.
              // biome-ignore lint/suspicious/noArrayIndexKey: see above
              key={r}
              ref={r === rows.length - 1 ? lastRow : undefined}
              className={`border-b border-white/8 ${fresh && r === rows.length - 1 ? "text-fish" : ""}`}
            >
              {row.map((value, c) => (
                <td
                  // biome-ignore lint/suspicious/noArrayIndexKey: fixed column order
                  key={c}
                  className={`py-0.5 pr-6 ${value.type === "text" ? "" : "text-right tabular-nums"}`}
                >
                  <Cell value={value} />
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
      <p className="pt-1 font-mono tabular-nums">
        {rows.length === total ? total : `${rows.length} of ${total}`}{" "}
        {total === 1 ? "row" : "rows"}
      </p>
    </div>
  );
}
