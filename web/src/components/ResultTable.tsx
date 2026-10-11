import type { QueryResult, SqlValue } from "../engine";

function Cell({ value }: { value: SqlValue }) {
  if (value.type === "null") return <span className="text-white/34">NULL</span>;
  if (value.type === "text")
    return <>{value.text === "" ? <span className="text-white/34">''</span> : value.text}</>;
  return <>{value.text}</>;
}

export function ResultTable({ result }: { result: QueryResult }) {
  return (
    <div className="overflow-auto">
      <table className="min-w-64 border-collapse font-mono">
        <thead>
          <tr className="border-b border-white/15 text-left">
            {result.columns.map((column, i) => (
              // Columns can repeat (SELECT a, a), so position is the stable key.
              // biome-ignore lint/suspicious/noArrayIndexKey: see above
              <th key={i} className="py-1 pr-6 font-medium">
                {column}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {result.rows.map((row, r) => (
            // Rows have no identity of their own yet; position is all there is.
            // biome-ignore lint/suspicious/noArrayIndexKey: see above
            <tr key={r} className="border-b border-white/8">
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
        {result.rows.length} {result.rows.length === 1 ? "row" : "rows"}
      </p>
    </div>
  );
}
