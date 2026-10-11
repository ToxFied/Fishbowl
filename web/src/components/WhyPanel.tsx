import type { Frame, Step } from "../trace";
import { Heading } from "./Heading";

/**
 * The planner's reason for each step where it had a choice. Today that is a
 * rule per step; once plans have costs, the plans it turned down go here too.
 */
export function WhyPanel({ steps, frame }: { steps: Step[]; frame: Frame }) {
  const explained = steps.filter((s) => s.node.reason);
  if (explained.length === 0) return null;
  return (
    <section className="flex flex-col gap-2">
      <Heading>Why this plan</Heading>
      {explained.map(({ node }) => (
        <p key={node.id}>
          <span className={`font-mono ${frame.current?.step === node.id ? "text-fish" : ""}`}>
            {node.operator}
          </span>{" "}
          <span className="text-white/60">{node.reason}</span>
        </p>
      ))}
    </section>
  );
}
