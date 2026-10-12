// Checks the exported site before it is deployed: every internal link and
// image in the HTML must exist in `out/`, and no page may link to a source
// `.md` file, which only resolves on GitHub. Run after `pnpm build` with
// GITHUB_ACTIONS=true, so links carry the /Fishbowl base path.
import { readdir, readFile } from "node:fs/promises";
import { join, posix, relative } from "node:path";

const basePath = "/Fishbowl";
const root = new URL("../out/", import.meta.url).pathname;
const files = (await readdir(root, { recursive: true, withFileTypes: true }))
  .filter((entry) => entry.isFile())
  .map((entry) => relative(root, join(entry.parentPath, entry.name)));
const exported = new Set(files);
const errors = new Set();

for (const required of ["llms.txt", "llms-full.txt", "docs/index.html"]) {
  if (!exported.has(required)) errors.add(`missing ${required}`);
}

for (const file of files.filter((path) => path.endsWith(".html"))) {
  const html = await readFile(join(root, file), "utf8");
  for (const [, attribute, url] of html.matchAll(/\b(href|src)="([^"]+)"/g)) {
    const problem = check(url, attribute, file);
    if (problem) errors.add(`${file}: ${problem}`);
  }
}

function check(url, attribute, file) {
  if (/^(?:[a-z][a-z\d+.-]*:|\/\/|#)/i.test(url)) return null;
  const path = url.split(/[?#]/, 1)[0];
  if (/\.mdx?$/i.test(path) && !path.includes("/llms.mdx/")) {
    return `links to source file ${url}`;
  }
  if (path.startsWith("/") && path !== basePath && !path.startsWith(`${basePath}/`)) {
    return `${attribute} ${url} lacks the ${basePath} base path`;
  }
  const target = path.startsWith("/")
    ? path.slice(basePath.length + 1)
    : posix.normalize(posix.join(posix.dirname(file), path));
  const candidates = [target, posix.join(target, "index.html")];
  return candidates.some((candidate) => exported.has(candidate.replace(/^\.?\/?/, "")))
    ? null
    : `${attribute} ${url} is missing from the export`;
}

if (errors.size > 0) {
  console.error([...errors].sort().join("\n"));
  process.exitCode = 1;
} else {
  console.log(`Checked ${files.length} exported files.`);
}
