import { createFileRoute } from "@tanstack/react-router";
import { seo } from "@/lib/seo";
import { SITE_URL } from "@/lib/site";
import { TERMS } from "@/content/legal";
import { LegalPage } from "@/components/site/LegalPage";

export const Route = createFileRoute("/terms")({
  head: () => ({
    meta: seo({
      title: "Terms — Callimachus",
      description:
        "Terms for using the Callimachus website and software. Free and open under AGPL-3.0, provided as-is, with a commercial license available.",
      path: "/terms",
    }),
    links: [{ rel: "canonical", href: `${SITE_URL}/terms` }],
  }),
  component: () => <LegalPage doc={TERMS} />,
});
