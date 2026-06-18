import { Link } from "@tanstack/react-router";
import { Container } from "./Container";
import { Logo } from "./Logo";
import { CONTACT_EMAIL, GITHUB_URL, ISSUES_URL, MARKETPLACE_URL, OPENVSX_URL } from "@/lib/site";

const COLUMNS: { title: string; links: { label: string; to?: string; href?: string }[] }[] = [
  {
    title: "Get it",
    links: [
      { label: "Download", to: "/download" },
      { label: "Desktop app", to: "/desktop" },
      { label: "VS Code & Cursor", to: "/vscode" },
      { label: "cal CLI", to: "/cli" },
      { label: "MCP server", to: "/mcp" },
    ],
  },
  {
    title: "Project",
    links: [
      { label: "GitHub", href: GITHUB_URL },
      { label: "Issues", href: ISSUES_URL },
      { label: "Marketplace", href: MARKETPLACE_URL },
      { label: "Open VSX", href: OPENVSX_URL },
      { label: "Pricing", to: "/pricing" },
    ],
  },
  {
    title: "Legal",
    links: [
      { label: "Terms", to: "/terms" },
      { label: "Privacy", to: "/privacy" },
      { label: "Contact", href: `mailto:${CONTACT_EMAIL}` },
    ],
  },
];

export function Footer() {
  return (
    <footer className="mt-32 border-t border-border">
      <Container className="grid gap-12 py-16 sm:grid-cols-2 lg:grid-cols-[1.5fr_repeat(3,1fr)]">
        <div className="max-w-xs">
          <Logo />
          <p className="mt-4 text-sm leading-relaxed text-muted-foreground">
            The card catalogue for your AI coding history. Local, fast, and quietly yours.
          </p>
        </div>

        {COLUMNS.map((col) => (
          <div key={col.title}>
            <p className="cat-label">{col.title}</p>
            <ul className="mt-4 space-y-2.5">
              {col.links.map((l) => (
                <li key={l.label}>
                  {l.to ? (
                    <Link
                      to={l.to}
                      className="text-sm text-muted-foreground transition-colors hover:text-foreground"
                    >
                      {l.label}
                    </Link>
                  ) : (
                    <a
                      href={l.href}
                      target={l.href?.startsWith("http") ? "_blank" : undefined}
                      rel="noreferrer"
                      className="text-sm text-muted-foreground transition-colors hover:text-foreground"
                    >
                      {l.label}
                    </a>
                  )}
                </li>
              ))}
            </ul>
          </div>
        ))}
      </Container>

      <Container className="flex flex-col gap-2 border-t border-border py-6 text-xs text-muted-foreground sm:flex-row sm:items-center sm:justify-between">
        <p>© 2026 Ari Shaller · Free under AGPL-3.0</p>
        <p className="font-mono">Named for the first librarian to catalogue Alexandria.</p>
      </Container>
    </footer>
  );
}
