import { Link, createFileRoute } from "@tanstack/react-router";
import { seo } from "@/lib/seo";
import { ldScript, softwareApplicationLd } from "@/lib/jsonld";
import { SITE_URL } from "@/lib/site";
import { ProductLayout } from "@/components/site/ProductLayout";
import { CommandBlock } from "@/components/site/CommandBlock";

export const Route = createFileRoute("/mcp")({
  head: () => ({
    meta: seo({
      title: "Callimachus MCP server: give agents a memory",
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
  {
    name: "get_thread",
    note: "A thread as a packed transcript, trimmed to a budget (24k chars by default)",
  },
  { name: "list_tags", note: "Discover the user's tags / collections" },
  { name: "list_open_todos", note: "Unfinished TODOs across past sessions (no key needed)" },
  { name: "get_thread_knowledge", note: "Distilled summary, decisions, gotchas for a thread" },
  { name: "recall_decisions", note: "Semantically recall past decisions and why" },
  { name: "recall_gotchas", note: "Semantically recall known pitfalls to avoid" },
  {
    name: "find_prior_work",
    note: "Prior sessions similar to a task: the 'have I done this?' guard",
  },
  {
    name: "project_memory",
    note: "A project's decisions, gotchas and open TODOs as budgeted markdown (or JSON)",
  },
  { name: "ask_history", note: "A synthesized, cited answer over your own history (RAG)" },
  { name: "threads_for_file", note: "Which past sessions touched a file path" },
  {
    name: "check_decision",
    note: "Surface prior decisions, dated, before re-litigating a proposal; reversed ones name their replacement",
  },
  { name: "linked_commits", note: "The git commits a thread likely produced" },
  {
    name: "recurring_issues",
    note: "Errors you keep hitting across sessions, grouped by signature (last 180 days)",
  },
  { name: "list_snapshots", note: "Resumable session snapshots for a project" },
  {
    name: "load_snapshot",
    note: "A snapshot's carry-forward memory + transcript, trimmed to a budget",
  },
  {
    name: "complete_todo",
    note: "Mark an open TODO done; it stays done across a re-index",
    write: true,
  },
  {
    name: "record_decision",
    note: "Persist a decision (with optional rationale) into memory, or the one that replaces an older decision",
    write: true,
  },
  { name: "record_gotcha", note: "Persist a gotcha into a project's memory", write: true },
  {
    name: "flag_fact",
    note: "Flag a remembered fact as stale or wrong; it leaves memory until you review it",
    write: true,
  },
  {
    name: "snapshot_session",
    note: "Checkpoint a thread for handoff across compaction or tools",
    write: true,
  },
];

function McpPage() {
  return (
    <ProductLayout
      no="04"
      kicker="MCP server"
      title="Give every agent a memory."
      description="callimachus-mcp exposes your local history to any MCP client through twenty-three tools that read and write the memory layer. Instead of re-explaining context, your agent can look it up: searching past sessions, recalling settled decisions and known gotchas, asking your history a cited question, linking threads to the git commits they produced, and pulling the exact thread it needs. It can also write back by closing TODOs, recording new decisions and gotchas (and which older decision one replaces), flagging a remembered fact the code shows is stale or wrong, and snapshotting a session for handoff across a context-window compaction or another tool. It only ever touches Callimachus's own index and memory, never your files."
    >
      <div className="grid gap-10 lg:grid-cols-2">
        <div className="flex min-w-0 flex-col gap-4">
          <p className="cat-label">Install &amp; connect</p>
          <p className="text-sm leading-relaxed text-muted-foreground">
            Install the{" "}
            <Link to="/download" className="text-link hover:underline">
              desktop app
            </Link>{" "}
            and open it once so it indexes your history; the server reads that index. In Claude
            Code, the quickest way in is the Callimachus plugin, which bundles this server with the{" "}
            <code className="font-mono">/callimachus:recall</code> skill and the memory hooks.
          </p>
          <CommandBlock
            label="Claude Code plugin (then /plugin install callimachus@callimachus)"
            lines={["claude plugin marketplace add BetaBots-LLC/callimachus"]}
          />
          <p className="text-sm leading-relaxed text-muted-foreground">
            Then click Install the cal CLI in the app's Settings &gt; Agents so the hooks can run.
            On macOS and Linux the app installs only that CLI while the plugin is on, so nothing
            loads twice. On Windows, skip the plugin and click Enable for Claude Code there instead:
            the plugin's hooks need Git Bash, and the app keeps its own install.
          </p>
          <p className="cat-label pt-2">The plugin without the app</p>
          <p className="text-sm leading-relaxed text-muted-foreground">
            On macOS and Linux the plugin works with no desktop app at all. Install the{" "}
            <code className="font-mono">cal</code> CLI from the same release and build the index
            once; after that the plugin's SessionStart hook refreshes the index in the background
            whenever no full pass finished in the last hour, and{" "}
            <code className="font-mono">cal watch</code> keeps it fresh to the minute if you want.
            You give up the app's Projects page, Chat, Settings and the harvest controls, so memory
            distillation stays off; search, recall and the memory hooks work the same.
          </p>
          <CommandBlock
            label="then cal embed for semantic search (downloads a 128 MB model once)"
            lines={["npx callimachus-mcp --install-cal", "cal index"]}
          />
          <p className="text-sm leading-relaxed text-muted-foreground">
            Without the plugin, Enable for Claude Code (or Connect in the setup checklist) registers
            the app itself as Claude Code's MCP server, and Other agents in the same section does
            the same for Codex, Cursor and Gemini CLI. For any other client, register the npm
            launcher, which downloads the prebuilt server and checks its SHA-256:
          </p>
          <CommandBlock
            label="register (Claude Code shown)"
            lines={["claude mcp add callimachus -- npx -y callimachus-mcp"]}
          />
          <p className="cat-label pt-2">Or build from source</p>
          <CommandBlock
            label="from a checkout of the repo (add --force over an earlier source install)"
            lines={[
              "cargo install --locked --path apps/desktop/src-tauri/standalone --bin callimachus-mcp",
            ]}
          />
          <p className="text-sm leading-relaxed text-muted-foreground">
            Reads the same local index as the app: no separate database, no extra indexing. Ships a{" "}
            <code className="font-mono">/recall</code> skill that teaches agents when to reach for
            it.
          </p>
        </div>

        <div className="flex min-w-0 flex-col gap-4">
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
            Eighteen read tools and five <span className="font-mono text-amber-500">write</span>{" "}
            tools (<code className="font-mono">complete_todo</code>,{" "}
            <code className="font-mono">record_decision</code>,{" "}
            <code className="font-mono">record_gotcha</code>,{" "}
            <code className="font-mono">flag_fact</code>,{" "}
            <code className="font-mono">snapshot_session</code>) that update Callimachus's own
            memory, never your project files. Every remembered fact carries the date its session
            happened, a decision a later session reversed is kept as history and named as replaced,
            and a flag from an agent hides a fact only until you review it (a fact you pinned stays
            in).
          </p>
          <p className="text-sm leading-relaxed text-muted-foreground">
            It also offers <code className="font-mono">recall</code>,{" "}
            <code className="font-mono">check_decision</code> and{" "}
            <code className="font-mono">prior_work</code> as prompts (slash commands in Claude Code)
            and project memory as the <code className="font-mono">callimachus://memory</code>{" "}
            resource, and when a repo isn't distilled yet it answers with its closest past sessions
            instead of an empty list.
          </p>
          <p className="text-sm leading-relaxed text-muted-foreground">
            Every tool declares an output schema and returns structured content that matches it, so
            a client can rely on the shape: lists under a key, markdown under{" "}
            <code className="font-mono">markdown</code>, and setup guidance as{" "}
            <code className="font-mono">notice</code> when the index can't answer yet. The schemas
            are published at <code className="font-mono">/schemas/mcp/&lt;tool&gt;.json</code>, such
            as{" "}
            <a href="/schemas/mcp/recall_decisions.json" className="text-link hover:underline">
              recall_decisions.json
            </a>
            . The text block reads the same for every client; one older than MCP 2025-06-18 gets no
            structured content.
          </p>
        </div>
      </div>
    </ProductLayout>
  );
}
