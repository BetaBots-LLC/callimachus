// The report's share card: a 1200 x 630 PNG (the Open Graph size most sites crop to) drawn
// on a canvas in the webview, in the app's font and the theme on screen. It shows only
// aggregate numbers, agent names and one error line, which `report.rs` takes from tool output
// (never a user turn) with absolute paths cut to file names: nothing from a prompt body, no
// project path. No external requests: the font is the bundled Geist, the monospace one the
// system's.
import type { Report } from "../../lib/api";
import { formatUsd } from "../../lib/format";

export const CARD_WIDTH = 1200;
export const CARD_HEIGHT = 630;

const SANS = '"Geist Variable", system-ui, sans-serif';
const MONO = 'ui-monospace, "SF Mono", Menlo, Consolas, monospace';
/** The brand sunset the Stats heatmap uses, at falling strengths for the agent bar. */
const ACCENT = "#C16E2C";
const RAMP = [1, 0.72, 0.5, 0.32, 0.18];

const PALETTE = {
  light: { bg: "#ffffff", surface: "#f5f5f5", fg: "#0a0a0a", muted: "#737373", line: "#e5e5e5" },
  dark: { bg: "#0a0a0a", surface: "#171717", fg: "#fafafa", muted: "#a1a1a1", line: "#262626" },
} as const;

export type CardTheme = keyof typeof PALETTE;

const dateFmt: Intl.DateTimeFormatOptions = { month: "short", year: "numeric" };

/** The view's rule: a sliver of a share reads "<1%", not a rounded-up "1%". */
function pct(share: number): string {
  return share > 0 && share < 0.01 ? "<1%" : `${Math.round(share * 100)}%`;
}

/** What the hours figure covers: every agent, one, or only the sessions with usable times
 *  (Cursor stores most sessions without them). */
function hoursLabel(t: NonNullable<Report["totals"]>): string {
  if (t.hoursSessions >= t.sessions) return "hours with agents";
  if (t.hoursSources.length === 1) {
    const one = t.byTool.find((x) => x.source === t.hoursSources[0]);
    if (one) return `hours in ${one.label}`;
  }
  return "hours in timed sessions";
}

function compact(n: number): string {
  return n.toLocaleString("en-US", { notation: n >= 100_000 ? "compact" : "standard" });
}

/** Whole dollars from $1,000 up: cents on a headline number are noise. */
function money(n: number): string {
  if (n >= 1000) return `$${Math.round(n).toLocaleString("en-US")}`;
  return formatUsd(n);
}

/** `text` cut with an ellipsis to fit `max` px in the context's current font. */
function fit(ctx: CanvasRenderingContext2D, text: string, max: number): string {
  if (ctx.measureText(text).width <= max) return text;
  let lo = 0;
  let hi = text.length;
  while (lo < hi) {
    const mid = Math.ceil((lo + hi) / 2);
    if (ctx.measureText(`${text.slice(0, mid)}…`).width <= max) lo = mid;
    else hi = mid - 1;
  }
  return `${text.slice(0, lo).trimEnd()}…`;
}

/** Up to `lines` lines of `text` wrapped at spaces (a long token breaks anywhere). */
function wrap(ctx: CanvasRenderingContext2D, text: string, max: number, lines: number): string[] {
  const out: string[] = [];
  let rest = text.trim();
  while (rest && out.length < lines) {
    if (out.length === lines - 1) {
      out.push(fit(ctx, rest, max));
      break;
    }
    let cut = rest.length;
    while (cut > 0 && ctx.measureText(rest.slice(0, cut)).width > max) {
      const space = rest.lastIndexOf(" ", cut - 1);
      cut = space > 0 ? space : cut - 1;
    }
    out.push(rest.slice(0, Math.max(cut, 1)).trimEnd());
    rest = rest.slice(Math.max(cut, 1)).trimStart();
  }
  return out;
}

function roundRect(
  ctx: CanvasRenderingContext2D,
  x: number,
  y: number,
  w: number,
  h: number,
  r: number,
) {
  ctx.beginPath();
  // roundRect arrived in Safari 16; an older WebKit gets square corners.
  if (typeof ctx.roundRect === "function") ctx.roundRect(x, y, w, h, r);
  else ctx.rect(x, y, w, h);
}

