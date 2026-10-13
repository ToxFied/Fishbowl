import { useEffect, useMemo, useRef, useState } from "react";
import {
  type Engine,
  type KeyInfo,
  PAGE_SIZE,
  type PageInfo,
  type TableInfo,
  type TreeInfo,
} from "../engine";
import { formatValue } from "../trace";
import { Heading } from "./Heading";
import { ScrollX } from "./ScrollX";

function keyText(key: KeyInfo): string {
  if (key.values.length === 0) return String(key.rowid);
  return `${key.values.map(formatValue).join(", ")} #${key.rowid}`;
}

/** A page's contents in short, to tell whether an insert changed it. */
const fingerprint = (p: PageInfo) =>
  `${p.used}:${p.keys.length}:${p.children.join(",")}:${p.next}:${p.overflow.length}`;

interface Change {
  /** Pages that are new or changed, drawn orange. */
  pages: Set<number>;
  /** What happened to the tree's shape, if anything did. */
  split: string | null;
}

function compare(before: TreeInfo, after: TreeInfo): Change {
  const old = new Map(before.pages.map((p) => [p.id, p]));
  const pages = new Set(
    after.pages
      .filter((p) => {
        const was = old.get(p.id);
        return !was || fingerprint(was) !== fingerprint(p);
      })
      .map((p) => p.id),
  );
  const added = after.pages.filter((p) => !old.has(p.id));
  const gone = before.pages.filter((p) => !after.pages.some((q) => q.id === p.id));
  let split: string | null = null;
  if (after.depth > before.depth) {
    const root = after.pages[0];
    split = `Root p${after.root} was full. Its contents moved down into ${root?.children
      .map((c) => `p${c}`)
      .join(" and ")}, and the tree grew to ${after.depth} levels. The root keeps its page number.`;
  } else if (added.length > 0) {
    split = added
      .map((page) => {
        const level = after.pages.filter((p) => p.level === page.level);
        const left = level[level.indexOf(page) - 1];
        const first = page.keys[0];
        const parent = after.pages.find((p) => p.children.includes(page.id));
        return `p${left?.id} was full, so it split: new page p${page.id} takes ${
          first ? `keys from ${keyText(first)}` : "the upper half"
        }${parent ? `, and p${parent.id} gets a signpost to it` : ""}.`;
      })
      .join(" ");
  } else if (after.depth < before.depth || gone.length > 0) {
    split = `Pages ${gone.map((p) => `p${p.id}`).join(", ")} merged away and went to the free list.`;
  }
  return { pages, split };
}

/** Leaves under a page, so each page is as wide as the leaves it covers. */
function leafCount(id: number, byId: Map<number, PageInfo>): number {
  const page = byId.get(id);
  if (!page || page.children.length === 0) return 1;
  return page.children.reduce((sum, c) => sum + leafCount(c, byId), 0);
}

function PageBox({
  page,
  changed,
  selected,
  onSelect,
}: {
  page: PageInfo;
  changed: boolean;
  selected: boolean;
  onSelect: () => void;
}) {
  const fill = page.used / PAGE_SIZE;
  const first = page.keys[0];
  const last = page.keys[page.keys.length - 1];
  const border = changed ? "border-fish" : selected ? "border-white" : "border-white/34";
  return (
    <button
      type="button"
      data-page={page.id}
      onClick={onSelect}
      className={`flex w-full min-w-0 flex-col gap-1 border px-1.5 py-1 text-left font-mono text-[11px] leading-tight ${border}`}
    >
      <span className="flex justify-between gap-1">
        <span className={changed ? "text-fish" : ""}>p{page.id}</span>
        <span className="tabular-nums text-white/60">{page.keys.length}</span>
      </span>
      <span className="h-1 bg-white/15">
        <span
          className={`block h-full ${changed ? "bg-fish" : "bg-white"}`}
          style={{ width: `${Math.min(100, fill * 100)}%` }}
        />
      </span>
      <span className="truncate text-white/60">
        {page.kind === "interior"
          ? `≤ ${page.keys.map(keyText).join(" ≤ ")}`
          : first && last
            ? `${keyText(first)} … ${keyText(last)}`
            : "empty"}
      </span>
    </button>
  );
}

