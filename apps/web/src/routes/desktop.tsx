import { createFileRoute } from "@tanstack/react-router";
import { BookOpen, Boxes, FileDown, MessagesSquare, PieChart, Trash2 } from "lucide-react";
import { downloadData } from "@/server/releases";
import { seo } from "@/lib/seo";
import { ldScript, softwareApplicationLd } from "@/lib/jsonld";
import { SITE_URL } from "@/lib/site";
import { ProductLayout } from "@/components/site/ProductLayout";
import { FeaturePanel } from "@/components/site/FeaturePanel";
import { DownloadButton } from "@/components/download/DownloadButton";

export const Route = createFileRoute("/desktop")({
  loader: () => downloadData(),
  head: ({ loaderData }) => ({
    meta: seo({
      title: "Callimachus Desktop — read & search your AI history",
      description:
        "The Callimachus desktop app for macOS, Windows, and Linux: browse and search every AI coding thread, chat over your own history, and export to Obsidian. Local and private.",
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
      description="A native app for macOS, Windows, and Linux. Every thread your agents have written, browsable and searchable in one place — with a chat that can read your own history back to you."
      cta={<DownloadButton release={release} primaryOs={primaryOs} />}
    >
      <div className="grid gap-4 lg:grid-cols-12">
        <FeaturePanel
          icon={BookOpen}
          label="Browse & read"
          title="Every transcript, legible"
          className="lg:col-span-7"
        >
          Open any session as a clean, rendered transcript — user turns, assistant turns, tool calls
          folded away. Jump between threads without leaving the app.
        </FeaturePanel>
        <FeaturePanel
          icon={MessagesSquare}
          label="Chat"
          title="Talk to your archive"
          className="lg:col-span-5"
        >
          A built-in, provider-agnostic chat that can search your own history and run shell commands
          with your approval. Bring your own key — Anthropic, OpenAI, Gemini, OpenRouter, or Ollama.
        </FeaturePanel>
        <FeaturePanel
          icon={FileDown}
          label="Export"
          title="Out to Obsidian"
          className="lg:col-span-5"
        >
          Send any thread to your vault as a clean note — optionally with an AI-written summary of
          the decisions, gotchas, and TODOs buried in it.
        </FeaturePanel>
        <FeaturePanel
          icon={PieChart}
          label="Stats"
          title="Know your corpus"
          className="lg:col-span-4"
        >
          A dashboard of threads and messages per source and role, your busiest projects, and how
          much of the archive is embedded for semantic search.
        </FeaturePanel>
        <FeaturePanel
          icon={Trash2}
          label="Cleanup"
          title="Reclaim the shelves"
          className="lg:col-span-4"
        >
          A paginated, size-aware table to prune old threads and reclaim disk space when the archive
          gets heavy.
        </FeaturePanel>
        <FeaturePanel
          icon={Boxes}
          label="One index"
          title="Shared everywhere"
          className="lg:col-span-4"
        >
          The same local index powers the CLI, the editor extension, and the MCP server. Index once;
          reach it from anywhere.
        </FeaturePanel>
      </div>

      <figure className="mt-12 rounded-xl border border-border bg-card p-2">
        <img
          src="/hero.png"
          alt="The Callimachus desktop app: a searchable, catalogued list of AI coding threads"
          width={1200}
          height={750}
          className="rounded-lg"
          loading="lazy"
        />
      </figure>
    </ProductLayout>
  );
}
