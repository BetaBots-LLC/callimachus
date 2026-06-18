// Terms & Privacy copy. Plain structured content rendered as prose. These are
// honest, reasonable defaults written for a local-first OSS tool — NOT legal
// advice. Have counsel review before relying on them publicly.

export interface LegalSection {
  heading: string;
  body: string[];
}

export interface LegalDoc {
  title: string;
  updated: string;
  summary: string;
  sections: LegalSection[];
}

export const LEGAL_UPDATED = "17 June 2026";

export const PRIVACY: LegalDoc = {
  title: "Privacy",
  updated: LEGAL_UPDATED,
  summary:
    "Callimachus is local-first, and this website uses only privacy-friendly, cookieless analytics. No advertising, no tracking cookies, no cross-site profiles.",
  sections: [
    {
      heading: "The short version",
      body: [
        "The Callimachus application runs entirely on your computer. Your conversation index, your search queries, and your embeddings never leave your machine. There is no account to create and nothing is sent to us.",
        "This website (callimachus.app) uses Vercel Web Analytics for aggregate visit counts. It is cookieless and sets no tracking cookies — it does not build a profile of you or follow you across sites. We don't know who you are, and we'd like to keep it that way.",
      ],
    },
    {
      heading: "What this website collects",
      body: [
        "Beyond standard server request logs (IP address, user agent, page requested) that our host Vercel processes to serve the site, we use Vercel Web Analytics — a cookieless, privacy-friendly service that records aggregate page views and visitor counts without fingerprinting, advertising, or cross-site tracking. No personal profile is built and no tracking cookies are set.",
        "The download buttons read the latest published release from GitHub's public API at request time so we can show you the current version. That request happens on our server, not in your browser.",
      ],
    },
    {
      heading: "What the application collects",
      body: [
        "By design, nothing is sent to us. Callimachus indexes the conversation logs your AI agents already write on disk and stores the index locally. Any provider API keys you add are kept in your operating system's keychain, never transmitted to us.",
        "The only outbound network activity the app makes on your behalf is: (1) a one-time download of a small embedding model from its public host the first time you enable semantic search, and (2) requests to the AI provider you configure if you use the in-app chat — using your own key, directly to that provider.",
      ],
    },
    {
      heading: "Downloads",
      body: [
        "Application binaries are distributed through GitHub Releases. When you download a build, GitHub serves the file and may log the request under its own privacy policy.",
      ],
    },
    {
      heading: "Third-party links",
      body: [
        "This site links to GitHub, the VS Code Marketplace, and Open VSX. Those services have their own privacy policies, which govern your use of them.",
      ],
    },
    {
      heading: "Changes & contact",
      body: [
        "If this policy changes materially, the updated date above will change. Questions about privacy can go to ari@shaller.dev.",
      ],
    },
  ],
};

export const TERMS: LegalDoc = {
  title: "Terms",
  updated: LEGAL_UPDATED,
  summary:
    "Callimachus is free and open source under AGPL-3.0, offered as-is, with a commercial license available for uses AGPL doesn't allow.",
  sections: [
    {
      heading: "Acceptance",
      body: [
        "By using this website or the Callimachus software, you agree to these terms. If you don't agree, please don't use them.",
      ],
    },
    {
      heading: "License to use the software",
      body: [
        "The Callimachus software is licensed under the GNU Affero General Public License, version 3.0 or later (AGPL-3.0-or-later). You may use, study, modify, and share it under those terms, including the requirement to release source for modified versions you distribute or operate as a network service.",
        "If AGPL's obligations don't fit your situation — for example, embedding Callimachus in closed-source software or a proprietary hosted service — a commercial license is available. Contact ari@shaller.dev.",
      ],
    },
    {
      heading: "Acceptable use",
      body: [
        "Use the software and site lawfully. Don't attempt to disrupt the site, misrepresent your affiliation with the project, or use the Callimachus name or marks in a way that implies endorsement you don't have.",
      ],
    },
    {
      heading: "No warranty",
      body: [
        'The software and website are provided "as is", without warranty of any kind, express or implied, including merchantability, fitness for a particular purpose, and non-infringement. You run the software at your own risk.',
      ],
    },
    {
      heading: "Limitation of liability",
      body: [
        "To the fullest extent permitted by law, the authors and contributors are not liable for any damages arising from your use of the software or website. Where liability cannot be excluded, it is limited to the maximum extent permitted by applicable law.",
      ],
    },
    {
      heading: "Trademarks",
      body: [
        'The name "Callimachus" and the project\'s logos and brand assets are reserved and are not licensed under AGPL or any commercial license. You may make nominative reference to the project, but may not use the marks to brand your own products or imply endorsement.',
      ],
    },
    {
      heading: "Changes & contact",
      body: [
        "These terms may be updated; the date above reflects the latest version. Questions can go to ari@shaller.dev.",
      ],
    },
  ],
};
