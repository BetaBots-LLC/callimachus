import { Link, createFileRoute } from "@tanstack/react-router";
import { seo } from "@/lib/seo";
import { ldScript, softwareApplicationLd } from "@/lib/jsonld";
import { SITE_URL } from "@/lib/site";
import { ProductLayout } from "@/components/site/ProductLayout";
import { CommandBlock } from "@/components/site/CommandBlock";

export const Route = createFileRoute("/cli")({
  head: () => ({
    meta: seo({
      title: "cal — the Callimachus CLI",
      description:
        "Search your AI coding history from the terminal. cal search, recent, cat, stats, and export — pipe-friendly, reading the same local index as the desktop app.",
      path: "/cli",
    }),
    links: [{ rel: "canonical", href: `${SITE_URL}/cli` }],
    scripts: [ldScript(softwareApplicationLd())],
  }),
  component: CliPage,
});

function CliPage() {
  return (
    <ProductLayout
      no="03"
      kicker="cal CLI"
      title="Your history, pipeable."
      description="A small, fast terminal client over the same local index. Grep your past sessions, pull a packed transcript to stdout, and feed it straight into the next agent."
    >
      <div className="grid gap-10 lg:grid-cols-2">
        <div className="flex flex-col gap-4">
          <p className="cat-label">Install</p>
          <p className="text-sm leading-relaxed text-muted-foreground">
            Easiest path: install the{" "}
            <Link to="/download" className="text-link hover:underline">
              desktop app
            </Link>{" "}
            — it puts <code className="font-mono">cal</code> on your PATH, ready to use. Same local
            index, nothing else to set up.
          </p>
          <p className="cat-label pt-2">Or build from source</p>
          <CommandBlock
            label="from a checkout of the repo"
            lines={["cargo install --path apps/desktop/src-tauri --bin cal"]}
          />
        </div>

        <div className="flex flex-col gap-4">
          <p className="cat-label">Use it</p>
          <CommandBlock
            label="examples"
            lines={[
              "# semantic + keyword search, newest-best first",
              'cal search "vector index migration" -y',
              "# the most recent threads",
              "cal recent -n 10",
              "# packed transcript to stdout — pipe it anywhere",
              "cal cat 42 | pbcopy",
              "# seed another agent with past context",
              'claude "$(cal cat 42)"',
            ]}
          />
        </div>
      </div>

      <div className="mt-12 border-t border-border pt-8">
        <p className="max-w-[60ch] leading-relaxed text-muted-foreground">
          Every command takes <code className="font-mono">--json</code> for scripting,{" "}
          <code className="font-mono">-s</code> to filter by source, and{" "}
          <code className="font-mono">-p</code> to scope to a project. There's also{" "}
          <code className="font-mono">cal stats</code> for a corpus overview and{" "}
          <code className="font-mono">cal export</code> to write an Obsidian note.
        </p>
      </div>
    </ProductLayout>
  );
}
