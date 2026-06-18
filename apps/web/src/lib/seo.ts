// Per-route SEO helpers. `seo()` returns the meta array TanStack Start's `head()`
// expects (title + description + Open Graph + Twitter). `canonical()` builds an
// absolute URL for the <link rel="canonical"> a route adds to head().links.

import { DESCRIPTION, SITE_URL } from "./site";

export const DEFAULT_OG_IMAGE = `${SITE_URL}/og.png`;

export function canonical(path: string): string {
  return new URL(path, SITE_URL).href;
}

type Meta =
  | { title: string }
  | { name: string; content: string }
  | { property: string; content: string };

export function seo(opts: {
  title: string;
  description?: string;
  path?: string;
  image?: string;
  type?: "website" | "article";
}): Meta[] {
  const description = opts.description ?? DESCRIPTION;
  const image = opts.image ?? DEFAULT_OG_IMAGE;
  const url = opts.path ? canonical(opts.path) : SITE_URL;

  return [
    { title: opts.title },
    { name: "description", content: description },
    { property: "og:type", content: opts.type ?? "website" },
    { property: "og:site_name", content: "Callimachus" },
    { property: "og:title", content: opts.title },
    { property: "og:description", content: description },
    { property: "og:url", content: url },
    { property: "og:image", content: image },
    { name: "twitter:card", content: "summary_large_image" },
    { name: "twitter:title", content: opts.title },
    { name: "twitter:description", content: description },
    { name: "twitter:image", content: image },
  ];
}
