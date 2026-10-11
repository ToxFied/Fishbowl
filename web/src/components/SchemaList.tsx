import type { TableInfo } from "../engine";

export function SchemaList({ tables }: { tables: TableInfo[] }) {
  if (tables.length === 0) return <p className="text-white/34">No tables.</p>;
  return (
    <ul className="flex flex-col gap-2 font-mono">
      {tables.map((table) => (
        <li key={table.name}>
          <span>{table.name}</span>
          <span className="tabular-nums text-white/34"> {table.row_count} rows</span>
          <ul className="pl-4 text-white/60">
            {table.columns.map((column) => (
              <li key={column.name}>
                {column.name}{" "}
                <span className="text-white/34">{column.declared_type || "(no type)"}</span>
                {column.primary_key && <span className="text-white/34"> PRIMARY KEY</span>}
                {column.not_null && <span className="text-white/34"> NOT NULL</span>}
              </li>
            ))}
            {table.indexes.map((index) => {
              const columns = index.columns.map((c) => (c.descending ? `${c.name} DESC` : c.name));
              return (
                <li key={`index ${index.name}`}>
                  <span className="text-white/34">{index.unique ? "unique index " : "index "}</span>
                  {index.name} <span className="text-white/34">({columns.join(", ")})</span>
                </li>
              );
            })}
          </ul>
        </li>
      ))}
    </ul>
  );
}
