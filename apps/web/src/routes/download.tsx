import { Link, createFileRoute } from "@tanstack/react-router";
import { ArrowUpRight, Puzzle, Share2, Terminal } from "lucide-react";
import { downloadData } from "@/server/releases";
import { seo } from "@/lib/seo";
import { ldScript, softwareApplicationLd } from "@/lib/jsonld";
import { SITE_URL } from "@/lib/site";
import { Container } from "@/components/site/Container";
import { DownloadButton } from "@/components/download/DownloadButton";
import { DownloadMatrix } from "@/components/download/DownloadMatrix";

export const Route = createFileRoute("/download")({
  loader: () => downloadData(),
  head: ({ loaderData }) => ({
    meta: seo({
      title: "Download Callimachus — macOS, Windows & Linux",
      description:
        "Download the Callimachus desktop app for macOS, Windows, or Linux. Free and open source. Also available as a VS Code/Cursor extension, a CLI, and an MCP server.",
      path: "/download",
    }),
    links: [{ rel: "canonical", href: `${SITE_URL}/download` }],
    scripts: [ldScript(softwareApplicationLd({ version: loaderData?.release.version }))],
  }),
  component: DownloadPage,
});

const OTHER = [
  { icon: Puzzle, name: "VS Code & Cursor", to: "/vscode", note: "Search inside your editor" },
  { icon: Terminal, name: "cal CLI", to: "/cli", note: "Pipe history from the terminal" },
  { icon: Share2, name: "MCP server", to: "/mcp", note: "Give agents your history" },
] as const;

function DownloadPage() {
  const { release, primaryOs } = Route.useLoaderData();

  return (
    <Container className="py-16 sm:py-20">
      <div className="max-w-2xl">
        <span className="cat-label text-primary">№ — accession</span>
        <h1 className="mt-3 text-balance text-4xl text-foreground sm:text-5xl">
          Take a copy home.
        </h1>
        <p className="mt-4 text-lg leading-relaxed text-muted-foreground">
          The desktop app bundles everything — the index, the search, and the{" "}
          <code className="rounded bg-card px-1.5 py-0.5 font-mono text-[0.85em]">cal</code> CLI.
          Pick your platform; it auto-updates from there.
        </p>
        <div className="mt-8">
          <DownloadButton release={release} primaryOs={primaryOs} showAll={false} />
        </div>
      </div>

      <div className="mt-14 grid gap-14 lg:grid-cols-[1.3fr_0.7fr]">
        <div>
          <p className="cat-label mb-4">All builds</p>
          <DownloadMatrix release={release} />
        </div>

        <aside className="flex flex-col gap-4">
          <p className="cat-label">Other reading desks</p>
          <ul className="flex flex-col gap-2">
            {OTHER.map((o) => {
              const Icon = o.icon;
              return (
                <li key={o.name}>
                  <Link
                    to={o.to}
                    className="group flex items-center gap-3 rounded-md border border-border bg-card px-4 py-3 transition-colors hover:border-foreground/20"
                  >
                    <Icon className="size-5 text-primary" />
                    <span className="flex-1">
                      <span className="block text-sm text-foreground">{o.name}</span>
                      <span className="block font-mono text-xs text-muted-foreground">
                        {o.note}
                      </span>
                    </span>
                    <ArrowUpRight className="size-4 text-muted-foreground transition-transform duration-200 ease-[var(--ease-out-quint)] group-hover:-translate-y-0.5 group-hover:translate-x-0.5 group-hover:text-link" />
                  </Link>
                </li>
              );
            })}
          </ul>
          <div className="mt-2 rounded-md border border-border p-4">
            <p className="cat-label mb-2">Requires</p>
            <p className="text-sm leading-relaxed text-muted-foreground">
              macOS 12+, Windows 10+, or a modern Linux desktop. The first run downloads a small
              embedding model for semantic search; everything else is offline.
            </p>
          </div>
        </aside>
      </div>
    </Container>
  );
}
