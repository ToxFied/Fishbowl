import {
  type CSSProperties,
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import type { QueryResult, Row, TraceEvent } from "../engine";
import { type Frame, flatten, formatValue, frameAt, narrate, pace, type Step } from "../trace";
import { Heading } from "./Heading";
import { ResultTable } from "./ResultTable";
import { ScanTables } from "./ScanTables";
import { WhyPanel } from "./WhyPanel";

// Plain-English notes, shown for a step you click.
const OPERATORS: Record<string, string> = {
  SeqScan: "Reads every row of the table, one at a time, in storage order.",
  IndexScan:
    "Walks down an index's B-tree to the first match, reads entries in order until the last, and fetches each row by rowid. Rows it never reaches are never read.",
  Filter: "Passes on only the rows where the condition is true. NULL counts as not true.",
  Project: "Computes the output columns from each row it receives.",
  OneRow: "Produces one empty row, so a SELECT without FROM has something to compute on.",
  Sort: "Reads every row it is given, then hands them up in order. It cannot return its first row before it has seen the last.",
  Aggregate:
    "Sorts rows into groups with equal GROUP BY values and computes count(), sum() and the rest for each. One row comes out per group.",
  Distinct: "Passes on each row only the first time it sees it.",
  Limit: "Skips the OFFSET rows, passes on rows until it has enough, then stops asking.",
  NestedLoopJoin:
    "For each row of its first input, reads all of its second input again and passes on the pairs that meet the condition.",
  Materialize:
    "Reads its input once and keeps the rows, so the join above can replay them for every left row.",
  Subquery:
    "A query inside an expression. Its count is how many times it ran: once if it reads nothing from the outer row, once per row if it does.",
  SubqueryScan: "Passes on the rows of a view or a subquery in FROM.",
  Union: "Reads both inputs and returns each distinct row once, sorted.",
  UnionAll: "Passes on every row of its first input, then every row of its second.",
  Intersect: "Returns the distinct rows that appear in both inputs, sorted.",
  Except: "Returns the distinct rows of its first input that are not in its second, sorted.",
};

/** Steps that read all their input before passing anything on. */
const HOLDS = new Set(["Sort", "Aggregate", "Materialize", "Union", "Intersect", "Except"]);

const reducedMotion = () => window.matchMedia("(prefers-reduced-motion: reduce)").matches;

function Values({ row }: { row: Row }) {
  const shown = row.slice(0, 6);
  return (
    <>
      {shown.map((value, i) => (
        // biome-ignore lint/suspicious/noArrayIndexKey: values in fixed column order
        <span key={i} className={i > 0 ? "pl-2" : ""}>
          {formatValue(value)}
        </span>
      ))}
      {row.length > shown.length && <span className="pl-2">…</span>}
    </>
  );
}

function Chip({ step, frame, live }: { step: Step; frame: Frame; live: boolean }) {
  const s = frame.steps[step.node.id];
  const event: TraceEvent | null | undefined = s?.last;
  const tone = live ? "text-fish" : "text-white/34";
  if (!s || !event) {
    if (!HOLDS.has(step.node.operator)) return <span />;
    const held = step.inputs.reduce(
      (sum, input) => sum + (frame.steps[input.node.id]?.out ?? 0),
      0,
    );
    return held > 0 ? <span className="text-white/34">holding {held}</span> : <span />;
  }
  if (event.kind === "run" || event.kind === "reuse") {
    return (
      <span className={tone}>
        {event.kind === "run" ? `run ${s.runs} for ` : "reused for "}
        <Values row={event.row} />
      </span>
    );
  }
  const dropped = event.kind === "drop";
  const body =
    step.node.operator === "NestedLoopJoin" && s.leftWidth > 0 ? (
      <>
        <Values row={event.row.slice(0, s.leftWidth)} />
        <span className="px-2">⋈</span>
        <Values row={event.row.slice(s.leftWidth)} />
      </>
    ) : (
      <Values row={event.row} />
    );
  return (
    <span className={tone}>
      {event.rowid !== undefined && <span className="pr-2">#{event.rowid}</span>}
      <span className={dropped ? "line-through" : ""}>{body}</span>
      {dropped && <span className="pl-2">✕</span>}
    </span>
  );
}

function Count({ step, frame }: { step: Step; frame: Frame }) {
  const s = frame.steps[step.node.id];
  if (!s) return null;
  if (step.node.operator === "Subquery") return <>×{s.runs}</>;
  return (
    <>
      {s.out}
      {s.dropped > 0 && <span className="text-white/60"> ✕{s.dropped}</span>}
    </>
  );
}

export function TraceView({ result, autoplay }: { result: QueryResult; autoplay: boolean }) {
  const trace = result.trace;
  const steps = useMemo(() => flatten(result.plan), [result.plan]);
  const total = trace?.events.length ?? 0;
  const [position, setPosition] = useState(autoplay ? 0 : total);
  const [playing, setPlaying] = useState(autoplay && total > 0);
  const [fast, setFast] = useState(false);
  const [focus, setFocus] = useState<number | null>(null);
  const frame = useMemo(
    () => (trace ? frameAt(trace, steps, position) : null),
    [trace, steps, position],
  );

  // Advance one event per tick while playing; stop at the end.
  useEffect(() => {
    if (!playing) return;
    if (position >= total) {
      setPlaying(false);
      return;
    }
    const timer = setTimeout(() => setPosition((p) => p + 1), pace(total, fast));
    return () => clearTimeout(timer);
  }, [playing, position, total, fast]);

  const go = (p: number) => {
    setPlaying(false);
    setPosition(Math.max(0, Math.min(total, p)));
  };
  const toggle = () => {
    if (playing) setPlaying(false);
    else {
      if (position >= total) setPosition(0);
      setPlaying(true);
    }
  };

  // Arrow keys step, space plays, unless you are typing.
  const keys = useRef({ go, toggle, position });
  keys.current = { go, toggle, position };
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const target = e.target as HTMLElement;
      if (target.closest("textarea, input, select")) return;
      const k = keys.current;
      if (e.key === "ArrowRight") k.go(k.position + 1);
      else if (e.key === "ArrowLeft") k.go(k.position - 1);
      else if (e.key === " " && !target.closest("button")) k.toggle();
      else return;
      e.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  // Each event moves one row one level: out of its table into a scan, from an
  // input up into the step that acted, from the root into the result. One
  // short movement per event, and only when moving forward by one.
  const anchors = useRef(new Map<string, HTMLElement>());
  const register = useCallback((key: string, el: HTMLElement | null) => {
    if (el) anchors.current.set(key, el);
    else anchors.current.delete(key);
  }, []);
  const previous = useRef(position);
  useLayoutEffect(() => {
    const forward = position === previous.current + 1;
    previous.current = position;
    const event = frame?.current;
    if (!forward || !event || !trace || reducedMotion()) return;
    const step = steps[event.step];
    const before = trace.events[position - 2];
    const chip = (s: Step | null | undefined) =>
      s ? anchors.current.get(`step:${s.node.id}`) : undefined;
    const source =
      event.kind === "run" || event.kind === "reuse"
        ? chip(step?.parent)
        : event.rowid !== undefined
          ? anchors.current.get(`${event.step}:${event.rowid}`)
          : chip(step?.inputs.find((input) => input.node.id === before?.step));
    const duration = Math.min(340, pace(total, fast) * 0.8);
    const rise = (target: HTMLElement | undefined, from: HTMLElement | undefined) => {
      if (!target) return;
      const dy = from ? from.getBoundingClientRect().top - target.getBoundingClientRect().top : 14;
      target.animate(
        [
          { transform: `translateY(${dy}px)`, opacity: 0.5 },
          { transform: "none", opacity: 1 },
        ],
        { duration, easing: "cubic-bezier(.2,.7,.2,1)" },
      );
    };
    rise(chip(step), source);
    if (step?.parent === null && event.kind === "out") {
      rise(anchors.current.get("result"), chip(step));
    }
  }, [position, frame, trace, steps, total, fast]);

  if (!trace || !frame) return null;
  const current = frame.current;
  const focused = focus === null ? null : steps[focus];

  return (
    <div className="flex flex-col gap-6">
      <section className="flex flex-col gap-2">
        <Heading>Result</Heading>
        <ResultTable
          columns={result.columns}
          rows={frame.result}
          total={result.rows.length}
          fresh={current?.step === result.plan.id && current.kind === "out"}
          lastRow={(el) => register("result", el)}
        />
      </section>

      <section className="flex flex-col gap-2">
        <Heading>Plan</Heading>
        <div className="font-mono">
          {steps.map((step) => {
            const { node, depth } = step;
            const live = current?.step === node.id;
            const touched = (frame.steps[node.id]?.last ?? null) !== null;
            return (
              <button
                key={node.id}
                type="button"
                title={`${node.operator} ${node.detail}`}
                onClick={() => setFocus(focus === node.id ? null : node.id)}
                className="grid w-full grid-cols-[minmax(0,1fr)_4.5rem] items-baseline gap-x-3 py-1 text-left sm:grid-cols-[minmax(0,1.25fr)_4.5rem_minmax(0,1fr)]"
              >
                <span className="truncate" style={{ paddingLeft: `${depth}rem` }}>
                  {depth > 0 && <span className="text-white/34">└ </span>}
                  <span className={live ? "text-fish" : touched ? "text-white" : "text-white/60"}>
                    {node.operator}
                  </span>
                  {node.detail && <span className="text-white/60"> {node.detail}</span>}
                </span>
                <span className={`text-right tabular-nums ${live ? "text-fish" : ""}`}>
                  <Count step={step} frame={frame} />
                </span>
                <span
                  ref={(el) => register(`step:${node.id}`, el)}
                  className="col-span-2 truncate whitespace-pre pl-(--indent) sm:col-span-1 sm:pl-0"
                  style={{ "--indent": `${depth + 1}rem` } as CSSProperties}
                >
                  <Chip step={step} frame={frame} live={live} />
                </span>
              </button>
            );
          })}
        </div>
        <p className="min-h-5 font-mono">
          {current ? (
            <span className="text-fish">{narrate(frame, steps)}</span>
          ) : position === 0 ? (
            <span className="text-white/60">Press play, or step with ← →</span>
          ) : null}
          {trace.truncated && position === total && (
            <span className="text-white/60"> · the trace keeps the first {total} events</span>
          )}
        </p>
        {focused && (
          <p className="text-white/60">
            <span className="text-white">{focused.node.operator}</span>{" "}
            {OPERATORS[focused.node.operator] ?? ""}
          </p>
        )}
      </section>

      {trace.scans.length > 0 && (
        <section className="flex flex-col gap-2">
          <Heading>Tables</Heading>
          <ScanTables steps={steps} scans={trace.scans} frame={frame} register={register} />
        </section>
      )}

      <WhyPanel steps={steps} frame={frame} />

      <div className="sticky bottom-0 -mx-4 flex items-center gap-1 border-t border-white/15 bg-black px-3 py-2 font-mono">
        <button type="button" aria-label="Start" className="px-2 py-1" onClick={() => go(0)}>
          ⏮
        </button>
        <button
          type="button"
          aria-label="Step back"
          className="px-2 py-1"
          onClick={() => go(position - 1)}
        >
          ◀
        </button>
        <button
          type="button"
          onClick={toggle}
          className="w-16 border border-white px-2 py-1 text-center"
        >
          {playing ? "pause" : "play"}
        </button>
        <button
          type="button"
          aria-label="Step forward"
          className="px-2 py-1"
          onClick={() => go(position + 1)}
        >
          ▶
        </button>
        <button type="button" aria-label="End" className="px-2 py-1" onClick={() => go(total)}>
          ⏭
        </button>
        <input
          type="range"
          min={0}
          max={total}
          value={position}
          onChange={(e) => go(Number(e.target.value))}
          aria-label="Scrub through the trace"
          className="scrubber mx-2 min-w-0 flex-1"
        />
        <span className="text-right tabular-nums">
          {position}/{total}
        </span>
        <button
          type="button"
          onClick={() => setFast(!fast)}
          className={`w-8 px-1 py-1 text-right ${fast ? "text-white" : "text-white/60"}`}
        >
          {fast ? "4×" : "1×"}
        </button>
      </div>
    </div>
  );
}
