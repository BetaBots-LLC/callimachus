// A rehype plugin that makes rendered markdown safe to show inside the app window.
//
// Transcripts and AI answers are untrusted text. Rendered as-is, a link click would
// navigate the whole webview away (no back button), a `javascript:` href would run in
// the app origin with IPC access, and a remote image would load (a tracking pixel)
// the moment a thread is opened. This plugin rewrites the HTML tree so that:
//  - links keep their href only for http(s) and mailto, and are marked target=_blank so
//    the click handler in main.tsx (and the opener plugin) send them to the system
//    browser; every other link (javascript:, data:, file:, relative paths) is unwrapped
//    to its text;
//  - in-page fragment links (footnotes) are kept as-is;
//  - images never load: an http(s) image becomes a plain link to it, anything else
//    becomes its alt text.
//
// The node types are declared structurally so this stays free of @types/hast.

interface HastNode {
  type: string;
  tagName?: string;
  value?: string;
  properties?: Record<string, unknown>;
  children?: HastNode[];
}

const LINK_PROTOCOLS = new Set(["http:", "https:", "mailto:"]);
const IMAGE_PROTOCOLS = new Set(["http:", "https:"]);

/** Parse an absolute URL; null for relative or malformed hrefs. */
function absoluteUrl(href: string): URL | null {
  try {
    return new URL(href);
  } catch {
    return null;
  }
}

/** Whether an href may stay clickable (opened in the system browser). */
export function isSafeExternalHref(href: unknown): href is string {
  if (typeof href !== "string") return false;
  const url = absoluteUrl(href.trim());
  return url !== null && LINK_PROTOCOLS.has(url.protocol);
}

function text(value: string): HastNode {
  return { type: "text", value };
}

function externalLink(href: string, children: HastNode[]): HastNode {
  return {
    type: "element",
    tagName: "a",
    properties: { href, target: "_blank", rel: ["noopener", "noreferrer"] },
    children,
  };
}

/** Rewrite one node, returning what replaces it (usually just itself). Children are
 *  transformed first, so an image nested in an unwrapped link is handled too.
 *  `inLink` avoids nesting an image's replacement link inside another link. */
function transformNode(node: HastNode, inLink: boolean): HastNode[] {
  if (node.type === "element" && node.tagName === "img") {
    const src = typeof node.properties?.src === "string" ? node.properties.src.trim() : "";
    const alt = typeof node.properties?.alt === "string" ? node.properties.alt.trim() : "";
    const url = absoluteUrl(src);
    if (url && IMAGE_PROTOCOLS.has(url.protocol)) {
      const label = text(alt ? `Image: ${alt}` : `Image (${url.host})`);
      return inLink ? [label] : [externalLink(url.href, [label])];
    }
    return alt ? [text(alt)] : [];
  }

  const isLink = node.type === "element" && node.tagName === "a";
  if (node.children) node.children = transformChildren(node.children, inLink || isLink);

  if (isLink) {
    const href = node.properties?.href;
    if (typeof href === "string" && href.startsWith("#")) return [node]; // footnotes
    if (isSafeExternalHref(href)) return [externalLink(href.trim(), node.children ?? [])];
    return node.children ?? []; // unwrap: keep the link text, drop the dead/unsafe href
  }

  return [node];
}

function transformChildren(nodes: HastNode[], inLink: boolean): HastNode[] {
  return nodes.flatMap((n) => transformNode(n, inLink));
}

/** The rehype plugin (use after remark-rehype). */
export function rehypeSafeLinks() {
  return (tree: unknown) => {
    const root = tree as HastNode;
    if (root.children) root.children = transformChildren(root.children, false);
  };
}
