import { createFileRoute } from "@tanstack/react-router";
import { Check } from "lucide-react";
import { seo } from "@/lib/seo";
import { COMMERCIAL_MAILTO, GITHUB_URL, SITE_URL } from "@/lib/site";
import { Container } from "@/components/site/Container";
import { buttonVariants } from "@/components/ui/button";
import { cn } from "@/lib/utils";

export const Route = createFileRoute("/pricing")({
  head: () => ({
    meta: seo({
      title: "Pricing — free and open, with a commercial option",
      description:
        "Callimachus is free and open source under AGPL-3.0. A commercial license is available for closed-source redistribution or proprietary hosted services.",
      path: "/pricing",
    }),
    links: [{ rel: "canonical", href: `${SITE_URL}/pricing` }],
  }),
  component: PricingPage,
});

const OSS = [
  "Every feature — desktop, CLI, editor extension, MCP server",
  "All 11 agent sources, hybrid search, in-app chat",
  "Local-first; your data never leaves your machine",
  "Use, modify, and share under AGPL-3.0",
];

const COMMERCIAL = [
  "Everything in the open-source edition",
  "Use without AGPL's source-disclosure obligations",
  "Ship inside closed-source software or a hosted service",
  "Terms negotiated to fit your deployment",
];

function PricingPage() {
  return (
    <Container className="py-16 sm:py-20">
      <header className="max-w-2xl">
        <span className="cat-label text-primary">№ — lending terms</span>
        <h1 className="mt-3 text-balance text-4xl text-foreground sm:text-5xl">
          Free to read. Licensed to resell.
        </h1>
        <p className="mt-4 text-lg leading-relaxed text-muted-foreground">
          Callimachus is open source under AGPL-3.0 — genuinely free for personal use, research, and
          open projects. A commercial license exists for the cases AGPL deliberately doesn't cover.
        </p>
      </header>

      <div className="mt-14 grid gap-5 lg:grid-cols-2">
        {/* Open source */}
        <div className="flex flex-col rounded-xl border border-border bg-card p-8">
          <p className="cat-label">Open source</p>
          <p className="mt-4 font-display text-4xl text-foreground">
            $0<span className="ml-2 align-middle text-base text-muted-foreground">/ forever</span>
          </p>
          <p className="mt-2 text-sm text-muted-foreground">AGPL-3.0-or-later</p>
          <ul className="mt-7 flex flex-1 flex-col gap-3">
            {OSS.map((f) => (
              <li key={f} className="flex gap-3 text-sm leading-relaxed text-foreground">
                <Check className="mt-0.5 size-4 shrink-0 text-link" />
                {f}
              </li>
            ))}
          </ul>
          <a
            href={GITHUB_URL}
            target="_blank"
            rel="noreferrer"
            className={cn(buttonVariants({ variant: "outline" }), "mt-8")}
          >
            Get the source
          </a>
        </div>

        {/* Commercial — emphasized */}
        <div className="flex flex-col rounded-xl border border-primary/40 bg-card p-8 ring-1 ring-primary/20">
          <p className="cat-label text-primary">Commercial</p>
          <p className="mt-4 font-display text-4xl text-foreground">Let's talk</p>
          <p className="mt-2 text-sm text-muted-foreground">For uses AGPL doesn't permit</p>
          <ul className="mt-7 flex flex-1 flex-col gap-3">
            {COMMERCIAL.map((f) => (
              <li key={f} className="flex gap-3 text-sm leading-relaxed text-foreground">
                <Check className="mt-0.5 size-4 shrink-0 text-primary" />
                {f}
              </li>
            ))}
          </ul>
          <a href={COMMERCIAL_MAILTO} className={cn(buttonVariants(), "mt-8")}>
            Email ari@shaller.dev
          </a>
        </div>
      </div>

      <p className="mx-auto mt-12 max-w-[60ch] text-center text-sm leading-relaxed text-muted-foreground">
        Not sure which you need? If you're using Callimachus yourself — even at work — the
        open-source edition is for you. The commercial license is only for redistributing it inside
        a closed product or service.
      </p>
    </Container>
  );
}
