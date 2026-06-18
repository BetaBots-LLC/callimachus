// Single source of truth for the site's external facts: URLs, the product matrix,
// the supported agents, navigation. Imported by routes, SEO, and JSON-LD.

export const SITE_URL =
  (import.meta.env.VITE_SITE_URL as string | undefined) ?? "https://callimachus.app";

export const REPO = "BetaBots-LLC/callimachus";
export const GITHUB_URL = `https://github.com/${REPO}`;
export const RELEASES_URL = `${GITHUB_URL}/releases`;
export const RELEASES_LATEST = `${GITHUB_URL}/releases/latest`;
export const ISSUES_URL = `${GITHUB_URL}/issues`;

export const VSCODE_EXT_ID = "betabots.callimachus-vscode";
export const MARKETPLACE_URL = `https://marketplace.visualstudio.com/items?itemName=${VSCODE_EXT_ID}`;
export const OPENVSX_URL = "https://open-vsx.org/extension/betabots/callimachus-vscode";

export const CONTACT_EMAIL = "ari@shaller.dev";
export const COMMERCIAL_MAILTO = `mailto:${CONTACT_EMAIL}?subject=Callimachus%20commercial%20license`;

export const TAGLINE = "Your AI coding history, finally catalogued.";
export const DESCRIPTION =
  "Callimachus indexes and searches your AI coding-agent conversations — across Claude Code, Codex, Cursor, Gemini and 7 more — in one fast, local, private catalogue. Desktop app, CLI, editor extension, and MCP server.";

// The 11 indexed agents, numbered like catalogue entries.
export const SOURCES = [
  "Claude Code",
  "Codex",
  "Cursor",
  "Gemini CLI",
  "Qwen Code",
  "Goose",
  "OpenCode",
  "Continue",
  "Cline",
  "Roo Code",
  "Kilo Code",
] as const;

export type ProductSlug = "desktop" | "vscode" | "cli" | "mcp";

export interface Product {
  slug: ProductSlug;
  no: string;
  name: string;
  tagline: string;
  blurb: string;
}

export const PRODUCTS: Product[] = [
  {
    slug: "desktop",
    no: "01",
    name: "Desktop app",
    tagline: "The reading room.",
    blurb:
      "A native app for macOS, Windows, and Linux. Browse, search, and read every thread; chat over your own history; export to Obsidian. Auto-updates.",
  },
  {
    slug: "vscode",
    no: "02",
    name: "VS Code & Cursor",
    tagline: "Search without leaving the editor.",
    blurb:
      "A sidebar and transcript tabs inside your editor. Same local index, no context-switch. Works in VS Code, Cursor, and VSCodium.",
  },
  {
    slug: "cli",
    no: "03",
    name: "cal CLI",
    tagline: "Your history, pipeable.",
    blurb:
      "`cal search`, `cal recent`, `cal cat` — grep your past sessions from the terminal and pipe context straight into the next agent.",
  },
  {
    slug: "mcp",
    no: "04",
    name: "MCP server",
    tagline: "Give every agent a memory.",
    blurb:
      "Expose your indexed history to any MCP client. Agents can search your past work and pull the thread they need, on demand.",
  },
];

export const NAV = [
  { label: "Download", href: "/download" },
  { label: "Desktop", href: "/desktop" },
  { label: "Editor", href: "/vscode" },
  { label: "CLI", href: "/cli" },
  { label: "MCP", href: "/mcp" },
  { label: "Pricing", href: "/pricing" },
] as const;
