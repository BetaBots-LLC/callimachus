import { Link, createFileRoute } from "@tanstack/react-router";
import {
  ArrowRight,
  Blocks,
  GitCommitVertical,
  History,
  Lightbulb,
  MessageCircleQuestion,
  RefreshCw,
  Scale,
  Search,
  Share2,
  ShieldCheck,
  Terminal,
} from "lucide-react";
import { GithubMark } from "@/components/site/icons";
import { downloadData } from "@/server/releases";
import { seo } from "@/lib/seo";
import { faqPageLd, ldScript, organizationLd, softwareApplicationLd } from "@/lib/jsonld";
import { DESCRIPTION, GITHUB_URL, PRODUCTS, SITE_URL, TAGLINE } from "@/lib/site";
import { FAQ } from "@/content/faq";
import { Container } from "@/components/site/Container";
import { SectionHeading } from "@/components/site/Section";
import { SourceMarquee } from "@/components/site/SourceMarquee";
import { FeaturePanel } from "@/components/site/FeaturePanel";
import { FaqAccordion } from "@/components/ui/accordion";
import { DownloadButton } from "@/components/download/DownloadButton";
import { buttonVariants } from "@/components/ui/button";
import { cn } from "@/lib/utils";

export const Route = createFileRoute("/")({
  loader: () => downloadData(),
  head: ({ loaderData }) => ({
    meta: seo({ title: `Callimachus — ${TAGLINE}`, description: DESCRIPTION, path: "/" }),
    links: [{ rel: "canonical", href: `${SITE_URL}/` }],
    scripts: [
      ldScript(organizationLd()),
      ldScript(softwareApplicationLd({ version: loaderData?.release.version })),
      ldScript(faqPageLd(FAQ)),
    ],
  }),
  component: Home,
});

const PRODUCT_ICONS = { desktop: Blocks, vscode: Search, cli: Terminal, mcp: Share2 } as const;

