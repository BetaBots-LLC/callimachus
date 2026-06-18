import { createFileRoute } from "@tanstack/react-router";
import { seo } from "@/lib/seo";
import { SITE_URL } from "@/lib/site";
import { PRIVACY } from "@/content/legal";
import { LegalPage } from "@/components/site/LegalPage";

export const Route = createFileRoute("/privacy")({
  head: () => ({
    meta: seo({
      title: "Privacy — Callimachus",
      description:
        "Callimachus is local-first — the app never tracks you. This website uses privacy-friendly, cookieless analytics only: no tracking cookies, no advertising, no cross-site profiles.",
      path: "/privacy",
    }),
    links: [{ rel: "canonical", href: `${SITE_URL}/privacy` }],
  }),
  component: () => <LegalPage doc={PRIVACY} />,
});
