"use client";

import SearchDialog from "@/components/search";
import { RootProvider } from "fumadocs-ui/provider/next";
import type { ReactNode } from "react";

export function Provider({ children }: { children: ReactNode }) {
  return (
    <RootProvider
      theme={{ forcedTheme: "dark", defaultTheme: "dark" }}
      search={{
        SearchDialog,
        links: [
          ["Getting started", "/docs/getting-started"],
          ["Case study", "/docs/case-study"],
          ["How it works", "/docs/learn"],
        ],
      }}
    >
      {children}
    </RootProvider>
  );
}
