// Epoch seconds -> short relative/absolute label.
export function formatTime(epochSeconds: number | null): string {
  if (!epochSeconds) return "";
  const d = new Date(epochSeconds * 1000);
  const now = Date.now();
  const diffDays = (now - d.getTime()) / 86_400_000;
  if (diffDays < 1) {
    return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  }
  if (diffDays < 7) {
    return d.toLocaleDateString([], { weekday: "short", hour: "2-digit", minute: "2-digit" });
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
  const escaped = snippet
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
  return escaped.split(MARK_START).join("<mark>").split(MARK_END).join("</mark>");
}

// Shorten a long absolute project path to its last two segments.
export function shortPath(path: string | null): string {
  if (!path) return "";
  const parts = path.split("/").filter(Boolean);
  return parts.length <= 2 ? path : "…/" + parts.slice(-2).join("/");
}
