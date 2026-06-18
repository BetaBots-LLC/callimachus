import { createFileRoute } from "@tanstack/react-router";
import { ClipboardCopy, PanelLeft, Paintbrush, ScrollText } from "lucide-react";
import { seo } from "@/lib/seo";
import { ldScript, softwareApplicationLd } from "@/lib/jsonld";
import { MARKETPLACE_URL, OPENVSX_URL, SITE_URL } from "@/lib/site";
import { ProductLayout } from "@/components/site/ProductLayout";
import { FeaturePanel } from "@/components/site/FeaturePanel";
import { buttonVariants } from "@/components/ui/button";
import { cn } from "@/lib/utils";

export const Route = createFileRoute("/vscode")({
  head: () => ({
    meta: seo({
      title: "Callimachus for VS Code & Cursor",
      description:
        "Search your AI coding history from inside VS Code, Cursor, or VSCodium. A sidebar and transcript tabs over the same local index — no context-switch.",
      path: "/vscode",
    }),
    links: [{ rel: "canonical", href: `${SITE_URL}/vscode` }],
    scripts: [ldScript(softwareApplicationLd())],
  }),
  component: VsCodePage,
});

function VsCodePage() {
  return (
    <ProductLayout
      no="02"
      kicker="VS Code & Cursor"
      title="Search without leaving the editor."
      description="A Callimachus sidebar and rich transcript tabs, right where you write code. Same local index as the desktop app — so the thread you need is one shortcut away, not one context-switch."
      cta={
        <>
          <a
            href={MARKETPLACE_URL}
            target="_blank"
            rel="noreferrer"
            className={cn(buttonVariants())}
          >
            VS Code Marketplace
          </a>
          <a
            href={OPENVSX_URL}
            target="_blank"
            rel="noreferrer"
            className={cn(buttonVariants({ variant: "outline" }))}
          >
            Open VSX (Cursor)
          </a>
        </>
      }
    >
      <div className="grid gap-4 sm:grid-cols-2">
        <FeaturePanel icon={PanelLeft} label="Sidebar" title="Live search & recents">
          A dedicated panel: search the whole index, scope to the open project, browse recent
          threads, and see corpus stats — themed to your editor's light or dark theme.
        </FeaturePanel>
        <FeaturePanel icon={ScrollText} label="Transcripts" title="Threads as tabs">
          Open any thread as a rendered transcript tab — user bubbles, full-markdown assistant
          turns, collapsible tool calls — styled to match the desktop app.
        </FeaturePanel>
        <FeaturePanel icon={ClipboardCopy} label="Reuse" title="Insert & copy in a click">
          Drop a thread's context into the active editor or copy it to the clipboard to seed your
          next prompt, without leaving the file you're in.
        </FeaturePanel>
        <FeaturePanel icon={Paintbrush} label="Native feel" title="Works in your fork">
          VS Code, Cursor, and VSCodium all supported. Requires the {""}
          <code className="rounded bg-background px-1 py-0.5 font-mono text-[0.85em]">cal</code>{" "}
          CLI, which ships with the desktop app.
        </FeaturePanel>
      </div>
    </ProductLayout>
  );
}
