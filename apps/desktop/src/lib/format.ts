// Epoch seconds -> short relative/absolute label. The hour is "numeric" ("1:04 PM"), as in
// formatSince: Today shows both side by side, and a 2-digit hour ("01:04 PM") read as a
// different clock.
export function formatTime(epochSeconds: number | null): string {
  if (!epochSeconds) return "";
  const d = new Date(epochSeconds * 1000);
  const now = Date.now();
  const diffDays = (now - d.getTime()) / 86_400_000;
  if (diffDays < 1) {
    return d.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
  }
  if (diffDays < 7) {
    return d.toLocaleDateString([], { weekday: "short", hour: "numeric", minute: "2-digit" });
  }
  return d.toLocaleDateString([], { year: "numeric", month: "short", day: "numeric" });
}

// Match-boundary sentinels emitted by the backend's snippet() (SQL char(1)/char(2)).
const MARK_START = String.fromCharCode(1);
const MARK_END = String.fromCharCode(2);

// Render an FTS5 snippet safely. snippet() does NOT escape the surrounding body,
// which may contain literal "<script>" etc., so HTML-escape the text FIRST and
// only then swap the control-char sentinels for <mark>.
export function renderSnippet(snippet: string): string {
  const escaped = snippet.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  return escaped.split(MARK_START).join("<mark>").split(MARK_END).join("</mark>");
}

// Trim a snippet's head so its first match starts within about `lead` characters. A
// results row shows one truncated line, and snippet() often puts the match 40+ characters
// in, past the ellipsis ("Bash output: FAIL tests/webhooks/retry.test.…" with no visible
// match). Backs up to a word boundary so the line never starts mid-word, and marks the cut
// with "…". Run it on the raw snippet, before renderSnippet.
export function leadToMatch(snippet: string, lead = 20): string {
  const at = snippet.indexOf(MARK_START);
  if (at <= lead) return snippet;
  const from = at - lead;
  const space = snippet.slice(from, at).search(/\s/);
  const start = space < 0 ? from : from + space + 1;
  return `…${snippet.slice(start)}`;
}

// USD spend formatter: thousands separators + cents. A positive sub-cent amount reads
// as "<$0.01" instead of collapsing to "$0.00"; at $0.01 and above, toLocaleString rounds
// half-up as usual. Negatives fall through to toLocaleString ("-$…") so an upstream data
// bug surfaces in the UI rather than being masked as "$0.00".
export function formatUsd(n: number): string {
  if (n === 0) return "$0.00";
  if (n > 0 && n < 0.01) return "<$0.01";
  return n.toLocaleString("en-US", {
    style: "currency",
    currency: "USD",
    minimumFractionDigits: 2,
    maximumFractionDigits: 2,
  });
}

// Path separators for both POSIX and Windows. Agent histories recorded on Windows store
// paths like `C:\Users\me\code\proj`, so splitting on "/" alone leaves them whole.
const PATH_SEP = /[\\/]+/;

/** The non-empty segments of a POSIX or Windows path. */
function pathSegments(path: string): string[] {
  return path.split(PATH_SEP).filter(Boolean);
}

// Shorten a long absolute project path to its last two segments, keeping the
// separator style the path already uses.
export function shortPath(path: string | null): string {
  if (!path) return "";
  const parts = pathSegments(path);
  if (parts.length <= 2) return path;
  const sep = path.includes("\\") && !path.includes("/") ? "\\" : "/";
  return `…${sep}${parts.slice(-2).join(sep)}`;
}

/** The last segment of a POSIX or Windows path (a folder or file name). */
export function baseName(path: string): string {
  return pathSegments(path).pop() ?? path;
}

/** When a window started, for "Since <time>": the time alone today, "yesterday at" the
 *  time, the weekday within a week, else the date. */
export function formatSince(epochSeconds: number, now = Date.now()): string {
  const d = new Date(epochSeconds * 1000);
  const time = d.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
  const startOfToday = new Date(now);
  startOfToday.setHours(0, 0, 0, 0);
  const dayStart = new Date(d);
  dayStart.setHours(0, 0, 0, 0);
  const daysAgo = Math.round((startOfToday.getTime() - dayStart.getTime()) / 86_400_000);
  if (daysAgo <= 0) return time;
  if (daysAgo === 1) return `yesterday at ${time}`;
  if (daysAgo < 7) return `${d.toLocaleDateString([], { weekday: "long" })} at ${time}`;
  return d.toLocaleDateString([], {
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
  });
}

/** A session length: "under a minute", "42 min", "1 h 5 min", "3 h". */
export function formatDuration(secs: number): string {
  if (secs < 60) return "under a minute";
  const mins = Math.round(secs / 60);
  if (mins < 60) return `${mins} min`;
  const h = Math.floor(mins / 60);
  const m = mins % 60;
  return m === 0 ? `${h} h` : `${h} h ${m} min`;
}

// Local days, as the Stats heatmap now buckets them: a day cell sends its local midnight
// and the second before the next one (`localDayBounds`), so both ends name the same day.
const shortDate = (epochSeconds: number) =>
  new Date(epochSeconds * 1000).toLocaleDateString([], { month: "short", day: "numeric" });

/** The local day holding `epochSeconds`, as an inclusive [midnight, next midnight - 1 s]
 *  range in unix seconds. Built from calendar fields, so a DST day is 23 or 25 hours. */
export function localDayBounds(epochSeconds: number): { after: number; before: number } {
  const d = new Date(epochSeconds * 1000);
  const start = new Date(d.getFullYear(), d.getMonth(), d.getDate());
  const next = new Date(d.getFullYear(), d.getMonth(), d.getDate() + 1);
  return { after: start.getTime() / 1000, before: next.getTime() / 1000 - 1 };
}

/** A date range as the end of a sentence ("Nothing in web for <range>."): "on Nov 3",
 *  "between Nov 3 and Nov 9", "since Nov 3", "before Nov 3". Empty for no range. */
export function rangePhrase(after: number | null, before: number | null): string {
  if (after !== null && before !== null) {
    const a = shortDate(after);
    const b = shortDate(before);
    return a === b ? `on ${a}` : `between ${a} and ${b}`;
  }
  if (after !== null) return `since ${shortDate(after)}`;
  if (before !== null) return `before ${shortDate(before)}`;
  return "";
}

/** The same range as a short chip label: "Nov 3", "Nov 3 to Nov 9", "since Nov 3". */
export function rangeLabel(after: number | null, before: number | null): string {
  if (after !== null && before !== null) {
    const a = shortDate(after);
    const b = shortDate(before);
    return a === b ? a : `${a} to ${b}`;
  }
  return rangePhrase(after, before);
}

/** What captured a snapshot, in words: the hook event of an auto snapshot ("PreCompact"),
 *  or "manual" for one saved by name (`cal snapshot`). Unknown events show as stored. */
export function snapshotEventText(event: string): string {
  const e = event.toLowerCase();
  if (e === "precompact") return "before compaction";
  if (e === "sessionend") return "at session end";
  if (e === "manual") return "saved by hand";
  return event;
}
