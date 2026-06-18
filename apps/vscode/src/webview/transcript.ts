// Parse the packed `cal cat` transcript into a structured, renderable shape.
//
// The packed format (see context.rs `pack_thread`) is a markdown body wrapped in
// an XML-ish envelope:
//
//   <reference_thread source="…" title="…" project="…">
//   <!-- optional budget note -->
//   ### user
//   …
//   ### assistant
//   …
//   ### tool: Bash
//   …
//   ### … 4 turns elided …
//   </reference_thread>
//
// We strip the envelope (surfacing its metadata) and split the body on the
// recognised `### role` headings so the UI can render each turn distinctly —
// without needing a message-level JSON endpoint from `cal`.

export interface TranscriptMeta {
  source: string | null;
  title: string | null;
  project: string | null;
  note: string | null;
}

export type TurnKind = "user" | "assistant" | "tool" | "divider";

export interface Turn {
  id: number;
  kind: TurnKind;
  label: string;
  text: string;
}

export interface Transcript {
  meta: TranscriptMeta;
  turns: Turn[];
}

const HEADING = /^###\s+(.+?)\s*$/;

/** Classify a `### …` heading, or return null if it's ordinary content. */
function classify(heading: string): Pick<Turn, "kind" | "label"> | null {
  const s = heading.trim();
  if (/elided/i.test(s)) return { kind: "divider", label: s.replace(/…/g, "").trim() };
  const tool = s.match(/^tool:\s*(.+)$/i);
  if (tool) return { kind: "tool", label: `Tool · ${tool[1].trim()}` };
  if (/^tool result$/i.test(s)) return { kind: "tool", label: "Tool result" };
  if (/^user$/i.test(s)) return { kind: "user", label: "You" };
  if (/^assistant$/i.test(s)) return { kind: "assistant", label: "Assistant" };
  if (/^system$/i.test(s)) return { kind: "assistant", label: "System" };
  return null; // an h3 inside message content — keep it as content, not a turn.
}

export function parseTranscript(raw: string, fallbackTitle?: string | null): Transcript {
  const meta: TranscriptMeta = {
    source: null,
    title: fallbackTitle ?? null,
    project: null,
    note: null,
  };
  let body = raw.trim();

  const open = body.match(/^<reference_thread\b([^>]*)>/);
  if (open) {
    const attr = (k: string) => open[1].match(new RegExp(`${k}="([^"]*)"`))?.[1] ?? null;
    meta.source = attr("source");
    meta.title = attr("title") || meta.title;
    meta.project = attr("project");
    body = body.slice(open[0].length);
  }
  body = body.replace(/<\/reference_thread>\s*$/, "");

  const note = body.match(/^\s*<!--\s*([\s\S]*?)\s*-->/);
  if (note) {
    meta.note = note[1].trim();
    body = body.slice((note.index ?? 0) + note[0].length);
  }

  const turns: Omit<Turn, "id">[] = [];
  let cur: Omit<Turn, "id"> | null = null;
  for (const line of body.split("\n")) {
    const m = line.match(HEADING);
    const cls = m ? classify(m[1]) : null;
    if (cls) {
      if (cur) turns.push(cur);
      cur = { ...cls, text: "" };
    } else if (cur) {
      cur.text += (cur.text ? "\n" : "") + line;
    } else if (line.trim()) {
      // Stray preamble before the first role heading.
      cur = { kind: "assistant", label: "", text: line };
    }
  }
  if (cur) turns.push(cur);

  for (const t of turns) t.text = t.text.trim();
  return {
    meta,
    turns: turns
      .filter((t) => t.kind === "divider" || t.text.length > 0)
      .map((t, i) => ({ ...t, id: i })),
  };
}