function Subtree(props: {
  id: number;
  byId: Map<number, PageInfo>;
  changed: Set<number>;
  selected: number | null;
  onSelect: (id: number) => void;
}) {
  const { id, byId, changed, selected, onSelect } = props;
  const page = byId.get(id);
  if (!page) return null;
  return (
    <div className="flex min-w-0 flex-col gap-1.5" style={{ flex: `${leafCount(id, byId)} 1 0` }}>
      <PageBox
        page={page}
        changed={changed.has(id)}
        selected={selected === id}
        onSelect={() => onSelect(id)}
      />
      {page.overflow.map((chain) => (
        <span key={chain.cell} className="truncate font-mono text-[11px] text-white/60">
          ↳ {chain.pages.map((p) => `p${p}`).join(" → ")}
        </span>
      ))}
      {page.children.length > 0 && (
        <div className="flex gap-1">
          {page.children.map((child) => (
            <Subtree key={child} {...props} id={child} />
          ))}
        </div>
      )}
    </div>
  );
}

function PageDetail({ page, onSelect }: { page: PageInfo; onSelect: (id: number) => void }) {
  return (
    <div className="flex flex-col gap-1.5 font-mono">
      <p>
        page {page.id} · {page.kind} · {page.used.toLocaleString()} of {PAGE_SIZE.toLocaleString()}{" "}
        bytes ({Math.round((page.used / PAGE_SIZE) * 100)}%)
        {page.next !== null && (
          <>
            {" "}
            · next leaf{" "}
            <button
              type="button"
              className="underline pointer-coarse:px-2 pointer-coarse:py-3"
              onClick={() => onSelect(page.next ?? 0)}
            >
              p{page.next}
            </button>
          </>
        )}
      </p>
      {page.children.length > 0 && (
        <p className="flex flex-wrap gap-x-3 pointer-coarse:gap-x-4">
          {page.children.map((child, i) => {
            const key = page.keys[i];
            return (
              <button
                key={child}
                type="button"
                onClick={() => onSelect(child)}
                className="pointer-coarse:min-h-11"
              >
                <span className="text-white/60">{key ? `≤ ${keyText(key)}` : "rest"} →</span> p
                {child}
              </button>
            );
          })}
        </p>
      )}
      {page.kind === "leaf" && (
        <p className="max-h-40 overflow-auto break-words text-white/60">
          {page.keys.map(keyText).join("  ")}
        </p>
      )}
      {page.overflow.map((chain) => (
        <p key={chain.cell}>
          {keyText(page.keys[chain.cell] ?? { values: [], rowid: 0 })}:{" "}
          {chain.bytes.toLocaleString()} bytes, too big for the leaf, chained over{" "}
          {chain.pages.map((p) => `p${p}`).join(" → ")}
        </p>
      ))}
    </div>
  );
}

/** An INSERT of made-up values for any table. The rowid comes free. */
function sampleInsert(table: TableInfo, n: number): string {
  const columns = table.columns.filter((c) => !(c.primary_key && c.affinity === "integer"));
  const values = columns.map((c) => {
    if (c.affinity === "integer") return String((n % 8) + 1);
    if (c.affinity === "real") return ((n % 10) / 2).toFixed(1);
    return `'inserted ${n}'`;
  });
  return `INSERT INTO ${table.name} (${columns.map((c) => c.name).join(", ")}) VALUES (${values.join(", ")})`;
}

interface Props {
  engine: Engine;
  tables: TableInfo[];
  /** Goes up whenever a statement may have changed pages. */
  version: number;
  /** Rows were inserted here; the schema list should catch up. */
  onTables: (tables: TableInfo[]) => void;
}

