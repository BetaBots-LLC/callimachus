// Product screenshots ("plates") for the marketing site, shown by ScreenshotGallery.
//
// To add one: capture the app in the dark UI (it matches the site), trim the window shadow and
// convert to webp, drop it in /public/screenshots/<file>, then add an entry below.
// `fig` + `title` are rendered as a catalogue figure label; `blurb` is the one-line description.
// Captions describe what each image shows, so they stay true until the image changes.
//
// TODO(screenshots): every plate below predates the Today / Search / Chat / Projects tab bar
// (each shows the retired Coach, Knowledge and Ask tabs), so all six need re-shooting:
// - search.webp: add the project chip and the date presets, drop the toolbar Reindex button,
//   show a "Decisions and gotchas" group above the threads.
// - projects.webp: the Project page (picker, Recent sessions, Memory, Snapshots, What agents see).
// - ask.webp: Chat in Ask mode, with [thread N] citation chips.
// - coach.webp: retire it; replace with agents.webp (the Project page's What agents see: the
//   hook text and the AGENTS.md block with their char counts). The heatmap now lives on
//   Stats and recurring errors on the Project page; the Today view is the new home hero.
// - stats.webp: include the clickable activity heatmap and "What Callimachus did for your
//   agents this week".
// - chat.webp: the new header (provider and model only, the Chat | Ask control).
// The home hero (/public/hero.png) is the brand banner, not an app capture; replace it with
// the Today view (CAPTURE.md has the shot list).

export interface Shot {
  /** Path under /public, e.g. "/screenshots/search.webp". */
  file: string;
  /** Alt text: describe the screen (SEO + accessibility). */
  alt: string;
  /** Catalogue figure number, e.g. "fig. 1". */
  fig: string;
  /** Short figure title, lowercase, catalogue voice. */
  title: string;
  /** One-line description of what the figure shows. */
  blurb: string;
  width?: number;
  height?: number;
}

export const SCREENSHOTS: Shot[] = [
  {
    file: "/screenshots/search.webp",
    alt: "Callimachus desktop search: a ranked list of AI coding threads with source, subagent, semantic and starred filters, and a thread open beside it",
    fig: "fig. 1",
    title: "the reading room",
    blurb:
      "Every thread your agents have written in one ranked list, with source, subagent, semantic and starred filters and the thread open beside it. Keyword fused with on-device meaning, so the half-remembered one is a keystroke away.",
    width: 1194,
    height: 822,
  },
  {
    file: "/screenshots/projects.webp",
    alt: "A repo's project memory: pinned and distilled decisions with the session each came from, and the actions to distill, write the memory file and update AGENTS.md",
    fig: "fig. 2",
    title: "per-project memory",
    blurb:
      "Each repo's decisions, gotchas, and open TODOs, gathered across every session into a durable memory your agents start from.",
    width: 1194,
    height: 822,
  },
  {
    file: "/screenshots/ask.webp",
    alt: "Ask your history: a synthesized answer with [thread N] citations, above the list of past sessions it drew on",
    fig: "fig. 3",
    title: "ask your history",
    blurb:
      "A synthesized, cited answer drawn from your own past sessions, with [thread N] references back to the sources it used.",
    width: 1194,
    height: 822,
  },
  {
    file: "/screenshots/coach.webp",
    alt: "A 52-week activity heatmap above the recurring errors you keep hitting across tools, each with its count and the threads it appeared in",
    fig: "fig. 4",
    title: "recurring errors",
    blurb:
      "A year of work at a glance (on the Stats page), and the errors you keep re-running across tools (on each project's page), so you fix the pattern instead of the instance.",
    width: 1194,
    height: 822,
  },
  {
    file: "/screenshots/stats.webp",
    alt: "The Stats dashboard: threads and messages per source, semantic coverage, and estimated spend by model",
    fig: "fig. 5",
    title: "know your corpus",
    blurb:
      "Threads, messages, and semantic coverage at a glance, plus an estimate of what your AI coding actually cost, broken down by model and by your priciest threads.",
    width: 1194,
    height: 822,
  },
  {
    file: "/screenshots/chat.webp",
    alt: "The in-app chat answering over the user's own indexed history, running on the logged-in Claude Code CLI with no API key",
    fig: "fig. 6",
    title: "talk to your archive",
    blurb:
      "A provider-agnostic chat that searches your own history before it answers. Bring a key, or run it keyless on your logged-in Claude Code or Codex CLI.",
    width: 1194,
    height: 822,
  },
];
