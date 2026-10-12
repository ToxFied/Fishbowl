import { defineConfig, defineDocs } from "fumadocs-mdx/config";
import { metaSchema, pageSchema } from "fumadocs-core/source/schema";

export const docs = defineDocs({
  dir: "../docs",
  docs: {
    schema: pageSchema,
    postprocess: {
      includeProcessedMarkdown: true,
    },
  },
  meta: {
    schema: metaSchema,
  },
});

const monochromeTheme = {
  name: "fishbowl-monochrome",
  type: "dark" as const,
  colors: {
    "editor.background": "#000000",
    "editor.foreground": "#ffffff",
  },
  tokenColors: [
    {
      scope: [],
      settings: {
        foreground: "#ffffff",
      },
    },
  ],
};

export default defineConfig({
  mdxOptions: {
    rehypeCodeOptions: {
      themes: {
        light: monochromeTheme,
        dark: monochromeTheme,
      },
    },
  },
});
