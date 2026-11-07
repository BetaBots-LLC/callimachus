import type { MouseEvent } from "react";
import { type AskSource, SOURCE_LABELS } from "../lib/api";
import { shortPath } from "../lib/format";
import { useUi } from "../store/ui";
import { Badge } from "@/components/ui/badge";
import { Markdown } from "./Markdown";

/** In-page href a citation chip carries. Fragment links survive rehypeSafeLinks and
 *  main.tsx's link guard (both leave "#..." alone), so the click handler below owns them. */
const CITE_HREF = "#cal-thread-";

// "[thread 12]", and the grouped forms answers also use: "[thread 12, 34]",
// "[threads 12, thread 34]". Not when it is already a link ("[thread 12](...)").
const CITATION = /\[threads? (\d+(?:\s*,\s*(?:thread )?\d+)*)\](?!\()/g;

/** Markdown link title text: one line, with the quote and backslash escaped. */
function linkTitle(title: string): string {
  return title.replace(/\s+/g, " ").replace(/[\\"]/g, "\\$&");
}

/** Rewrite [thread N] citations into links to CITE_HREF, outside fenced code (where they
 *  are literal text). `titles` names a cited thread on hover when the answer knows it. */
export function linkCitations(text: string, titles?: ReadonlyMap<number, string>): string {
  let inFence = false;
  return text
    .split("\n")
    .map((line) => {
      if (/^\s*(```|~~~)/.test(line)) {
        inFence = !inFence;
        return line;
      }
      if (inFence) return line;
      return line.replace(CITATION, (_, ids: string) =>
        ids
          .split(",")
          .map((part) => part.replace(/thread/, "").trim())
          .map((id) => {
            const title = titles?.get(Number(id));
            return `[thread ${id}](${CITE_HREF}${id}${title ? ` "${linkTitle(title)}"` : ""})`;
          })
          .join(" "),
      );
    })
    .join("\n");
}

/** A source as an answer knows it: the live answer has all of AskSource, a saved one only
 *  the id and title its sources footer kept. */
export type CitedSource = Pick<AskSource, "threadId" | "title"> &
  Partial<Pick<AskSource, "source" | "projectPath" | "messageId">>;

// The footer ask_history's `answerWithSources` ends with, the text a saved Ask turn holds:
// a blank line, "Sources:", then one "[thread <id>] <title>" line per source.
const SOURCES_FOOTER = /\n\nSources:((?:\n\[thread \d+\] [^\n]*)+)\s*$/;

/** Split a saved Ask answer into its text and the sources its footer lists, so a reopened
 *  chat shows the same chips and source rows as the live answer. Text without the footer
 *  comes back whole, with no sources. */
export function splitSavedSources(text: string): { body: string; sources: CitedSource[] } {
  const m = text.match(SOURCES_FOOTER);
  if (!m || m.index === undefined) return { body: text, sources: [] };
  const sources = m[1]
    .split("\n")
    .filter(Boolean)
    .map((line) => {
      const [, id, title] = line.match(/^\[thread (\d+)\] (.*)$/) ?? [];
      return { threadId: Number(id), title: title && title !== "untitled" ? title : null };
    });
  return { body: text.slice(0, m.index), sources };
}

/** An assistant answer as markdown, with its [thread N] citations as chips that open the
 *  thread in Search: at the message the answer drew on when the source knows it, else at
 *  the top. */
export function CitedMarkdown({ text, sources }: { text: string; sources?: CitedSource[] }) {
  const openThread = useUi((s) => s.openThread);
  const titles = sources
    ? new Map(sources.filter((s) => s.title).map((s) => [s.threadId, s.title as string]))
    : undefined;
  const onClick = (e: MouseEvent<HTMLDivElement>) => {
    const anchor = (e.target as Element).closest?.(`a[href^="${CITE_HREF}"]`);
    if (!anchor) return;
    // Before main.tsx's document-level guard, which then sees defaultPrevented; without it
    // the fragment would land in the URL.
    e.preventDefault();
    const id = Number(anchor.getAttribute("href")?.slice(CITE_HREF.length));
    if (Number.isFinite(id) && id > 0) {
      openThread(id, sources?.find((s) => s.threadId === id)?.messageId ?? null);
    }
  };
  return (
    // biome-ignore lint/a11y/useKeyWithClickEvents lint/a11y/noStaticElementInteractions: delegation only; the chips are real links, so Enter on a focused chip fires this click
    <div className="cited-markdown" onClick={onClick}>
      <Markdown>{linkCitations(text, titles)}</Markdown>
    </div>
  );
}

/**
 * An "Ask your history" answer in Chat: the cited markdown, then the threads it drew on.
 * Each source opens its thread in Search, at its matching message when known. Moved here
 * from the old Ask tab.
 */
export function AskAnswer({ answer, sources }: { answer: string; sources: CitedSource[] }) {
  const openThread = useUi((s) => s.openThread);
  return (
    <div className="space-y-4">
      <CitedMarkdown text={answer} sources={sources} />
      {sources.length > 0 && (
        <section aria-label="Sources">
          <h3 className="mb-1.5 text-[13px] font-medium text-muted-foreground">Sources</h3>
          <ul className="divide-y overflow-hidden rounded-lg border">
            {sources.map((s) => (
              <li key={s.threadId}>
                <button
                  type="button"
                  onClick={() => openThread(s.threadId, s.messageId ?? null)}
                  className="flex w-full cursor-pointer items-center gap-2 px-3 py-2 text-left text-sm transition-colors outline-none hover:bg-muted/50 focus-visible:ring-3 focus-visible:ring-inset focus-visible:ring-ring/50"
                >
                  {s.source && (
                    <Badge variant="outline" className="shrink-0 text-[0.65rem]">
                      {SOURCE_LABELS[s.source]}
                    </Badge>
                  )}
                  <span className="min-w-0 flex-1 truncate">{s.title || "Untitled thread"}</span>
                  <span className="shrink-0 text-xs text-muted-foreground tabular-nums">
                    thread {s.threadId}
                    {s.projectPath ? ` · ${shortPath(s.projectPath)}` : ""}
                  </span>
                </button>
              </li>
            ))}
          </ul>
        </section>
      )}
    </div>
  );
}
