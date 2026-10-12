export const appName = "Fishbowl";
export const deploymentBasePath = process.env.NEXT_PUBLIC_BASE_PATH ?? "";
export const docsRoute = "/docs";
export const docsContentRoute = "/llms.mdx/docs";
export const repositoryUrl = "https://github.com/ToxFied/Fishbowl";
export const repositoryBranch = "main";
export const productionUrl = "https://toxfied.github.io/Fishbowl";

export function canonicalSiteUrl(path: string): string {
  const normalizedPath = path.startsWith("/") ? path : `/${path}`;
  return `${productionUrl}${normalizedPath}`;
}
