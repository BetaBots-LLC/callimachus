// JSON-LD structured-data builders, injected via head().scripts so crawlers see
// them in the server-rendered <head>. Typed loosely (Record) since schema.org
// shapes are open-ended.

import { CONTACT_EMAIL, DESCRIPTION, GITHUB_URL, SITE_URL } from "./site";

type Ld = Record<string, unknown>;

export function organizationLd(): Ld {
  return {
    "@context": "https://schema.org",
    "@type": "Organization",
    name: "Callimachus",
    url: SITE_URL,
    logo: `${SITE_URL}/icon.svg`,
    sameAs: [GITHUB_URL],
    email: CONTACT_EMAIL,
  };
}

export function softwareApplicationLd(opts?: { version?: string }): Ld {
  return {
    "@context": "https://schema.org",
    "@type": "SoftwareApplication",
    name: "Callimachus",
    applicationCategory: "DeveloperApplication",
    operatingSystem: "macOS, Windows, Linux",
    description: DESCRIPTION,
    url: SITE_URL,
    downloadUrl: `${SITE_URL}/download`,
    ...(opts?.version ? { softwareVersion: opts.version } : {}),
    offers: {
      "@type": "Offer",
      price: "0",
      priceCurrency: "USD",
    },
  };
}

export function faqPageLd(items: { q: string; a: string }[]): Ld {
  return {
    "@context": "https://schema.org",
    "@type": "FAQPage",
    mainEntity: items.map((it) => ({
      "@type": "Question",
      name: it.q,
      acceptedAnswer: { "@type": "Answer", text: it.a },
    })),
  };
}

/** Convenience: a head().scripts entry rendering one JSON-LD block in <head>. */
export function ldScript(data: Ld): { type: "application/ld+json"; children: string } {
  return { type: "application/ld+json", children: JSON.stringify(data) };
}