function Home() {
  const { release, primaryOs } = Route.useLoaderData();

  return (
    <>
      {/* Hero */}
      <section className="relative overflow-hidden border-b border-border">
        {/* lamplight: a warm glow rising from behind the headline */}
        <div
          aria-hidden
          className="pointer-events-none absolute -top-40 left-1/2 h-[36rem] w-[60rem] -translate-x-1/2 rounded-full opacity-40 blur-3xl"
          style={{
            background: "radial-gradient(closest-side, oklch(0.55 0.12 50 / 0.5), transparent)",
          }}
        />
        <Container className="relative grid gap-14 py-20 lg:grid-cols-[1.05fr_0.95fr] lg:items-center lg:py-28">
          <div className="flex flex-col items-start gap-6">
            <span className="cat-label inline-flex items-center gap-2 text-primary">
              № 001 — the card catalogue for AI coding history
            </span>
            <h1 className="text-balance text-[clamp(2.6rem,7vw,4.6rem)] leading-[1.02] text-foreground">
              You already solved this. Find which thread.
            </h1>
            <p className="max-w-[54ch] text-lg leading-relaxed text-muted-foreground">
              Callimachus quietly indexes every conversation your AI agents write — across{" "}
              <strong className="font-medium text-foreground">eleven tools</strong> — into one fast,
              local, searchable catalogue. Keyword and meaning. No cloud, no account, no tracking.
            </p>
            <div className="flex flex-col gap-4 pt-2 sm:flex-row sm:items-center">
              <DownloadButton release={release} primaryOs={primaryOs} />
              <a
                href={GITHUB_URL}
                target="_blank"
                rel="noreferrer"
                className={cn(buttonVariants({ variant: "ghost", size: "lg" }), "self-start")}
              >
                <GithubMark className="size-[1.1em]" />
                Star on GitHub
              </a>
            </div>
          </div>

          {/* Hero plate — tilted like a card pulled from a drawer */}
          <div className="relative">
            <div className="rotate-[1.4deg] rounded-xl border border-border bg-card p-2 shadow-[0_24px_60px_-20px_oklch(0.1_0.02_50/0.7)] transition-transform duration-500 ease-[var(--ease-out-quint)] hover:rotate-0">
              <img
                src="/hero.png"
                alt="The Callimachus desktop app showing a searched, catalogued list of AI coding threads"
                width={1200}
                height={750}
                className="rounded-lg"
              />
            </div>
            <span className="absolute -bottom-3 left-4 rounded-sm border border-border bg-background px-2 py-1 font-mono text-[0.65rem] text-muted-foreground">
              fig. 1 — the reading room
            </span>
          </div>
        </Container>

        <Container className="relative pb-12">
          <p className="cat-label mb-3">Eleven agents, one index</p>
          <SourceMarquee />
        </Container>
      </section>

      {/* The insight */}
      <section className="border-b border-border">
        <Container className="py-20 sm:py-24">
          <p className="max-w-4xl text-balance font-display text-2xl leading-[1.35] text-foreground sm:text-[2rem]">
            Your best debugging session is three tools and two weeks ago. The fix is in there — in a
            Claude Code thread, or was it Cursor? Callimachus is the index card that finds it in a
            keystroke, so you stop re-explaining yourself to robots.
          </p>
        </Container>
      </section>

      {/* Features — deliberately uneven grid */}
      <section className="border-b border-border">
        <Container className="py-20 sm:py-28">
          <SectionHeading
            label="№ 002 — what's on the shelves"
            title="Filed, cross-referenced, instant."
            intro="One local index behind every surface. Built for the way you actually go looking for something you half-remember."
          />
          <div className="mt-12 grid gap-4 lg:grid-cols-12">
            <FeaturePanel
              icon={Search}
              label="Hybrid search"
              title="Find it by what it was about"
              className="lg:col-span-7"
            >
              Keyword search (SQLite FTS5) fused with on-device semantic similarity, so a vague
              memory — “that vector index migration” — surfaces the right thread even when you've
              forgotten the exact words. Filter by source, project, recency, starred, or tag — or
              type <code className="font-mono">file:embed/mod.rs</code> to find every thread that
              touched a path. Subagent transcripts stay out of the way until you ask for them.
            </FeaturePanel>
            <FeaturePanel
              icon={ShieldCheck}
              label="Local-first"
              title="It never leaves your machine"
              className="lg:col-span-5"
            >
              The index, the embeddings, the search — all on disk, on your computer. No account, no
              sync, no telemetry. The quietest tool in your stack.
            </FeaturePanel>
            <FeaturePanel
              icon={RefreshCw}
              label="Live indexing"
              title="Always current, never in the way"
              className="lg:col-span-5"
            >
              A background watcher catalogues new sessions as you work. Reindex any source on
              demand; forget it's running the rest of the time.
            </FeaturePanel>
            <FeaturePanel
              icon={Share2}
              label="Reusable context"
              title="Hand the past to the present"
              className="lg:col-span-7"
            >
              Copy a thread's packed context, insert it into your editor, export it to Obsidian, or
              let the MCP server feed it to an agent on demand. Your history becomes raw material
              again, not just a read-only archive.
            </FeaturePanel>
            <FeaturePanel
              icon={Lightbulb}
              label="Knowledge layer"
              title="Distill the lessons, not just the logs"
              className="lg:col-span-7"
            >
              Free heuristic TODO extraction, plus opt-in LLM distillation of decisions, gotchas,
              and summaries — with cross-thread semantic recall of past decisions and gotchas. Needs
              local Ollama (keyless) or a cloud API key.
            </FeaturePanel>
            <FeaturePanel
              icon={MessageCircleQuestion}
              label="Ask your history"
              title="A cited answer over your own threads"
              className="lg:col-span-5"
            >
              Ask a question and get a synthesized, cited answer drawn from your own sessions, with
              [thread N] citations back to the sources it used. Retrieval-augmented over the local
              index; needs an LLM engine enabled.
            </FeaturePanel>
            <FeaturePanel
              icon={GitCommitVertical}
              label="Git linkage"
              title="See which commits a conversation produced"
              className="lg:col-span-7"
            >
              Callimachus infers — entirely on-device — which commits a thread shipped, by
              overlapping the files it discussed with <code className="font-mono">git log</code>{" "}
              inside the thread's time window. Walk the thread-to-commit timeline from the app or{" "}
              <code className="font-mono">cal commits</code>, and ask “which commit came out of this
              conversation?” straight from an agent.
            </FeaturePanel>
            <FeaturePanel
              icon={History}
              label="Session snapshots"
              title="Resumable cross-tool handoff"
              className="lg:col-span-5"
            >
              Durable checkpoints of a thread — packed transcript plus carry-forward project memory
              — so work survives a context-window compaction or a jump to another tool.
              Auto-captured via Claude Code hooks, or snapshot and resume by hand.
            </FeaturePanel>
            <FeaturePanel
              icon={Scale}
              label="Decision guard"
              title="Catch contradictions before they ship"
              className="lg:col-span-12"
            >
              Record a decision with the rationale behind it, then let the guard surface settled
              calls on a topic before an agent quietly re-litigates one. Check a proposal against
              your own history — from <code className="font-mono">cal check</code> or the MCP — so
              the past gets a vote before the code does.
            </FeaturePanel>
          </div>
        </Container>
      </section>

      {/* Four ways in */}
      <section className="border-b border-border">
        <Container className="py-20 sm:py-28">
          <SectionHeading
            label="№ 003 — reading desks"
            title="Four ways into the same catalogue."
            intro="One index, read from wherever you already are."
          />
          <ul className="mt-12 border-t border-border">
            {PRODUCTS.map((p) => {
              const Icon = PRODUCT_ICONS[p.slug];
              return (
                <li key={p.slug} className="border-b border-border">
                  <Link
                    to={`/${p.slug}`}
                    className="group flex flex-col gap-3 py-7 transition-colors hover:bg-card/60 sm:flex-row sm:items-baseline sm:gap-8"
                  >
                    <span className="font-mono text-xs text-muted-foreground sm:w-10 sm:pt-1">
                      № {p.no}
                    </span>
                    <div className="flex items-center gap-3 sm:w-56 sm:shrink-0">
                      <Icon className="size-5 text-primary" />
                      <span className="font-display text-xl text-foreground">{p.name}</span>
                    </div>
                    <p className="max-w-[46ch] flex-1 text-muted-foreground">{p.blurb}</p>
                    <ArrowRight className="hidden size-5 shrink-0 self-center text-muted-foreground transition-transform duration-200 ease-[var(--ease-out-quint)] group-hover:translate-x-1 group-hover:text-link sm:block" />
                  </Link>
                </li>
              );
            })}
          </ul>
        </Container>
      </section>

      {/* Privacy band */}
      <section className="border-b border-border bg-card">
        <Container className="flex flex-col gap-6 py-20 sm:py-24">
          <span className="cat-label text-primary">№ 004 — house rules</span>
          <h2 className="max-w-3xl text-balance text-3xl text-foreground sm:text-4xl">
            A library that doesn't read over your shoulder.
          </h2>
          <p className="max-w-[60ch] text-lg leading-relaxed text-muted-foreground">
            Cookieless, aggregate analytics on this site — no tracking cookies, no cross-site
            profiles. No telemetry in the app. Your conversations are indexed where they already
            live — on your machine — and stay there. Open source under AGPL-3.0, so you never have
            to take our word for it.
          </p>
          <div className="flex flex-wrap gap-3">
            <a
              href={GITHUB_URL}
              target="_blank"
              rel="noreferrer"
              className={cn(buttonVariants({ variant: "outline" }))}
            >
              <GithubMark className="size-[1.1em]" />
              Read the source
            </a>
            <Link to="/privacy" className={cn(buttonVariants({ variant: "ghost" }))}>
              Our privacy stance
            </Link>
          </div>
        </Container>
      </section>

      {/* FAQ */}
      <section className="border-b border-border">
        <Container className="grid gap-12 py-20 sm:py-28 lg:grid-cols-[0.8fr_1.2fr]">
          <SectionHeading label="№ 005 — at the desk" title="Questions, briefly answered." />
          <div>
            <FaqAccordion items={FAQ} />
          </div>
        </Container>
      </section>

      {/* Closing CTA */}
      <section>
        <Container className="flex flex-col items-start gap-6 py-24 sm:items-center sm:py-32 sm:text-center">
          <h2 className="max-w-3xl text-balance text-4xl text-foreground sm:text-5xl">
            Open the catalogue.
          </h2>
          <p className="max-w-[48ch] text-lg text-muted-foreground">
            Free, local, and quietly yours. Run the app once and every thread you've ever written is
            a keystroke away.
          </p>
          <DownloadButton release={release} primaryOs={primaryOs} className="sm:items-center" />
        </Container>
      </section>
    </>
  );
}
