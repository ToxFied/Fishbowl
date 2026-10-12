import { getPageMarkdownUrl, source } from "@/lib/source";
import {
  DocsBody,
  DocsDescription,
  DocsPage,
  DocsTitle,
  MarkdownCopyButton,
} from "fumadocs-ui/layouts/docs/page";
import { buttonVariants } from "fumadocs-ui/components/ui/button";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "fumadocs-ui/components/ui/popover";
import { createRelativeLink } from "fumadocs-ui/mdx";
import { ChevronDown, ExternalLinkIcon, TextIcon } from "lucide-react";
import { notFound } from "next/navigation";
import { getMDXComponents, repositoryLink } from "@/components/mdx";
import type { Metadata } from "next";
import {
  productionUrl,
  repositoryBranch,
  repositoryUrl,
} from "@/lib/shared";

export default async function Page(props: PageProps<"/docs/[[...slug]]">) {
  const params = await props.params;
  const page = source.getPage(params.slug);
  if (!page) notFound();

  const MDX = page.data.body;
  const markdownUrl = getPageMarkdownUrl(page).url;

  return (
    <DocsPage
      id="nd-page"
      role="main"
      tabIndex={-1}
      toc={page.data.toc}
      full={page.data.full}
      breadcrumb={{ enabled: false }}
      tableOfContent={{ style: "clerk" }}
      tableOfContentPopover={{ style: "clerk" }}
    >
      <DocsTitle className="text-balance text-[2rem] font-semibold tracking-[-0.04em]">
        {page.data.title}
      </DocsTitle>
      <DocsDescription className="mb-0 mt-1 max-w-2xl text-base leading-7">
        {page.data.description}
      </DocsDescription>
      <div className="flex items-center gap-2 border-b border-fd-border pb-7 pt-1">
        <MarkdownCopyButton markdownUrl={markdownUrl} />
        <PageActionsPopover
          markdownUrl={markdownUrl}
          githubUrl={`${repositoryUrl}/blob/${repositoryBranch}/docs/${page.path}`}
        />
      </div>
      <DocsBody className="max-w-none [&>h2]:mt-12 [&>h2]:border-b [&>h2]:border-fd-border [&>h2]:pb-2 [&>h2]:tracking-[-0.025em] [&>h3]:mt-8 [&>h3]:tracking-[-0.015em]">
        <MDX
          components={getMDXComponents({
            a: createRelativeLink(source, page, repositoryLink(page.path)),
          })}
        />
      </DocsBody>
    </DocsPage>
  );
}

function PageActionsPopover({
  markdownUrl,
  githubUrl,
}: {
  markdownUrl: string;
  githubUrl: string;
}) {
  const itemClassName =
    "inline-flex items-center gap-2 rounded-lg p-2 text-sm hover:bg-fd-accent hover:text-fd-accent-foreground [&_svg]:size-4";

  return (
    <Popover>
      <PopoverTrigger
        className={buttonVariants({
          color: "secondary",
          size: "sm",
          className:
            "gap-2 data-[state=open]:bg-fd-accent data-[state=open]:text-fd-accent-foreground",
        })}
      >
        Open
        <ChevronDown className="size-3.5 text-fd-muted-foreground" />
      </PopoverTrigger>
      <PopoverContent className="flex flex-col">
        <a
          href={githubUrl}
          rel="noreferrer noopener"
          target="_blank"
          className={itemClassName}
        >
          <svg fill="currentColor" role="img" viewBox="0 0 24 24">
            <title>GitHub</title>
            <path d="M12 .297c-6.63 0-12 5.373-12 12 0 5.303 3.438 9.8 8.205 11.385.6.113.82-.258.82-.577 0-.285-.01-1.04-.015-2.04-3.338.724-4.042-1.61-4.042-1.61C4.422 18.07 3.633 17.7 3.633 17.7c-1.087-.744.084-.729.084-.729 1.205.084 1.838 1.236 1.838 1.236 1.07 1.835 2.809 1.305 3.495.998.108-.776.417-1.305.76-1.605-2.665-.3-5.466-1.332-5.466-5.93 0-1.31.465-2.38 1.235-3.22-.135-.303-.54-1.523.105-3.176 0 0 1.005-.322 3.3 1.23.96-.267 1.98-.399 3-.405 1.02.006 2.04.138 3 .405 2.28-1.552 3.285-1.23 3.285-1.23.645 1.653.24 2.873.12 3.176.765.84 1.23 1.91 1.23 3.22 0 4.61-2.805 5.625-5.475 5.92.42.36.81 1.096.81 2.22 0 1.606-.015 2.896-.015 3.286 0 .315.21.69.825.57C20.565 22.092 24 17.592 24 12.297c0-6.627-5.373-12-12-12" />
          </svg>
          Open in GitHub
          <ExternalLinkIcon className="ms-auto size-3.5 text-fd-muted-foreground" />
        </a>
        <a
          href={markdownUrl}
          rel="noreferrer noopener"
          target="_blank"
          className={itemClassName}
        >
          <TextIcon />
          View as Markdown
          <ExternalLinkIcon className="ms-auto size-3.5 text-fd-muted-foreground" />
        </a>
      </PopoverContent>
    </Popover>
  );
}

export function generateStaticParams() {
  return source.generateParams();
}

export async function generateMetadata(
  props: PageProps<"/docs/[[...slug]]">,
): Promise<Metadata> {
  const params = await props.params;
  const page = source.getPage(params.slug);
  if (!page) notFound();

  return {
    title: page.data.title,
    description: page.data.description,
    alternates: {
      canonical: `${productionUrl}${page.url}`,
    },
    openGraph: {
      title: `${page.data.title} · Fishbowl`,
      description: page.data.description,
      url: `${productionUrl}${page.url}`,
      type: "article",
    },
  };
}
