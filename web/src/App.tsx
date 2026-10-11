import { type ReactNode, useEffect, useRef, useState } from "react";
import markUrl from "../../brand/favicon.svg";
import { PlanView } from "./components/PlanView";
import { ResultTable } from "./components/ResultTable";
import { SchemaList } from "./components/SchemaList";
import { StatementLog } from "./components/StatementLog";
import { createEngine, type Engine, type Entry, type TableInfo } from "./engine";

const SEED = `CREATE TABLE fish (id INTEGER, name TEXT, species TEXT, weight REAL);
INSERT INTO fish VALUES
  (1, 'goldie', 'goldfish', 0.2),
  (2, 'bubbles', 'koi', 1.5),
  (3, 'nemo', 'clownfish', NULL),
  (4, 'dory', 'tang', 0.6),
  (5, 'wanda', 'goldfish', 0.3),
  (6, 'koko', 'koi', 2.4),
  (7, 'finn', 'betta', 0.1),
  (8, 'pearl', 'koi', NULL);`;

const EXAMPLES: Record<string, string> = {
  filter: "SELECT name, weight * 1000 AS grams\nFROM fish\nWHERE species = 'koi' AND weight > 1;",
  nulls: "SELECT name, weight\nFROM fish\nWHERE weight IS NULL OR weight < 0.25;",
  expressions: "SELECT 7 / 2, 7 % 2, 7.0 / 2, 'gold' || 'fish', NULL + 1;",
  error: "SELECT name FROM fish ORDER BY weight;",
};

const START = EXAMPLES.filter ?? "";

function Section({
  title,
  children,
  className = "",
}: {
  title: string;
  children: ReactNode;
  className?: string;
}) {
  return (
    <section className={`flex min-h-0 flex-col gap-2 ${className}`}>
      <h2 className="font-mono text-[11px] uppercase tracking-[0.12em]">{title}</h2>
      {children}
    </section>
  );
}

export function App() {
  const engine = useRef<Engine | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [sql, setSql] = useState(START);
  const [log, setLog] = useState<Entry[]>([]);
  const [tables, setTables] = useState<TableInfo[]>([]);
  const [selected, setSelected] = useState<number | null>(null);

  function run(text: string, previous: Entry[]) {
    if (!engine.current) return;
    const output = engine.current.run(text);
    const next = [...previous, ...output.statements];
    setLog(next);
    setTables(output.tables);
    const lastQuery = next.findLastIndex((e) => e.status === "ok" && e.result.kind === "query");
    setSelected(lastQuery === -1 ? null : lastQuery);
  }

  // `run` reads only refs and setters, so loading once on mount is correct.
  // biome-ignore lint/correctness/useExhaustiveDependencies: mount-only effect
  useEffect(() => {
    createEngine()
      .then((loaded) => {
        engine.current = loaded;
        run(`${SEED}\n${START}`, []);
      })
      .catch((e: unknown) => setLoadError(String(e)));
  }, []);

  function reset() {
    engine.current?.reset();
    run(`${SEED}\n${START}`, []);
  }

  const entry = selected === null ? undefined : log[selected];
  const query = entry?.status === "ok" && entry.result.kind === "query" ? entry.result : null;

  return (
    <div className="flex h-full flex-col">
      <header className="flex h-11 shrink-0 items-center gap-2.5 border-b border-white/15 px-4">
        <img src={markUrl} alt="" className="size-5" />
        <h1 className="font-semibold tracking-tight">fishbowl</h1>
        <span className="font-mono text-white/34">in-memory engine · WebAssembly</span>
      </header>

      <main className="grid min-h-0 flex-1 grid-cols-1 lg:grid-cols-[minmax(0,5fr)_minmax(0,7fr)]">
        <div className="flex min-h-0 flex-col gap-5 overflow-auto border-white/15 p-4 lg:border-r">
          <Section title="Query">
            <textarea
              value={sql}
              onChange={(e) => setSql(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
                  e.preventDefault();
                  run(sql, log);
                }
              }}
              spellCheck={false}
              rows={7}
              aria-label="SQL"
              className="w-full resize-y border border-white/20 bg-black p-2 font-mono leading-relaxed outline-none focus:border-white"
            />
            <div className="flex flex-wrap items-center gap-x-4 gap-y-1 font-mono">
              <button
                type="button"
                onClick={() => run(sql, log)}
                disabled={!engine.current}
                className="border border-white px-3 py-1 disabled:border-white/34 disabled:text-white/34"
              >
                Run ⌘↵
              </button>
              <button type="button" onClick={reset}>
                Reset
              </button>
              <span className="ml-auto flex gap-3">
                {Object.entries(EXAMPLES).map(([name, text]) => (
                  <button
                    key={name}
                    type="button"
                    onClick={() => setSql(text)}
                    className="text-white/60 hover:text-white"
                  >
                    {name}
                  </button>
                ))}
              </span>
            </div>
            {loadError && <p>Engine failed to load: {loadError}</p>}
          </Section>

          <Section title="Log">
            <StatementLog entries={log} selected={selected} onSelect={setSelected} />
          </Section>

          <Section title="Schema">
            <SchemaList tables={tables} />
          </Section>
        </div>

        <div className="flex min-h-0 flex-col gap-6 overflow-auto p-4">
          <Section title="Plan">
            {query ? (
              <PlanView key={selected} plan={query.plan} />
            ) : (
              <p className="text-white/34">Run a SELECT to see its plan.</p>
            )}
          </Section>
          {query && (
            <Section title="Result">
              <ResultTable result={query} />
            </Section>
          )}
        </div>
      </main>
    </div>
  );
}
