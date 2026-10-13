import { type ReactNode, useEffect, useRef, useState } from "react";
import markUrl from "../../brand/favicon.svg";
import { Heading } from "./components/Heading";
import { PageViewer } from "./components/PageViewer";
import { SchemaList } from "./components/SchemaList";
import { StatementLog } from "./components/StatementLog";
import { TraceView } from "./components/TraceView";
import { createEngine, type Engine, type Entry, type TableInfo } from "./engine";

// Three small tables: fish to filter and look up, tanks to join them to, and
// enough feedings to fill several B-tree pages, one of them with a note too
// big for its leaf. ANALYZE counts them, so the planner's estimates are real.
const SEED = `CREATE TABLE tank (id INTEGER PRIMARY KEY, name TEXT, litres INTEGER);
INSERT INTO tank VALUES (1, 'reef', 300), (2, 'pond', 2000), (3, 'bowl', 4);
CREATE TABLE fish (id INTEGER PRIMARY KEY, name TEXT, species TEXT, weight REAL, tank INTEGER);
INSERT INTO fish VALUES
  (1, 'goldie', 'goldfish', 0.2, 3),
  (2, 'bubbles', 'koi', 1.5, 2),
  (3, 'nemo', 'clownfish', NULL, 1),
  (4, 'dory', 'tang', 0.6, 1),
  (5, 'wanda', 'goldfish', 0.3, 3),
  (6, 'koko', 'koi', 2.4, 2),
  (7, 'finn', 'betta', 0.1, 3),
  (8, 'pearl', 'koi', NULL, 2);
CREATE INDEX fish_species ON fish (species);
CREATE TABLE feeding (id INTEGER PRIMARY KEY, fish INTEGER, grams REAL, note TEXT);
INSERT INTO feeding (fish, grams, note)
  SELECT a.id, b.id * 0.5, 'flakes from ' || c.name FROM fish a, fish b, fish c;
UPDATE feeding SET note = replace(replace(replace('xxxxxxxx', 'x', 'yyyyyyyy'), 'y', 'zzzzzzzz'), 'z', 'krill ')
  WHERE id = 100;
CREATE INDEX feeding_fish ON feeding (fish);
ANALYZE;`;

const EXAMPLES: Record<string, string> = {
  index: "SELECT name, weight\nFROM fish\nWHERE species = 'koi' AND weight > 1;",
  scan: "SELECT name, weight\nFROM fish\nWHERE weight IS NULL OR weight < 0.25;",
  rowid: "SELECT id, fish, grams\nFROM feeding\nWHERE id BETWEEN 200 AND 220;",
  join: "SELECT fish.name, tank.name\nFROM fish JOIN tank ON fish.tank = tank.id;",
  "join order":
    "SELECT tank.name, fish.name, feeding.grams\nFROM feeding\n  JOIN fish ON feeding.fish = fish.id\n  JOIN tank ON fish.tank = tank.id\nWHERE tank.name = 'reef' AND feeding.grams > 3.5;",
  "sorted by index": "SELECT id, fish, grams\nFROM feeding\nORDER BY id\nLIMIT 5;",
  "left join":
    "SELECT tank.name, fish.name\nFROM tank LEFT JOIN fish\n  ON fish.tank = tank.id AND fish.weight > 2;",
  correlated:
    "SELECT name, species, weight\nFROM fish AS f\nWHERE weight = (\n  SELECT max(weight) FROM fish WHERE species = f.species\n);",
  groups:
    "SELECT species, count(*), avg(weight)\nFROM fish\nGROUP BY species\nHAVING count(*) > 1\nORDER BY 2 DESC;",
  "top 3": "SELECT name, weight\nFROM fish\nORDER BY weight DESC\nLIMIT 3;",
  distinct: "SELECT DISTINCT species FROM fish;",
  union: "SELECT name FROM fish WHERE tank = 1\nUNION\nSELECT name FROM tank;",
  unsupported: "WITH heavy AS (SELECT * FROM fish WHERE weight > 1)\nSELECT name FROM heavy;",
};

const START = EXAMPLES.index ?? "";

type View = "trace" | "pages";

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="flex min-h-0 flex-col gap-2">
      <Heading>{title}</Heading>
      {children}
    </section>
  );
}