/** The card's headline figures, the ones the view shows above it too. */
export function headline(report: Report): { label: string; value: string }[] {
  const t = report.totals;
  if (!t) return [];
  const out = [{ label: "sessions", value: compact(t.sessions) }];
  if (t.hoursSessions > 0)
    out.push({ label: hoursLabel(t), value: compact(Math.round(t.estimatedHours)) });
  out.push({ label: "active days", value: compact(t.activeDays) });
  if (report.spend && report.spend.total.totalCost > 0)
    out.push({ label: "est. spend (list price)", value: money(report.spend.total.totalCost) });
  return out;
}

/** Draw the card. Waits for the bundled font, so the first save never falls back to the
 *  system face. */
export async function drawReportCard(
  report: Report,
  theme: CardTheme,
  scale = 1,
): Promise<HTMLCanvasElement> {
  await Promise.all([
    document.fonts.load(`600 64px ${SANS}`),
    document.fonts.load(`400 20px ${SANS}`),
  ]).catch(() => {});
  const c = PALETTE[theme];
  const canvas = document.createElement("canvas");
  canvas.width = CARD_WIDTH * scale;
  canvas.height = CARD_HEIGHT * scale;
  const ctx = canvas.getContext("2d");
  if (!ctx) throw new Error("this webview cannot draw on a canvas");
  ctx.scale(scale, scale);
  ctx.textBaseline = "alphabetic";

  ctx.fillStyle = c.bg;
  ctx.fillRect(0, 0, CARD_WIDTH, CARD_HEIGHT);
  // A thin brand rule along the top edge.
  ctx.fillStyle = ACCENT;
  ctx.fillRect(0, 0, CARD_WIDTH, 6);

  const pad = 64;
  const inner = CARD_WIDTH - pad * 2;
  const t = report.totals;

  ctx.fillStyle = c.muted;
  ctx.font = `500 20px ${SANS}`;
  ctx.fillText("Callimachus", pad, pad + 14);
  ctx.fillStyle = c.fg;
  ctx.font = `600 52px ${SANS}`;
  ctx.fillText("Your agents, so far", pad, pad + 76);
  if (t) {
    const from = new Date(t.firstSession * 1000).toLocaleDateString("en-US", dateFmt);
    const to = new Date(t.lastSession * 1000).toLocaleDateString("en-US", dateFmt);
    ctx.fillStyle = c.muted;
    ctx.font = `400 22px ${SANS}`;
    const tools = t.byTool.length;
    ctx.fillText(
      `${from === to ? from : `${from} to ${to}`} · ${tools} ${tools === 1 ? "agent" : "agents"} · ${compact(t.messages)} messages`,
      pad,
      pad + 114,
    );
  }

  // Headline numbers.
  const stats = headline(report);
  const statsTop = 236;
  if (stats.length > 0) {
    const w = inner / stats.length;
    stats.forEach((s, i) => {
      const x = pad + i * w;
      ctx.fillStyle = c.fg;
      ctx.font = `600 60px ${SANS}`;
      ctx.fillText(fit(ctx, s.value, w - 24), x, statsTop + 56);
      ctx.fillStyle = c.muted;
      ctx.font = `400 20px ${SANS}`;
      ctx.fillText(fit(ctx, s.label, w - 24), x, statsTop + 90);
    });
  }

  ctx.fillStyle = c.line;
  ctx.fillRect(pad, 362, inner, 1);

  // Lower half: the agent mix on the left, the biggest recurring error on the right.
  const colTop = 400;
  const gap = 48;
  const leftW = t ? 470 : 0;
  if (t && t.byTool.length > 0) {
    ctx.fillStyle = c.muted;
    ctx.font = `500 18px ${SANS}`;
    ctx.fillText("Sessions by agent", pad, colTop);
    // Three agents and "Other": four legend rows fit above the footnote, and every
    // segment of the bar gets one.
    const top = t.byTool.slice(0, 3);
    const rest = t.byTool.slice(3).reduce((n, x) => n + x.share, 0);
    const parts = rest > 0 ? [...top, { label: "Other", share: rest }] : top;
    let x = pad;
    const barY = colTop + 20;
    roundRect(ctx, pad, barY, leftW, 14, 7);
    ctx.save();
    ctx.clip();
    ctx.fillStyle = c.surface;
    ctx.fillRect(pad, barY, leftW, 14);
    parts.forEach((p, i) => {
      const w = Math.max(2, p.share * leftW);
      ctx.globalAlpha = RAMP[i] ?? 0.15;
      ctx.fillStyle = ACCENT;
      ctx.fillRect(x, barY, w, 14);
      x += w;
    });
    ctx.restore();
    ctx.globalAlpha = 1;
    parts.forEach((p, i) => {
      const y = barY + 46 + i * 30;
      ctx.globalAlpha = RAMP[i] ?? 0.15;
      ctx.fillStyle = ACCENT;
      roundRect(ctx, pad, y - 13, 14, 14, 3);
      ctx.fill();
      ctx.globalAlpha = 1;
      ctx.fillStyle = c.fg;
      ctx.font = `400 20px ${SANS}`;
      ctx.fillText(fit(ctx, p.label, leftW - 110), pad + 26, y);
      ctx.fillStyle = c.muted;
      ctx.textAlign = "right";
      ctx.fillText(pct(p.share), pad + leftW, y);
      ctx.textAlign = "left";
    });
  }

  const issue = report.recurringErrors?.[0];
  const rx = pad + (t ? leftW + gap : 0);
  const rw = CARD_WIDTH - pad - rx;
  if (issue) {
    ctx.fillStyle = c.muted;
    ctx.font = `500 18px ${SANS}`;
    ctx.fillText("Most frequent recurring error", rx, colTop);
    roundRect(ctx, rx, colTop + 20, rw, 112, 12);
    ctx.fillStyle = c.surface;
    ctx.fill();
    ctx.fillStyle = c.fg;
    ctx.font = `400 19px ${MONO}`;
    wrap(ctx, issue.example, rw - 40, 3).forEach((line, i) => {
      ctx.fillText(line, rx + 20, colTop + 54 + i * 28);
    });
    ctx.font = `400 20px ${SANS}`;
    const seen = `Hit ${issue.count.toLocaleString("en-US")} times in ${issue.sessions.toLocaleString("en-US")} sessions`;
    const fix = issue.fix
      ? `${seen} · fixed in ${issue.fix.shortSha}${issue.fix.subject ? `: ${issue.fix.subject}` : ""}`
      : `${seen} · no fix linked yet`;
    ctx.fillStyle = issue.fix ? c.fg : c.muted;
    ctx.fillText(fit(ctx, fix, rw), rx, colTop + 166);
  } else if (report.memory && report.memory.decisions + report.memory.gotchas > 0) {
    // No recurring errors: what the agents now remember instead.
    const m = report.memory;
    ctx.fillStyle = c.muted;
    ctx.font = `500 18px ${SANS}`;
    ctx.fillText("Project memory", rx, colTop);
    ctx.fillStyle = c.fg;
    ctx.font = `600 40px ${SANS}`;
    ctx.fillText(
      fit(ctx, `${compact(m.decisions)} decisions · ${compact(m.gotchas)} gotchas`, rw),
      rx,
      colTop + 64,
    );
    ctx.fillStyle = c.muted;
    ctx.font = `400 20px ${SANS}`;
    ctx.fillText(
      `across ${m.projectsWithMemory} ${m.projectsWithMemory === 1 ? "project" : "projects"}`,
      rx,
      colTop + 100,
    );
  }

  ctx.fillStyle = c.muted;
  ctx.font = `400 16px ${SANS}`;
  ctx.textAlign = "right";
  ctx.fillText("callimachus.app", CARD_WIDTH - pad, CARD_HEIGHT - 32);
  ctx.textAlign = "left";
  if (report.spend && report.spend.total.totalCost > 0)
    ctx.fillText(
      "Spend is the list-price equivalent of recorded token usage, not a bill.",
      pad,
      CARD_HEIGHT - 32,
    );
  return canvas;
}

/** The card as PNG bytes, at its true 1200 x 630 size. */
export async function reportCardPng(report: Report, theme: CardTheme): Promise<Uint8Array> {
  const canvas = await drawReportCard(report, theme);
  const blob = await new Promise<Blob | null>((resolve) => canvas.toBlob(resolve, "image/png"));
  if (!blob) throw new Error("the card could not be encoded as a PNG");
  return new Uint8Array(await blob.arrayBuffer());
}