export function PageViewer({ engine, tables, version, onTables }: Props) {
  const names = tables.flatMap((t) => [t.name, ...t.indexes.map((i) => i.name)]);
  const [name, setName] = useState(() =>
    names.includes("feeding") ? "feeding" : (names[0] ?? ""),
  );
  const [tree, setTree] = useState<TreeInfo | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [change, setChange] = useState<Change>({ pages: new Set(), split: null });
  const [selected, setSelected] = useState<number | null>(null);
  const [inserting, setInserting] = useState(false);
  const [inserted, setInserted] = useState(0);
  const latest = useRef<TreeInfo | null>(null);

  const table = tables.find((t) => t.name === name || t.indexes.some((i) => i.name === name));

  // Read the tree again whenever it may have changed, and mark what did.
  const refresh = (shown: string) => {
    const out = engine.inspect(shown);
    if ("error" in out) {
      setError(out.error);
      setTree(null);
      latest.current = null;
      return null;
    }
    const before = latest.current;
    const next =
      before && before.name === out.tree.name
        ? compare(before, out.tree)
        : { pages: new Set<number>(), split: null };
    latest.current = out.tree;
    setError(null);
    setTree(out.tree);
    setChange(next);
    return next;
  };

  // biome-ignore lint/correctness/useExhaustiveDependencies: refresh reads only refs and setters
  useEffect(() => {
    refresh(name);
  }, [name, version]);

  // Insert one row per tick until a page splits, then stop on it.
  // biome-ignore lint/correctness/useExhaustiveDependencies: refresh reads only refs and setters
  useEffect(() => {
    if (!inserting || !table) return;
    const timer = setTimeout(() => {
      const out = engine.run(sampleInsert(table, table.row_count + 1));
      onTables(out.tables);
      const failed = out.statements.find((s) => s.status === "error");
      const next = refresh(name);
      setInserted((n) => n + 1);
      if (failed && failed.status === "error") {
        setError(failed.message);
        setInserting(false);
      } else if (next?.split) {
        setInserting(false);
      }
    }, 45);
    return () => clearTimeout(timer);
  }, [inserting, inserted, table, name]);

  // A split can land off to the right of a wide tree; bring it into view.
  const treeRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!change.split) return;
    const added = [...change.pages].at(-1);
    treeRef.current
      ?.querySelector(`[data-page="${added}"]`)
      ?.scrollIntoView({ block: "nearest", inline: "nearest" });
  }, [change]);

  const byId = useMemo(() => new Map(tree?.pages.map((p) => [p.id, p]) ?? []), [tree]);
  const detail = byId.get(selected ?? tree?.root ?? -1);

  return (
    <section className="flex flex-col gap-4">
      <div className="flex flex-col gap-2">
        <Heading>B-tree pages</Heading>
        <div className="flex flex-wrap gap-x-3 gap-y-1 font-mono pointer-coarse:gap-y-0">
          {names.map((n) => (
            <button
              key={n}
              type="button"
              onClick={() => {
                setName(n);
                setSelected(null);
                setInserting(false);
              }}
              className={`pointer-coarse:min-h-11 ${n === name ? "text-white underline underline-offset-4" : "text-white/60"}`}
            >
              {n}
            </button>
          ))}
        </div>
      </div>

      {tree && (
        <p className="font-mono tabular-nums text-white/60">
          <span className="text-white">{tree.kind}</span> · root p{tree.root} · {tree.depth}{" "}
          {tree.depth === 1 ? "level" : "levels"} · {tree.pages.length} pages ·{" "}
          {table?.row_count ?? 0} rows · file {tree.file_pages} pages, {tree.free_pages} free
        </p>
      )}

      <div className="flex flex-wrap items-center gap-3 font-mono">
        <button
          type="button"
          disabled={!table}
          onClick={() => {
            setInserted(0);
            setInserting(!inserting);
          }}
          className="border border-white px-3 py-1 disabled:border-white/34 disabled:text-white/34 pointer-coarse:min-h-11"
        >
          {inserting ? "stop" : "insert until a split"}
        </button>
        {inserted > 0 && (
          <span className="tabular-nums text-white/60">
            inserted {inserted} {inserted === 1 ? "row" : "rows"} into {table?.name}
          </span>
        )}
      </div>
      {change.split && <p className="font-mono text-fish">{change.split}</p>}
      {error && <p className="font-mono">{error}</p>}

      {tree && (
        <ScrollX className="pb-1">
          <div
            ref={treeRef}
            className="flex"
            style={{ minWidth: `${leafCount(tree.root, byId) * 4.5}rem` }}
          >
            <Subtree
              id={tree.root}
              byId={byId}
              changed={change.pages}
              selected={selected ?? tree.root}
              onSelect={setSelected}
            />
          </div>
        </ScrollX>
      )}
      {tree?.truncated && (
        <p className="text-white/60">Showing the first {tree.pages.length} pages.</p>
      )}
      {detail && <PageDetail page={detail} onSelect={setSelected} />}
    </section>
  );
}
