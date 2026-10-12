import { source } from "@/lib/source";
import { createFromSource } from "fumadocs-core/search/server";
import { deploymentBasePath } from "@/lib/shared";

export const revalidate = false;

export const { staticGET: GET } = createFromSource(source, {
  language: "english",
  buildIndex: async (page) => ({
    id: page.url,
    title: page.data.title,
    description: page.data.description,
    structuredData: page.data.structuredData,
    url: `${deploymentBasePath}${page.url}`,
  }),
});
