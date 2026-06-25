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
        "Search your AI coding history from the terminal. cal search, recent, cat, ask, files, commits, memory, remember, check, snapshot, resume, stats, and export — pipe-friendly, reading the same local index as the desktop app.",
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
              "# a cited answer synthesized from your own history (RAG)",
              'cal ask "how did we handle the write-lock contention?"',
              "# every thread that touched a file path",
              "cal files embed/mod.rs",
              "# the thread→commit timeline — which commits each conversation produced",
              "cal commits",
              "# recurring errors you keep hitting across every tool (last 180 days)",
              "cal issues",
              "# estimated $ spend by model + your priciest threads",
              "cal cost",
              "# one JSON bundle for an external PR auditor (provenance + file history + errors)",
              "cal audit-pr . --changed-files src/auth.rs --shas $(git rev-parse HEAD)",
              "# a project's durable memory (decisions / gotchas / open TODOs)",
              "cal memory",
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
            ]}
          />
        </div>
      </div>

      <div className="mt-12 border-t border-border pt-8">
        <p className="max-w-[60ch] leading-relaxed text-muted-foreground">
          Every command takes <code className="font-mono">--json</code> for scripting,{" "}
          <code className="font-mono">-s</code> to filter by source, and{" "}
          <code className="font-mono">-p</code> to scope to a project; search, recent, and related
          also take <code className="font-mono">--starred</code> and{" "}
          <code className="font-mono">-t TAG</code>. There's also{" "}
          <code className="font-mono">cal stats</code> for a corpus overview,{" "}
          <code className="font-mono">cal export</code> to write an Obsidian note, and{" "}
          <code className="font-mono">cal related</code> to find threads near some text.
        </p>
        <p className="mt-4 max-w-[60ch] leading-relaxed text-muted-foreground">
          <code className="font-mono">cal commits</code> infers, entirely on-device, which git
          commits a thread produced — overlapping the files a thread discussed with{" "}
          <code className="font-mono">git log</code>'s changed files inside the thread's time
          window, where the shared-file count is the confidence. Run it inside a git repo or pass a
          path; it prints the thread→commit timeline, one row per commit with its linked-thread
          count.
        </p>
        <p className="mt-4 max-w-[60ch] leading-relaxed text-muted-foreground">
          Curate as you go with <code className="font-mono">cal star</code>,{" "}
          <code className="font-mono">cal tag</code>, and{" "}
          <code className="font-mono">cal tags</code>; surface leftover work with{" "}
          <code className="font-mono">cal todos</code>. With distillation enabled (local Ollama or
          an API key), <code className="font-mono">cal distill</code> and{" "}
          <code className="font-mono">cal knowledge</code> pull a thread's summary, decisions, and
          gotchas, and <code className="font-mono">cal decisions</code> /{" "}
          <code className="font-mono">cal gotchas</code> recall them semantically across your whole
          history.
        </p>
        <p className="mt-4 max-w-[60ch] leading-relaxed text-muted-foreground">
          Each project keeps a durable memory: <code className="font-mono">cal memory</code> prints
          its aggregated decisions, gotchas, and open TODOs (defaults to the current repo). And it
          writes back too — <code className="font-mono">cal remember decision|gotcha</code> pins a
          new fact into that memory (add <code className="font-mono">--because "&lt;why&gt;"</code>{" "}
          to a decision to keep its rationale), and <code className="font-mono">cal done</code>{" "}
          closes a leftover TODO. These only ever touch Callimachus's own index and memory, never
          your files.
        </p>
        <p className="mt-4 max-w-[60ch] leading-relaxed text-muted-foreground">
          A contradiction guard keeps agents honest:{" "}
          <code className="font-mono">cal check "&lt;proposal&gt;"</code> surfaces the settled
          decisions on a topic before one gets re-litigated. And for handoff across a context-window
          compaction or between tools,{" "}
          <code className="font-mono">cal snapshot &lt;thread-id&gt;</code> (optionally{" "}
          <code className="font-mono">-l LABEL</code>) saves a durable, resumable checkpoint — a
          packed transcript plus carry-forward project memory;{" "}
          <code className="font-mono">cal snapshots</code> lists them and{" "}
          <code className="font-mono">cal resume &lt;id&gt;</code> (optionally{" "}
          <code className="font-mono">-a AGENT</code>) picks one back up.
        </p>
      </div>
    </ProductLayout>
  );
}
