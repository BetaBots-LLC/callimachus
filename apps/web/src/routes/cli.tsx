import { Link, createFileRoute } from "@tanstack/react-router";
import { seo } from "@/lib/seo";
import { ldScript, softwareApplicationLd } from "@/lib/jsonld";
import { SITE_URL } from "@/lib/site";
import { ProductLayout } from "@/components/site/ProductLayout";
import { CommandBlock } from "@/components/site/CommandBlock";

export const Route = createFileRoute("/cli")({
  head: () => ({
    meta: seo({
      title: "cal: the Callimachus CLI",
      description:
        "Search your AI coding history from the terminal. cal search, recent, cat, ask, files, commits, why, memory, remember, check, snapshot, resume, stats, and export. Pipe-friendly, and it reads the same local index as the desktop app.",
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
        <div className="flex min-w-0 flex-col gap-4">
          <p className="cat-label">Install</p>
          <p className="text-sm leading-relaxed text-muted-foreground">
            Easiest path: install the{" "}
            <Link to="/download" className="text-link hover:underline">
              desktop app
            </Link>{" "}
            and click Enable for Claude Code in Settings &gt; Agents (Install the cal CLI when the
            Claude Code plugin is on), or Connect in its setup checklist. That links{" "}
            <code className="font-mono">cal</code> at{" "}
            <code className="font-mono">~/.local/bin/cal</code> on macOS and Linux (stock macOS
            leaves <code className="font-mono">~/.local/bin</code> off your PATH; Settings shows the
            line to add) and places <code className="font-mono">cal.exe</code> next to the app on
            Windows. Same local index, nothing else to set up.
          </p>
          <p className="cat-label pt-2">Without the app</p>
          <p className="text-sm leading-relaxed text-muted-foreground">
            On macOS and Linux, the npm launcher downloads <code className="font-mono">cal</code>{" "}
            from the same release as the MCP server, checks its SHA-256 and links it at{" "}
            <code className="font-mono">~/.local/bin/cal</code>. It never replaces another program's{" "}
            <code className="font-mono">cal</code>.
          </p>
          <CommandBlock
            label="needs Node.js 18 or later"
            lines={["npx callimachus-mcp --install-cal"]}
          />
          <p className="cat-label pt-2">Or build from source</p>
          <CommandBlock
            label="from a checkout of the repo (add --force over an earlier source install)"
            lines={["cargo install --locked --path apps/desktop/src-tauri/standalone --bin cal"]}
          />
        </div>

        <div className="flex min-w-0 flex-col gap-4">
          <p className="cat-label">Use it</p>
          <CommandBlock
            label="examples"
            lines={[
              "# semantic + keyword search, newest-best first",
              'cal search "vector index migration" -y',
              "# the most recent threads",
              "cal recent -n 10",
              "# packed transcript to stdout, ready to pipe anywhere",
              "cal cat 42 | pbcopy",
              "# seed another agent with past context",
              'claude "$(cal cat 42)"',
              "# a cited answer synthesized from your own history (RAG)",
              'cal ask "how did we handle the write-lock contention?"',
              "# every thread that touched a file path",
              "cal files embed/mod.rs",
              "# the thread→commit timeline: which commits each conversation produced",
              "cal commits",
              "# why a line is the way it is: the commit behind it, its sessions and their decisions",
              "cal why src/auth.rs:42",
              "# Agent Trace records (agent-trace.dev) for the commits your sessions produced",
              "cal trace export --dry-run",
              "# recurring errors you keep hitting across every tool, and the commit that fixed each",
              "cal issues",
              "# the generic ones too (a bare `error:`, a CI exit code)",
              "cal issues --all",
              "# estimated $ spend by model + your priciest threads",
              "cal cost",
              "# what the sessions behind each recent commit cost (also --by thread, project or day)",
              "cal cost --by commit --since 30",
              "# your agents, so far: sessions, hours, spend, recurring errors and memory, as markdown",
              "cal report",
              "# one JSON bundle for an external PR auditor (provenance + file history + errors)",
              "cal audit-pr . --changed-files src/auth.rs --shas $(git rev-parse HEAD)",
              "# a project's durable memory (decisions / gotchas / open TODOs)",
              "cal memory",
              "# share it with your team: write .callimachus/memory/ in the repo, for you to commit",
              "cal memory publish",
              "# what agents flagged, disputed, replaced or recorded in memory that nobody reviewed",
              "cal memory review",
              "# record a decision, with the reasoning behind it",
              'cal remember decision "use sqlite-vec for the KNN index" --because "fewest deps, ships in-process"',
              "# surface settled decisions before re-litigating one",
              'cal check "switch the KNN index to faiss"',
              "# checkpoint a thread, then resume it in another agent later",
              "cal snapshot 42 -l pre-refactor",
              "# close out a leftover TODO",
              "cal done 137",
              "# inject the repo's memory into AGENTS.md so any agent reads it",
              "cal agents",
              "# what a new agent session in this repo gets injected, or why it gets nothing",
              "cal hook --explain",
              "# what decides whether it all works here, ready to paste into a bug report",
              "cal doctor",
            ]}
          />
        </div>
      </div>

      <div className="mt-12 border-t border-border pt-8">
        <p className="max-w-[60ch] leading-relaxed text-muted-foreground">
          The query commands take <code className="font-mono">--json</code> for scripting,{" "}
          <code className="font-mono">-s</code> to filter by source, and{" "}
          <code className="font-mono">-p</code> to scope to a project; search, recent, and related
          also take <code className="font-mono">--starred</code> and{" "}
          <code className="font-mono">-t TAG</code>. There's also{" "}
          <code className="font-mono">cal stats</code> for a corpus overview,{" "}
          <code className="font-mono">cal export</code> to write an Obsidian note, and{" "}
          <code className="font-mono">cal related</code> to find threads near some text.
        </p>
        <p className="mt-4 max-w-[60ch] leading-relaxed text-muted-foreground">
          For a contract you can build on, pass <code className="font-mono">--json-version 1</code>{" "}
          (or set <code className="font-mono">CAL_JSON_VERSION=1</code> beside{" "}
          <code className="font-mono">--json</code>). Then search, recent, related, cat, stats,
          memory, decisions, gotchas, commits, why, trace, cost, report, issues, audit-pr, and
          doctor open with <code className="font-mono">schema_version</code>, a list comes under{" "}
          <code className="font-mono">results</code>, and each shape has a published JSON Schema at{" "}
          <code className="font-mono">/schemas/cal/&lt;command&gt;.v1.json</code>, such as{" "}
          <a href="/schemas/cal/search.v1.json" className="text-link hover:underline">
            search.v1.json
          </a>
          . New fields can appear within a version; a removed or changed one gets a new version.
          Plain <code className="font-mono">--json</code>, and every other command's JSON, keeps its
          unversioned shape, which may change in a future major release.
        </p>
        <p className="mt-4 max-w-[60ch] leading-relaxed text-muted-foreground">
          Something not working? <code className="font-mono">cal doctor</code> reports the index and
          its schema, the locks next to it, each agent's hooks and MCP entry, the hooks that gave up
          in the last week and why, and whether each agent's transcripts still parse. It changes
          nothing, shows your home folder as <code className="font-mono">~</code>, and holds no
          project text, so it can go straight into a bug report.
        </p>
        <p className="mt-4 max-w-[60ch] leading-relaxed text-muted-foreground">
          No desktop app on this machine? <code className="font-mono">cal index</code> builds the
          index from every agent's history, <code className="font-mono">cal watch</code> keeps it
          fresh as agents write, <code className="font-mono">cal embed</code> builds the semantic
          index (it downloads the on-device model once;{" "}
          <code className="font-mono">--no-download</code> uses only a model already there).{" "}
          <code className="font-mono">cal distill --auto</code> distills recent sessions under the
          daily cap with the memory engine you turned on in the app's Settings, so it needs the app
          once; after that it runs with the app closed. Only one process watches an index:{" "}
          <code className="font-mono">cal watch</code> won't start while the app is open, and the
          app leaves live indexing to a running <code className="font-mono">cal watch</code>.{" "}
          <code className="font-mono">cal backup</code> saves a copy of the index in the snapshots
          folder next to it (a free clone on APFS, a full copy elsewhere), or with{" "}
          <code className="font-mono">--to DIR</code> wherever you like.
        </p>
        <p className="mt-4 max-w-[60ch] leading-relaxed text-muted-foreground">
          <code className="font-mono">cal commits</code> infers, entirely on-device, which git
          commits a thread produced, by overlapping the files a thread discussed with{" "}
          <code className="font-mono">git log</code>'s changed files inside the thread's time
          window, where the shared-file count is the confidence. Run it inside a git repo or pass a
          path; it prints the thread→commit timeline, one row per commit with its linked-thread
          count. <code className="font-mono">cal why src/auth.rs:42</code> goes the other way:{" "}
          <code className="font-mono">git blame</code> names the commit behind the line, and you get
          the sessions that produced it, the ones that edited the file first, with their decisions
          and gotchas. Leave off the line for the commits behind the whole file. When the repo
          carries{" "}
          <a href="https://agent-trace.dev" className="text-link hover:underline">
            Agent Trace
          </a>{" "}
          records, the answer also names the agent and conversation another tool recorded for the
          line, and <code className="font-mono">cal trace export</code> writes records for the
          commits your own sessions produced.
        </p>
        <p className="mt-4 max-w-[60ch] leading-relaxed text-muted-foreground">
          Curate as you go with <code className="font-mono">cal star</code>,{" "}
          <code className="font-mono">cal tag</code>, and{" "}
          <code className="font-mono">cal tags</code>; surface leftover work with{" "}
          <code className="font-mono">cal todos</code>. With memory turned on in the app (your
          logged-in Claude Code or Codex CLI, local Ollama, or an API key),{" "}
          <code className="font-mono">cal distill</code> and{" "}
          <code className="font-mono">cal knowledge</code> pull a thread's summary, decisions, and
          gotchas, and <code className="font-mono">cal decisions</code> /{" "}
          <code className="font-mono">cal gotchas</code> recall them semantically across your whole
          history.
        </p>
        <p className="mt-4 max-w-[60ch] leading-relaxed text-muted-foreground">
          Each project keeps a durable memory: <code className="font-mono">cal memory</code> prints
          its aggregated decisions, gotchas, and open TODOs (defaults to the current repo), and{" "}
          <code className="font-mono">cal hook --explain</code> shows what a new agent session there
          would get: that memory, or a digest of the repo's recent sessions while it has none. It
          writes back too: <code className="font-mono">cal remember decision|gotcha</code> pins a
          new fact into that memory (add <code className="font-mono">--because "&lt;why&gt;"</code>{" "}
          to a decision to keep its rationale), and <code className="font-mono">cal done</code>{" "}
          closes a leftover TODO. These only ever touch Callimachus's own index and memory, never
          your files. The one exception is yours to run:{" "}
          <code className="font-mono">cal memory publish</code> writes the repo's pinned and
          recorded facts into <code className="font-mono">.callimachus/memory/</code> for you to
          review and commit, and everyone with Callimachus who works in the repo imports them as
          notes from the repo (<code className="font-mono">cal memory import --review</code> shows
          what an import would change).
        </p>
        <p className="mt-4 max-w-[60ch] leading-relaxed text-muted-foreground">
          A contradiction guard keeps agents honest:{" "}
          <code className="font-mono">cal check "&lt;proposal&gt;"</code> surfaces the settled
          decisions on a topic before one gets re-litigated. And for handoff across a context-window
          compaction or between tools,{" "}
          <code className="font-mono">cal snapshot &lt;thread-id&gt;</code> (optionally{" "}
          <code className="font-mono">-l LABEL</code>) saves a durable, resumable checkpoint (a
          packed transcript plus carry-forward project memory);{" "}
          <code className="font-mono">cal snapshots</code> lists them and{" "}
          <code className="font-mono">cal resume &lt;id&gt;</code> (optionally{" "}
          <code className="font-mono">-a AGENT</code>) picks one back up.
        </p>
      </div>
    </ProductLayout>
  );
}
