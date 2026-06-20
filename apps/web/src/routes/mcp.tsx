import { Link, createFileRoute } from "@tanstack/react-router";
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

const TOOLS: { name: string; note: string; write?: boolean }[] = [
  { name: "search_threads", note: "Keyword + optional semantic search of the whole index" },
  { name: "search_current_project", note: "Scope search to the launching repo" },
  { name: "recent_threads", note: "The most recently updated threads" },
  { name: "get_thread", note: "Fetch a full thread as a packed transcript" },
  { name: "list_tags", note: "Discover the user's tags / collections" },
  { name: "list_open_todos", note: "Unfinished TODOs across past sessions (no key needed)" },
  { name: "get_thread_knowledge", note: "Distilled summary, decisions, gotchas for a thread" },
  { name: "recall_decisions", note: "Semantically recall past decisions and why" },
  { name: "recall_gotchas", note: "Semantically recall known pitfalls to avoid" },
  { name: "project_memory", note: "A project's durable memory: decisions, gotchas, open TODOs" },
  { name: "ask_history", note: "A synthesized, cited answer over your own history (RAG)" },
  { name: "threads_for_file", note: "Which past sessions touched a file path" },
  { name: "complete_todo", note: "Mark an open TODO done — persists across re-index", write: true },
  { name: "record_decision", note: "Persist a decision into a project's memory", write: true },
  { name: "record_gotcha", note: "Persist a gotcha into a project's memory", write: true },
];

function McpPage() {
  return (
    <ProductLayout
      no="04"
      kicker="MCP server"
      title="Give every agent a memory."
      description="callimachus-mcp exposes your local history to any MCP client through fifteen tools — and now reads and writes the memory layer. Instead of re-explaining context, your agent can look it up: searching past sessions, recalling settled decisions and known gotchas, asking your history a cited question, and pulling the exact thread it needs. It can also write back — closing TODOs and recording new decisions and gotchas — but only ever touches Callimachus's own index and memory, never your files."
    >
      <div className="grid gap-10 lg:grid-cols-2">
        <div className="flex flex-col gap-4">
          <p className="cat-label">Install &amp; connect</p>
          <p className="text-sm leading-relaxed text-muted-foreground">
            Install the{" "}
            <Link to="/download" className="text-link hover:underline">
              desktop app
            </Link>{" "}
            — it ships <code className="font-mono">callimachus-mcp</code> on your PATH. Then
            register it with your client:
          </p>
          <CommandBlock
            label="register (Claude Code shown)"
            lines={["claude mcp add callimachus -- callimachus-mcp"]}
          />
          <p className="cat-label pt-2">Or build from source</p>
          <CommandBlock
            label="from a checkout of the repo"
            lines={["cargo install --path apps/desktop/src-tauri --bin callimachus-mcp"]}
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
                <code className="font-mono text-sm text-link">
                  {t.name}
                  {t.write && (
                    <span className="ml-2 align-middle font-mono text-[10px] uppercase tracking-wide text-amber-500">
                      write
                    </span>
                  )}
                </code>
                <span className="text-sm text-muted-foreground">{t.note}</span>
              </li>
            ))}
          </ul>
          <p className="text-sm leading-relaxed text-muted-foreground">
            Twelve read tools and three <span className="font-mono text-amber-500">write</span>{" "}
            tools (<code className="font-mono">complete_todo</code>,{" "}
            <code className="font-mono">record_decision</code>,{" "}
            <code className="font-mono">record_gotcha</code>) that update Callimachus's own memory —
            never your project files.
          </p>
        </div>
      </div>
    </ProductLayout>
  );
}
