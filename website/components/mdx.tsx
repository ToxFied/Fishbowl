import defaultMdxComponents from "fumadocs-ui/mdx";
import type { MDXComponents } from "mdx/types";
import type { ComponentProps } from "react";
import { repositoryBranch, repositoryUrl } from "@/lib/shared";

/** Resolves `../crates/x.rs` from `docs/learn/note.md` to `crates/x.rs`. */
function repositoryPath(fromDir: string, href: string): string {
  const parts = fromDir.split("/").filter(Boolean);
  for (const part of href.split("/")) {
    if (part === "..") parts.pop();
    else if (part !== ".") parts.push(part);
  }
  return parts.join("/");
}

/**
 * The link for anything that is not another docs page. createRelativeLink
 * resolves links between pages first; a relative link left over points at a
 * file elsewhere in the repository, so it opens on GitHub.
 */
export function repositoryLink(pagePath: string) {
  const dir = `docs/${pagePath.split("/").slice(0, -1).join("/")}`;

  return function RepositoryLink(props: ComponentProps<"a">) {
    const Link = defaultMdxComponents.a;
    const { href } = props;
    if (!href || /^(?:[a-z][a-z\d+.-]*:|\/|#)/i.test(href)) {
      return <Link {...props} />;
    }
    const path = repositoryPath(dir, href);
    return (
      <Link {...props} href={`${repositoryUrl}/blob/${repositoryBranch}/${path}`} />
    );
  };
}

export function getMDXComponents(components?: MDXComponents) {
  return {
    ...defaultMdxComponents,
    ...components,
  } satisfies MDXComponents;
}

export const useMDXComponents = getMDXComponents;

declare global {
  type MDXProvidedComponents = ReturnType<typeof getMDXComponents>;
}
