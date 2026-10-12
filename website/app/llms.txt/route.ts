import { formatLLMMarkdown, source } from "@/lib/source";
import { llms } from "fumadocs-core/source";

export const revalidate = false;

export function GET() {
  return new Response(`${formatLLMMarkdown(llms(source).index())}\n`, {
    headers: {
      "Content-Type": "text/plain; charset=utf-8",
    },
  });
}
