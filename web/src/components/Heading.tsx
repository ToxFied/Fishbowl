import type { ReactNode } from "react";

export function Heading({ children }: { children: ReactNode }) {
  return <h2 className="font-mono text-[11px] uppercase tracking-[0.12em]">{children}</h2>;
}
