import type { Entry } from "../engine";

function summary(entry: Entry): string {
  if (entry.status === "error") return entry.message;
  const { result } = entry;
  switch (result.kind) {
    case "create_table":
      return `created ${result.table}`;
    case "insert":
      return `inserted ${result.rows}`;
    case "update":
      return `updated ${result.rows}`;
    case "delete":
      return `deleted ${result.rows}`;
    case "drop_table":
      return `dropped ${result.table}`;
    case "create_view":
      return `created view ${result.view}`;
    case "drop_view":
      return `dropped view ${result.view}`;
    case "create_index":
      return `created ${result.index}`;
    case "drop_index":
      return `dropped ${result.index}`;
    case "analyze":
      return `analyzed ${result.tables.join(", ")}`;
    case "begin":
    case "commit":
    case "rollback":
    case "reindex":
      return result.kind;
    case "query":
      return `${result.rows.length} ${result.rows.length === 1 ? "row" : "rows"}`;
  }
}

interface Props {
  entries: Entry[];
  selected: number | null;
  onSelect: (index: number) => void;
}

export function StatementLog({ entries, selected, onSelect }: Props) {
  return (
    <ol className="flex flex-col font-mono">
      {entries.map((entry, i) => {
        const isQuery = entry.status === "ok" && entry.result.kind === "query";
        return (
          // The log only grows, so a position never changes meaning.
          // biome-ignore lint/suspicious/noArrayIndexKey: append-only list
          <li key={i}>
            <button
              type="button"
              disabled={!isQuery}
              onClick={() => onSelect(i)}
              className="grid w-full grid-cols-[1ch_1fr_auto] gap-2 py-0.5 text-left"
            >
              <span className="text-fish">{i === selected ? "›" : ""}</span>
              <span className="truncate">{entry.sql}</span>
              <span className={entry.status === "error" ? "text-white" : "text-white/34"}>
                {entry.status === "error" ? `error: ${summary(entry)}` : summary(entry)}
              </span>
            </button>
          </li>
        );
      })}
    </ol>
  );
}
