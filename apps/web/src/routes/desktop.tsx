import { createFileRoute } from "@tanstack/react-router";
import {
  BookOpen,
  FolderGit2,
  Lightbulb,
  ListChecks,
  MessagesSquare,
  PieChart,
  Search,
  Sunrise,
  Trash2,
} from "lucide-react";
import { downloadData } from "@/server/releases";
import { seo } from "@/lib/seo";
import { ldScript, softwareApplicationLd } from "@/lib/jsonld";
import { SITE_URL } from "@/lib/site";
import { ProductLayout } from "@/components/site/ProductLayout";
import { FeaturePanel } from "@/components/site/FeaturePanel";
import { ScreenshotGallery } from "@/components/site/ScreenshotGallery";
import { DownloadButton } from "@/components/download/DownloadButton";

export const Route = createFileRoute("/desktop")({
  loader: () => downloadData(),
  head: ({ loaderData }) => ({
    meta: seo({
      title: "Callimachus Desktop: read and search your AI history",
      description:
        "The Callimachus desktop app for macOS, Windows, and Linux: see what your agents did today, search every AI coding thread, give each repo one page with the memory your agents start from, ask your own history a cited question, and export to Obsidian. Local and private.",
      path: "/desktop",
    }),
    links: [{ rel: "canonical", href: `${SITE_URL}/desktop` }],
    scripts: [ldScript(softwareApplicationLd({ version: loaderData?.release.version }))],
  }),
  component: DesktopPage,
});

function DesktopPage() {
  const { release, primaryOs } = Route.useLoaderData();
  return (
    <ProductLayout
      no="01"
      kicker="Desktop app"
      title="The reading room."
      description="A native app for macOS, Windows, and Linux. Today shows what your agents did since you last looked, Search finds any thread they ever wrote, Chat answers from your own history, and Projects gives each repo one page with the memory your agents start from."
      cta={<DownloadButton release={release} primaryOs={primaryOs} placement="desktop_page" />}
    >
      <div className="grid gap-4 lg:grid-cols-12">
        <FeaturePanel
          icon={Sunrise}
          label="Today"
          title="What happened since you last looked"
          className="lg:col-span-7"
        >
          The app opens on your sessions since the last time you had it open, grouped by repo, each
          with its agent, its length and the commits it produced. New TODOs, errors you hit again
          and fresh snapshots sit above them, and Copy as standup turns the lot into plain markdown.
          No LLM involved.
        </FeaturePanel>
        <FeaturePanel
          icon={Search}
          label="Search"
          title="Scoped to the repo you mean"
          className="lg:col-span-5"
        >
          Keyword and on-device meaning, scoped to one repo and the last 24 hours, 7 days or 30
          days, and filtered by source, starred or tag. Decisions and gotchas that match show above
          the threads, and an empty scope offers to search every repo in one click.
        </FeaturePanel>
        <FeaturePanel
          icon={FolderGit2}
          label="Projects"
          title="One page per repo"
          className="lg:col-span-7"
        >
          Recent sessions to resume or open in a CLI, the repo's decisions, gotchas and TODOs, its
          snapshots, the commits its sessions produced, the errors that keep coming back, and what
          it cost. What agents see shows the exact text the session hook injects and the managed
          AGENTS.md block, with their sizes, so the memory is never a black box.
        </FeaturePanel>
        <FeaturePanel
          icon={MessagesSquare}
          label="Chat"
          title="Talk to your archive"
          className="lg:col-span-5"
        >
          A provider-agnostic chat that can search your own history and run shell commands with your
          approval. Switch it to Ask for one synthesized answer with [thread N] citations. Runs on
          your logged-in Claude Code or Codex CLI with no key, or on Anthropic, OpenAI, Gemini,
          OpenRouter or Ollama, set up once in Settings &gt; AI engine.
        </FeaturePanel>
        <FeaturePanel
          icon={Lightbulb}
          label="Memory"
          title="Useful on day one, better every session"
          className="lg:col-span-7"
        >
          Each repo starts with a digest of its recent sessions, its open TODOs and anything you
          Remember, with no LLM and no key. Turn on memory and it fills with decisions, gotchas and
          summaries distilled by the engine you pick: your logged-in Claude Code or Codex CLI, local
          Ollama, or a cloud API key. Claude Code's own auto memory is imported too, and the hooks
          hand it all to Claude Code, Codex, Gemini CLI, GitHub Copilot and Cursor as each session
          starts.
        </FeaturePanel>
        <FeaturePanel
          icon={BookOpen}
          label="Read & export"
          title="Every transcript, legible"
          className="lg:col-span-5"
        >
          Open any session as a clean, rendered transcript, with tool calls folded away, and jump to
          its project in one click. Send it to your Obsidian vault as a note, optionally with an
          AI-written summary of the decisions, gotchas and TODOs buried in it.
        </FeaturePanel>
        <FeaturePanel
          icon={PieChart}
          label="Stats"
          title="Know your corpus"
          className="lg:col-span-4"
        >
          Threads and messages per source, a year of activity where a click on a day searches it,
          semantic coverage, a spend estimate by model, and a weekly count of what Callimachus did
          for your agents.
        </FeaturePanel>
        <FeaturePanel
          icon={Trash2}
          label="Cleanup"
          title="Reclaim the shelves"
          className="lg:col-span-4"
        >
          A paginated, size-aware table to prune old threads when the archive gets heavy, plus
          Shrink index, which trims the tool output older versions stored and returns the space to
          disk.
        </FeaturePanel>
        <FeaturePanel
          icon={ListChecks}
          label="Setup"
          title="Four steps, one index"
          className="lg:col-span-4"
        >
          A first-run checklist indexes your history, connects your agents, turns on memory and
          builds semantic search; only the first step is required. In Claude Code, the plugin (
          <code className="font-mono">/plugin install callimachus@callimachus</code>) brings the
          skill, the MCP server and the memory hooks in one step. The CLI, the editor extension and
          the MCP server all read the same index.
        </FeaturePanel>
      </div>

      <ScreenshotGallery className="mt-12" />
    </ProductLayout>
  );
}