export function App() {
  const [engine, setEngine] = useState<Engine | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [sql, setSql] = useState(START);
  const [log, setLog] = useState<Entry[]>([]);
  const [tables, setTables] = useState<TableInfo[]>([]);
  const [selected, setSelected] = useState<number | null>(null);
  const [view, setView] = useState<View>("trace");
  // Goes up after every run, so the page viewer reads its tree again.
  const [version, setVersion] = useState(0);
  const [runError, setRunError] = useState<string | null>(null);
  const logRef = useRef<Entry[]>([]);

  function run(on: Engine, text: string, previous: Entry[]) {
    const output = on.run(text, true);
    const next = [...previous, ...output.statements];
    logRef.current = next;
    setLog(next);
    setTables(output.tables);
    setVersion((v) => v + 1);
    const failed = output.statements.find((e) => e.status === "error");
    setRunError(failed?.status === "error" ? failed.message : null);
    const lastQuery = next.findLastIndex((e) => e.status === "ok" && e.result.kind === "query");
    setSelected(lastQuery === -1 ? null : lastQuery);
    return output;
  }

  // `run` reads only its arguments and setters, so loading once on mount is correct.
  // biome-ignore lint/correctness/useExhaustiveDependencies: mount-only effect
  useEffect(() => {
    createEngine()
      .then((loaded) => {
        setEngine(loaded);
        run(loaded, `${SEED}\n${START}`, []);
      })
      .catch((e: unknown) => setLoadError(String(e)));
  }, []);

  // A query switches to its trace; a write stays put, so the page viewer
  // can show what it changed.
  function submit(text: string) {
    if (!engine) return;
    const output = run(engine, text, logRef.current);
    if (output.statements.some((e) => e.status === "ok" && e.result.kind === "query")) {
      setView("trace");
    }
  }

  function reset() {
    if (!engine) return;
    engine.reset();
    setSql(START);
    run(engine, `${SEED}\n${START}`, []);
  }

  const entry = selected === null ? undefined : log[selected];
  const query = entry?.status === "ok" && entry.result.kind === "query" ? entry.result : null;

  return (
    <div className="flex min-h-full flex-col lg:h-full">
      <header className="flex h-11 shrink-0 items-center gap-2.5 border-b border-white/15 px-4">
        <img src={markUrl} alt="" className="size-5" />
        <h1 className="font-semibold tracking-tight">fishbowl</h1>
        <span className="hidden text-white/60 sm:inline">A SQL database you can see inside.</span>
        <nav className="-mr-2 ml-auto flex h-full font-mono">
          {(["trace", "pages"] as const).map((v) => (
            <button
              key={v}
              type="button"
              onClick={() => setView(v)}
              className={`px-2 ${v === view ? "text-white underline underline-offset-4" : "text-white/60"}`}
            >
              {v}
            </button>
          ))}
          {/* The docs site is deployed next to this app, under docs/. */}
          <a href="docs/" className="flex items-center px-2 text-white/60 hover:text-white">
            docs
          </a>
          <a
            href="https://github.com/ToxFied/Fishbowl"
            className="flex items-center px-2 text-white/60 hover:text-white"
          >
            github
          </a>
        </nav>
      </header>

      <main className="flex min-h-0 flex-1 flex-col lg:grid lg:grid-cols-[minmax(0,5fr)_minmax(0,7fr)]">
        <div className="contents lg:flex lg:min-h-0 lg:flex-col lg:overflow-auto lg:border-r lg:border-white/15">
          <div className="order-1 flex flex-col gap-2 p-4 pb-2 lg:order-none lg:pb-4">
            <textarea
              value={sql}
              onChange={(e) => setSql(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
                  e.preventDefault();
                  submit(sql);
                }
              }}
              spellCheck={false}
              autoCapitalize="none"
              autoCorrect="off"
              rows={5}
              aria-label="SQL"
              className="w-full resize-y border border-white/20 bg-black p-2 font-mono leading-relaxed outline-none focus:border-white pointer-coarse:text-base"
            />
            <div className="flex flex-wrap items-center gap-x-3 gap-y-1.5 font-mono pointer-coarse:gap-y-0">
              <button
                type="button"
                onClick={() => submit(sql)}
                disabled={!engine}
                className="border border-white px-3 py-1 disabled:border-white/34 disabled:text-white/34 pointer-coarse:min-h-11"
              >
                Run<span className="pointer-coarse:hidden"> ⌘↵</span>
              </button>
              <button type="button" onClick={reset} className="py-1 pointer-coarse:min-h-11">
                Reset
              </button>
              {Object.entries(EXAMPLES).map(([name, text]) => (
                <button
                  key={name}
                  type="button"
                  onClick={() => {
                    setSql(text);
                    submit(text);
                  }}
                  className="py-1 text-white/60 hover:text-white pointer-coarse:min-h-11"
                >
                  {name}
                </button>
              ))}
            </div>
            {loadError && <p>Engine failed to load: {loadError}</p>}
          </div>

          <div className="order-3 flex flex-col gap-5 p-4 lg:order-none">
            <Section title="Log">
              <StatementLog entries={log} selected={selected} onSelect={setSelected} />
            </Section>
            <Section title="Schema">
              <SchemaList tables={tables} />
            </Section>
          </div>
        </div>

        <div className="order-2 flex min-h-0 flex-col gap-6 border-y border-white/15 p-4 lg:order-none lg:overflow-auto lg:border-y-0">
          {runError && <p className="font-mono">error: {runError}</p>}
          {view === "pages" && engine ? (
            <PageViewer engine={engine} tables={tables} version={version} onTables={setTables} />
          ) : query?.trace ? (
            <TraceView key={selected} result={query} autoplay />
          ) : (
            <p className="text-white/60">Run a SELECT to watch it.</p>
          )}
        </div>
      </main>
    </div>
  );
}
