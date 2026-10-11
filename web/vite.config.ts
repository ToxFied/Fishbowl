import { fileURLToPath } from "node:url";
import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

const fonts = fileURLToPath(new URL("node_modules/geist/dist/fonts", import.meta.url));

export default defineConfig({
  // Relative asset paths let the static build live under any URL prefix.
  base: "./",
  plugins: [react(), tailwindcss()],
  resolve: { alias: { "@fonts": fonts } },
  // The brand assets live in ../brand, outside the web root.
  server: { fs: { allow: [".."] } },
});
