import { createFileRoute } from "@tanstack/react-router";
import { seo } from "@/lib/seo";
import { ldScript, softwareApplicationLd } from "@/lib/jsonld";
import { SITE_URL } from "@/lib/site";
import { ProductLayout } from "@/components/site/ProductLayout";
import { CommandBlock } from "@/components/site/CommandBlock";

export const Route = createFileRoute("/mcp")({
  head: () => ({
    meta: seo({
      title: "Callimachus MCP server — give agents a memory",
      description:
        "An MCP server that exposes your indexed AI coding history to any client. Agents can search your past work and pull the thread they need, on demand.",
      path: "/mcp",
    }),
    links: [{ rel: "canonical", href: `${SITE_URL}/mcp` }],
    scripts: [ldScript(softwareApplicationLd())],
  }),
  component: McpPage,
});

const TOOLS = [
  { name: "search_threads", note: "Search the whole index" },
  { name: "search_current_project", note: "Scope search to the working repo" },
  { name: "recent_threads", note: "The most recently updated threads" },
  { name: "get_thread", note: "Fetch a full thread by id" },
];

function McpPage() {
  return (
    <ProductLayout
      no="04"
      kicker="MCP server"
      title="Give every agent a memory."
      description="callimachus-mcp exposes your local history to any MCP client. Instead of re-explaining context, your agent can look it up — searching your own past sessions and pulling the exact thread it needs."
    >
      <div className="grid gap-10 lg:grid-cols-2">
        <div className="flex flex-col gap-4">
          <p className="cat-label">Install &amp; connect</p>
          <CommandBlock
            label="terminal"
            lines={[
              "cargo install --path apps/desktop/src-tauri --bin callimachus-mcp",
              "# register it with your client (Claude Code shown)",
              "claude mcp add callimachus -- callimachus-mcp",
            ]}
          />
          <p className="text-sm leading-relaxed text-muted-foreground">
            Reads the same local index as the app — no separate database, no extra indexing. Ships a{" "}
            <code className="font-mono">/recall</code> skill that teaches agents when to reach for
            it.
          </p>
        </div>

        <div className="flex flex-col gap-4">
          <p className="cat-label">Tools it exposes</p>
          <ul className="border-t border-border">
            {TOOLS.map((t) => (
              <li
                key={t.name}
                className="flex flex-wrap items-baseline justify-between gap-x-4 gap-y-1 border-b border-border py-3.5"
              >
                <code className="font-mono text-sm text-link">{t.name}</code>
                <span className="text-sm text-muted-foreground">{t.note}</span>
              </li>
            ))}
          </ul>
        </div>
      </div>
    </ProductLayout>
  );
}
