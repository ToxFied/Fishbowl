import type { Metadata, Viewport } from "next";
import { Geist, Geist_Mono } from "next/font/google";
import { Provider } from "@/components/provider";
import { deploymentBasePath, productionUrl } from "@/lib/shared";
import "./global.css";

const sans = Geist({
  subsets: ["latin"],
  variable: "--font-sans",
});

const mono = Geist_Mono({
  subsets: ["latin"],
  variable: "--font-mono",
});

const description =
  "A SQL database written from scratch in Rust that runs in your browser and shows each query as it runs.";

export const metadata: Metadata = {
  metadataBase: new URL(productionUrl),
  title: {
    default: "Fishbowl Docs",
    template: "%s · Fishbowl",
  },
  description,
  icons: {
    icon: [
      {
        url: `${deploymentBasePath}/fishbowl-mark.svg`,
        type: "image/svg+xml",
      },
    ],
  },
  openGraph: {
    title: "Fishbowl Docs",
    description,
    type: "website",
    siteName: "Fishbowl Docs",
    images: [`${productionUrl}/social-card.png`],
  },
  twitter: {
    card: "summary_large_image",
    title: "Fishbowl Docs",
    description,
    images: [`${productionUrl}/social-card.png`],
  },
};

export const viewport: Viewport = {
  colorScheme: "dark",
  themeColor: "#000000",
};

export default function Layout({ children }: LayoutProps<"/">) {
  return (
    <html
      lang="en"
      className={`dark ${sans.variable} ${mono.variable}`}
      suppressHydrationWarning
    >
      <body className="flex min-h-screen flex-col">
        <a
          href="#nd-page"
          className="fixed start-4 top-4 z-50 -translate-y-20 rounded-lg bg-fd-primary px-4 py-2 text-sm font-medium text-fd-primary-foreground shadow-lg transition-transform focus:translate-y-0 focus:outline-none focus:ring-2 focus:ring-fd-ring focus:ring-offset-2 focus:ring-offset-fd-background"
        >
          Skip to documentation
        </a>
        <Provider>{children}</Provider>
      </body>
    </html>
  );
}
