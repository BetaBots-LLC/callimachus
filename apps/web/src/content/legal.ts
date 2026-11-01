// Terms & Privacy copy. Plain structured content rendered as prose. These are
// honest, reasonable defaults written for a local-first OSS tool. NOT legal
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

export const LEGAL_UPDATED = "1 November 2026";

export const PRIVACY: LegalDoc = {
  title: "Privacy",
  updated: LEGAL_UPDATED,
  summary:
    "Callimachus is local-first: your conversation history stays on your computer. Usage analytics in the desktop app are opt-in and never include your content, and this website uses cookieless analytics. No advertising, no tracking cookies, no cross-site profiles.",
  sections: [
    {
      heading: "The short version",
      body: [
        "The Callimachus application runs on your computer. Your conversation index, transcripts, search queries, embeddings and project memory stay on your machine, and there is no account to create. Conversation content leaves your machine only when you send it to an AI provider you choose, or when an agent you connect to Callimachus reads it, as described below.",
        "The desktop app can send anonymous, opt-in usage analytics to PostHog (US): event names, the app version, operating system, CPU architecture and language, and a few properties picked from fixed lists (such as which agent source, AI provider or setting an event concerns, an error category, or a count rounded into a range), tied to a random install ID. It never sends file paths, thread titles, prompts, transcripts, project names or API keys. It is off until you choose Allow, and you can turn it off any time in Settings → General or with CALLIMACHUS_TELEMETRY=0 or DO_NOT_TRACK=1. The cal CLI and the MCP server never send analytics.",
        "This website (callimachus.app) uses PostHog in cookieless mode: no cookies, no browser storage, no cross-site tracking. We don't know who you are, and we'd like to keep it that way.",
      ],
    },
    {
      heading: "What this website collects",
      body: [
        "Our host, Vercel, processes standard server request logs (IP address, user agent, page requested) to serve the site.",
        "We use PostHog analytics in cookieless mode to count page views, page leaves (how long a page stayed open and how far down it was scrolled) and which download button was clicked (the operating system it was for and where it sits on the page). PostHog stores nothing in your browser: no cookies and no local or session storage. To count unique visitors, PostHog computes a one-way hash of your IP address, user agent and a random value that changes every day and is then deleted, so a visit can't be linked to the same person on another day and no profile is built. The IP address itself is dropped once that hash is made, so it isn't stored and no location is looked up from it. We don't identify visitors, follow them across sites, or use the data for advertising.",
        "The download buttons read the latest published release from GitHub's public API at request time so we can show you the current version. That request happens on our server, not in your browser.",
      ],
    },
    {
      heading: "What the application collects",
      body: [
        "Callimachus indexes the conversation logs your AI agents already write on disk and stores the index locally. Any provider API keys you add are kept in your operating system's keychain and are never sent to us.",
        "The app makes these network requests on your behalf: (1) an update check: at launch and periodically while it runs, the desktop app downloads a small release manifest from GitHub Releases to see whether a newer version exists, and downloads an update only when you accept it; (2) a download of a small embedding model from Hugging Face the first time you build the semantic index, repeated only if that copy goes missing; (3) requests to the AI provider you configure, sent directly to that provider with your own key or account, when you use the in-app chat, Knowledge distillation (thread summaries, decisions and gotchas, including background auto-distill if you turn it on), Ask your history, a project brief or conflict review in the Projects tab, or an AI-summarized Obsidian export; and (4) opt-in usage analytics, described in the next section.",
        "These features send the relevant conversation content to the provider you picked; a project brief or conflict review sends the notes already distilled from your threads (and, for a brief, the project's folder path). Cloud providers include Anthropic, OpenAI, Google Gemini, OpenRouter and Ollama Cloud, and the keyless Claude Code and Codex CLI engines send it to Anthropic or OpenAI under your own subscription. Local Ollama keeps it on your computer, and a self-hosted Ollama server keeps it on the server you point it at. Before this content is sent, common secret formats such as API keys, access tokens and private keys are replaced with [REDACTED], in transcripts as well as in thread titles, search results and distilled notes. This is a best-effort safety net, not a guarantee: it recognizes common formats, so a key in an unusual format can slip through, and it doesn't change what you type into the chat yourself. Indexing, search and embeddings always run locally, whichever provider you choose.",
        "If you connect Callimachus to another agent (the MCP server, the session-start and optional proactive-recall hooks, or the memory block it manages in AGENTS.md or CLAUDE.md), that agent receives excerpts of your history, such as search results, threads it asks for and your project memory, redacted the same way, and sends them to its own AI provider with the rest of its context. That happens inside the agent you installed, under its provider's terms; Callimachus makes no request of its own for it.",
        "The cal command-line tool redacts what it prints the same way, because agents can run it too. If you want to see your history exactly as stored in your own terminal, add --raw to a command; the commands that feed an agent directly, such as the hooks, ignore it.",
      ],
    },
    {
      heading: "Usage analytics in the desktop app",
      body: [
        "Analytics are off by default. The app asks whether you want to share anonymous usage data, and nothing is sent unless you choose Allow. You can change your mind any time in Settings → General, and setting CALLIMACHUS_TELEMETRY=0 or DO_NOT_TRACK=1 in the app's environment turns analytics off whatever the setting says.",
        // Keep these three paragraphs in sync with Event / Event::props / super_props /
        // Telemetry::capture in apps/desktop/src-tauri/src/telemetry.rs. They name the
        // events by their exact wire names so the list can be checked against a payload.
        "What is sent, event by event. Each event is one of: app_opened (the app started, or you just chose Allow); app_active (sent automatically with the first event of each app session and of each UTC day, so an app left running in the tray still counts as active); index_completed; search_performed; thread_opened; chat_message_sent; knowledge_enabled; distill_completed; integration_installed; obsidian_exported; update_installed; setting_changed; or error_shown.",
        "Every event carries the same few details: the app version; the operating system (macOS, Windows or Linux); the CPU architecture (arm64 or x64); the language part of your locale, such as en or de (never the region, and only when the app's environment sets one); the analytics library name and version (callimachus-desktop and the app version); the time of the event; a random ID for the event itself, so a retried upload isn't counted twice; and the install ID described below. Two fixed flags tell PostHog not to build a person profile and not to look up a location.",
        "Some events add properties, each picked from a fixed list or range, and nothing else: index_completed sends how many agent sources were indexed (at most 64), the thread count and how long the run took; search_performed sends whether hybrid search and filters were used and the result count; thread_opened sends the agent source (such as claude_code or cursor); chat_message_sent sends the AI provider; knowledge_enabled sends the distillation engine; distill_completed sends the thread count; integration_installed sends the agent (claude_code, codex, cursor or gemini); obsidian_exported sends whether the note was AI-summarized; update_installed sends the previous and new app version; setting_changed sends which setting changed, never its new value; and error_shown sends an error category, such as network or rate_limited. Counts are rounded into the ranges 0, 1-9, 10-49, 50-199, 200-999, 1000-4999 and 5000+, and durations into ranges from under a second to over ten minutes. A value that isn't on its list is sent as other.",
        "What is never sent: file paths, project or repository names, thread titles, prompts, transcripts, search queries, distilled notes, API keys, or any other text from your history.",
        "Events carry a random install ID that is created when you opt in, so we can count active installs without knowing who you are. Opting out deletes it, and opting in again creates a new one. Like any internet request, analytics reach PostHog from your IP address; we don't use it to identify you, and our PostHog project is set to discard IP addresses instead of storing them.",
        "Analytics are processed by PostHog, Inc. on servers in the United States. We use them only to learn which features are used and where the app fails, so we can improve it. They are never sold or used for advertising.",
        "The analytics key is built into official release builds only: builds from source never send analytics, and neither do the cal CLI or the MCP server in any build.",
      ],
    },
    {
      heading: "Downloads",
      body: [
        "Application binaries are distributed through GitHub Releases, and the MCP server's npm launcher through the npm registry. When you download a build or check for updates, GitHub (or npm) serves the file and may log the request under its own privacy policy.",
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
        "If this policy changes materially, the updated date above will change, and changes to what the app sends are also noted in the release notes. Questions about privacy can go to ari@shaller.dev.",
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
        "If AGPL's obligations don't fit your situation, for example embedding Callimachus in closed-source software or a proprietary hosted service, a commercial license is available. Contact ari@shaller.dev.",
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
