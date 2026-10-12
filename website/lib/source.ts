import { docs } from "collections/server";
import { loader } from "fumadocs-core/source";
import {
  canonicalSiteUrl,
  deploymentBasePath,
  docsContentRoute,
  docsRoute,
} from "./shared";

export const source = loader({
  baseUrl: docsRoute,
  source: docs.toFumadocsSource(),
});

export function getPageMarkdownUrl(page: (typeof source)["$inferPage"]) {
  const segments = [...page.slugs, "content.md"];

  return {
    segments,
    url:
      deploymentBasePath +
      "/" +
      [page.locale, ...docsContentRoute.split("/"), ...segments]
        .filter(Boolean)
        .join("/"),
  };
}

export async function getLLMText(page: (typeof source)["$inferPage"]) {
  const processed = await page.data.getText("processed");
  const withoutDuplicateTitle = processed.replace(
    /^\s*#\s+.+(?:\r?\n)+/u,
    "",
  );

  return `# ${page.data.title} (${canonicalSiteUrl(page.url)})

${formatLLMMarkdown(withoutDuplicateTitle)}`;
}

export function formatLLMMarkdown(markdown: string): string {
  return rewriteDocumentationLinks(
    markdown
      .replace(
        /<Card\s+([\s\S]*?)\/>/gu,
        (_match, attributes: string) => renderCard(attributes),
      )
      .replace(
        /<Step>\s*\n([\s\S]*?)\n\s*<\/Step>/gu,
        (_match, body: string) => `\n\n${dedent(body)}\n\n`,
      )
      .replace(
        /<Callout(?:\s[^>]*)?>\s*\n([\s\S]*?)\n\s*<\/Callout>/gu,
        (_match, body: string) => `\n\n${dedent(body)}\n\n`,
      )
      .replace(
        /<Cards>\s*\n([\s\S]*?)\n\s*<\/Cards>/gu,
        (_match, body: string) => `\n\n${dedent(body)}\n\n`,
      )
      .replace(
        /<Steps>\s*\n([\s\S]*?)\n\s*<\/Steps>/gu,
        (_match, body: string) => `\n\n${dedent(body)}\n\n`,
      )
      .replace(/<\/?(?:Callout|Cards|Step|Steps)(?:\s[^>]*)?>\s*/gu, "")
      .replace(/^(#{1,6}\s+.+?)\s+\[#[a-z0-9-]+\]\s*$/gimu, "$1")
      .replace(/^[\t ]+$/gmu, "")
      .replace(/```([a-z0-9_-]+)\s+title="[^"]*"/giu, "```$1"),
  )
    .replace(/\n{3,}/gu, "\n\n")
    .replace(/^(#{1,6}\s+.+)\n(?=\S)/gmu, "$1\n\n")
    .trim();
}

function rewriteDocumentationLinks(markdown: string): string {
  return markdown
    .replace(
      /\]\(\/docs([^)]*)\)/gu,
      (_match, suffix: string) =>
        `](${canonicalSiteUrl(`/docs${suffix}`)})`,
    )
    .replace(
      /\]\((?:\.\/)?([a-z0-9-]+)\.(?:md|mdx)(#[^)]*)?\)/giu,
      (_match, slug: string, fragment: string | undefined) =>
        `](${canonicalSiteUrl(`/docs/${slug}${fragment ?? ""}`)})`,
    );
}

function renderCard(attributes: string): string {
  const title = readAttribute(attributes, "title");
  const description = readAttribute(attributes, "description");
  const href = readAttribute(attributes, "href");
  if (title === null || href === null) {
    return "";
  }
  const url = href.startsWith("/docs") ? canonicalSiteUrl(href) : href;
  return `- [${title}](${url})${description === null ? "" : ` — ${description}`}`;
}

function readAttribute(attributes: string, name: string): string | null {
  return new RegExp(`${name}="([^"]*)"`, "u").exec(attributes)?.[1] ?? null;
}

function dedent(value: string): string {
  const lines = value.split("\n");
  const indents = lines
    .filter((line) => line.trim().length > 0)
    .map((line) => /^\s*/u.exec(line)?.[0].length ?? 0);
  const indentation = Math.min(...indents);
  return lines
    .map((line) => line.slice(Math.min(indentation, line.length)))
    .join("\n")
    .trim();
}
