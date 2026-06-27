// Product screenshots ("plates") for the marketing site, shown by ScreenshotGallery.
//
// To add one: capture the app (dark UI — it matches the site), trim the window shadow and
// convert to webp, drop it in /public/screenshots/<file>, then add an entry below.
// `fig` + `title` are rendered as a catalogue figure label; `blurb` is the one-line description.

export interface Shot {
  /** Path under /public, e.g. "/screenshots/search.webp". */
  file: string;
  /** Alt text — describe the screen (SEO + accessibility). */
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
    alt: "Callimachus desktop search: a filtered, ranked list of AI coding threads across tools, with a thread open beside it",
    fig: "fig. 1",
    title: "the reading room",
    blurb:
      "Every thread your agents have written, filed by source and project. Keyword fused with on-device meaning, so the half-remembered one is a keystroke away.",
    width: 1194,
    height: 822,
  },
  {
    file: "/screenshots/projects.webp",
    alt: "The Projects tab aggregating a repo's distilled decisions, gotchas, and open TODOs with an LLM brief",
    fig: "fig. 2",
    title: "per-project memory",
    blurb:
      "Each repo's decisions, gotchas, and open TODOs, distilled across every session into a durable memory your agents can actually read.",
    width: 1194,
    height: 822,
  },
  {
    file: "/screenshots/ask.webp",
    alt: "Ask your history: a synthesized, cited answer drawn from the user's own past sessions",
    fig: "fig. 3",
    title: "ask your history",
    blurb:
      "A synthesized, cited answer drawn from your own past sessions, with [thread N] references back to the source it used.",
    width: 1194,
    height: 822,
  },
  {
    file: "/screenshots/coach.webp",
    alt: "The Coach view: a 52-week activity heatmap and the recurring errors you keep hitting across tools",
    fig: "fig. 4",
    title: "recurring errors",
    blurb:
      "A year of work at a glance, and the mistakes you keep re-running across tools, so you fix the pattern instead of the instance.",
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
    alt: "The in-app chat answering over the user's own indexed history, running on a keyless CLI engine",
    fig: "fig. 6",
    title: "talk to your archive",
    blurb:
      "A provider-agnostic chat that searches your own history before it answers. Bring a key, or run it keyless on your logged-in Claude Code or Codex CLI.",
    width: 1194,
    height: 822,
  },
];
