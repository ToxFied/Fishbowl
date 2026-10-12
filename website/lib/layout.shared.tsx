import type { BaseLayoutProps } from "fumadocs-ui/layouts/shared";
import {
  appName,
  deploymentBasePath,
  docsRoute,
  repositoryUrl,
} from "./shared";

function Brand() {
  return (
    <span className="flex items-center gap-2.5">
      <img
        aria-hidden="true"
        src={`${deploymentBasePath}/fishbowl-mark.svg`}
        className="size-6"
        width="24"
        height="24"
        alt=""
      />
      <span className="font-semibold tracking-[-0.02em]">{appName}</span>
    </span>
  );
}

export function baseOptions(): BaseLayoutProps {
  return {
    nav: {
      title: <Brand />,
      url: docsRoute,
    },
    // The demo is a separate static app at the site root, not a Next.js route.
    links: [
      {
        text: "Live demo",
        url: `${deploymentBasePath}/`,
        external: true,
      },
    ],
    githubUrl: repositoryUrl,
    themeSwitch: { enabled: false },
  };
}
