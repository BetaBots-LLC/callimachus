import { type ComponentProps, memo, useContext, useEffect, useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeHighlight from "rehype-highlight";
import { unified } from "unified";
import remarkParse from "remark-parse";
import remarkRehype from "remark-rehype";
import rehypeStringify from "rehype-stringify";
// Relative import on purpose: the VS Code webview reuses this file, and its build maps
// "@/" to its own sources (only "@/lib/utils" is redirected back here).
import { rehypeSafeLinks } from "../lib/safeLinks";
import { copyText } from "../lib/clipboard";
import { Button } from "./ui/button";
import { LargeMessageStates, MAX_MARKDOWN_CHARS } from "./largeMessageState";

// Syntax-highlight fenced code: detect the language when unlabeled (assistant prose often
// omits it), ignore unknown ones, and skip fences explicitly labeled as plain text. Tool
// output is labeled `text` (see ThreadView's toolBody), which avoids auto-detection, the
// costliest step: it tries every registered grammar.
const HIGHLIGHT = {
  detect: true,
  ignoreMissing: true,
  plainText: ["text", "txt", "plain", "plaintext"],
};

// Every rendering path also runs rehypeSafeLinks: transcripts are untrusted, so links
// may only open in the system browser and images never auto-load (see lib/safeLinks).
const REHYPE: ComponentProps<typeof ReactMarkdown>["rehypePlugins"] = [
  [rehypeHighlight, HIGHLIGHT],
  rehypeSafeLinks,
];

// The block still growing in a streaming reply re-parses every frame, and auto-detecting
// an unlabeled fence runs every registered grammar over the whole block: 17-22 ms per frame
// at 4-8 KB, over the frame budget. So the live block highlights labeled fences only; it is
// re-rendered with detection once it is final (see StreamingMarkdown).
const REHYPE_LIVE: ComponentProps<typeof ReactMarkdown>["rehypePlugins"] = [
  [rehypeHighlight, { ...HIGHLIGHT, detect: false }],
  rehypeSafeLinks,
];

// A single sanitizing markdown→HTML pipeline (no allowDangerousHtml, so raw HTML in a
// transcript is stripped). We render committed messages this way and CACHE the HTML by
// source string: in a long virtualized thread, scrolling a message back into view reuses
// its HTML instead of re-parsing + re-highlighting it — the difference between smooth and
// janky on 1000+-message threads.
const processor = unified()
  .use(remarkParse)
  .use(remarkGfm)
  .use(remarkRehype)
  .use(rehypeSafeLinks)
  .use(rehypeHighlight, HIGHLIGHT)
  .use(rehypeStringify);

const htmlCache = new Map<string, string>();
const MAX_CACHE = 600;

function mdToHtml(src: string): string {
  const cached = htmlCache.get(src);
  if (cached !== undefined) return cached;
  const html = String(processor.processSync(src));
  htmlCache.set(src, html);
  if (htmlCache.size > MAX_CACHE) {
    const oldest = htmlCache.keys().next().value;
    if (oldest !== undefined) htmlCache.delete(oldest);
  }
  return html;
}

// NOTE: prose's default `pre` is dark-bg + light-text. We override the bg to `bg-muted` (light
// in light mode), so we MUST also pin the code text to `--foreground` — otherwise the default
// light pre-text renders light-on-light and washes out in light mode. `text-foreground` is
// theme-aware, so it reads in both; hljs tokens still color specific spans on top.
const PROSE =
  "prose prose-sm dark:prose-invert max-w-none break-words prose-p:my-2 prose-pre:my-3 prose-pre:overflow-x-auto prose-pre:rounded-md prose-pre:bg-muted prose-pre:text-foreground prose-pre:p-3 prose-code:rounded prose-code:bg-muted prose-code:px-1 prose-code:py-0.5 prose-code:before:content-none prose-code:after:content-none prose-pre:prose-code:bg-transparent prose-pre:prose-code:p-0";

// An expanded large message shows this much at a time. A pre-wrap block of a few hundred
// KB lays out in well under a frame; the largest indexed messages are over 5 MB, which
// would freeze WebKit's line breaking if mounted in one go.
const LARGE_CHUNK_CHARS = 200_000;

function charCount(n: number): string {
  return n >= 1_000_000 ? `${(n / 1_000_000).toFixed(1)}M` : `${Math.round(n / 1000)}k`;
}

/** End of the chunk starting at `from`: the last line break in the window when there is
 *  one near its end, so each chunk is whole lines and stacks without a visible seam. */
function chunkEnd(text: string, from: number): number {
  const end = from + LARGE_CHUNK_CHARS;
  if (end >= text.length) return text.length;
  const nl = text.lastIndexOf("\n", end);
  return nl > end - LARGE_CHUNK_CHARS / 10 ? nl + 1 : end;
}

/** A message too large for the markdown pipeline. Nothing is mounted until the first
 *  expand (most are never opened, and a virtualized thread remounts rows on scroll); then
 *  it reveals one chunk per "Show more", each its own block, so a reveal lays out only
 *  the new chunk. With a `stateId` inside a LargeMessageStates provider, the open state
 *  and reveal progress survive the row being unmounted and remounted. */
function LargeMessage({ text, stateId }: { text: string; stateId?: number }) {
  const store = useContext(LargeMessageStates);
  const saved = stateId != null ? store?.get(stateId) : undefined;
  // A saved state for a different text (the row now holds other content) is ignored.
  const restore =
    saved && (saved.ends[saved.ends.length - 1] ?? 0) <= text.length ? saved : undefined;
  const [open, setOpen] = useState(restore?.open ?? false);
  const [ends, setEnds] = useState<number[]>(restore?.ends ?? []);
  const [copy, setCopy] = useState<"idle" | "copied" | "failed">("idle");
  useEffect(() => {
    if (store && stateId != null) store.set(stateId, { open, ends });
  }, [store, stateId, open, ends]);
  const shown = ends.length ? ends[ends.length - 1] : 0;
  const showMore = () =>
    setEnds((prev) => {
      const last = prev.length ? prev[prev.length - 1] : 0;
      return last >= text.length ? prev : [...prev, chunkEnd(text, last)];
    });
  return (
    <details
      className="rounded-md border bg-muted/30 px-3 py-2 text-sm"
      open={open}
      onToggle={(e) => {
        const now = e.currentTarget.open;
        if (now !== open) setOpen(now);
        if (now && ends.length === 0) showMore();
      }}
    >
      <summary className="cursor-pointer select-none text-muted-foreground">
        Large message ({charCount(text.length)} chars) · click to expand
      </summary>
      {ends.length > 0 && (
        <>
          <div className="mt-2 max-h-[60vh] overflow-auto whitespace-pre-wrap wrap-break-word font-mono text-[0.85rem]">
            {ends.map((end, i) => (
              // biome-ignore lint/suspicious/noArrayIndexKey: chunks only append, so the index is a stable identity
              <div key={i}>{text.slice(i === 0 ? 0 : ends[i - 1], end)}</div>
            ))}
          </div>
          <div className="mt-2 flex flex-wrap items-center gap-2 text-xs text-muted-foreground">
            {shown < text.length && (
              <>
                <span>
                  Showing {charCount(shown)} of {charCount(text.length)} chars
                </span>
                <Button size="xs" variant="outline" onClick={showMore}>
                  Show more
                </Button>
              </>
            )}
            <Button
              size="xs"
              variant="ghost"
              onClick={() =>
                copyText(text).then(
                  () => setCopy("copied"),
                  () => setCopy("failed"),
                )
              }
            >
              {copy === "copied"
                ? "Copied"
                : copy === "failed"
                  ? "Couldn't copy"
                  : "Copy full message"}
            </Button>
          </div>
        </>
      )}
    </details>
  );
}

/** Static markdown (committed messages). Memoized so a parent re-render that
 *  doesn't change `children` skips the (expensive) re-parse. Very large messages
 *  fall back to a collapsed plain block to avoid a parse freeze. */
export const Markdown = memo(function Markdown({
  children,
  stateId,
}: {
  children: string;
  /** Keys a large message's expanded state in an enclosing LargeMessageStates. */
  stateId?: number;
}) {
  if (children.length > MAX_MARKDOWN_CHARS)
    return <LargeMessage text={children} stateId={stateId} />;
  return (
    <div
      className={PROSE}
      // biome-ignore lint/security/noDangerouslySetInnerHtml: sanitized unified-pipeline HTML (no raw passthrough), cached for scroll perf
      dangerouslySetInnerHTML={{ __html: mdToHtml(children) }}
    />
  );
});

/** Inline markdown for short strings (distilled facts, summaries): renders inline
 *  code / emphasis / links but unwraps the block `<p>` so it flows inside a list item
 *  or sentence. No syntax-highlight pass — these are one-liners, not code blocks. */
export const InlineMarkdown = memo(function InlineMarkdown({ children }: { children: string }) {
  return (
    <span className="[&_code]:rounded [&_code]:bg-muted [&_code]:px-1 [&_code]:py-0.5 [&_code]:text-[0.85em]">
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        components={{
          // Unwrap blocks and links: distilled facts are one-liners, and any embedded
          // link (often a scraped relative file path) is dead in the webview — show its
          // text, not a broken anchor. Images show their alt text and never load.
          p: ({ children }) => <>{children}</>,
          a: ({ children }) => <>{children}</>,
          img: ({ alt }) => <>{alt ?? ""}</>,
        }}
      >
        {children}
      </ReactMarkdown>
    </span>
  );
});

/** Split markdown into top-level blocks on blank lines, without breaking fenced
 *  code. Only the trailing (growing) block changes while streaming. */
export function splitBlocks(src: string): string[] {
  const lines = src.split("\n");
  const blocks: string[] = [];
  let cur: string[] = [];
  let inFence = false;
  let fence = "";
  for (const line of lines) {
    const t = line.trimStart();
    const m = t.match(/^(```|~~~)/);
    if (m) {
      if (!inFence) {
        inFence = true;
        fence = m[1];
      } else if (t.startsWith(fence)) {
        inFence = false;
      }
    }
    if (!inFence && line.trim() === "") {
      if (cur.length) {
        blocks.push(cur.join("\n"));
        cur = [];
      }
    } else {
      cur.push(line);
    }
  }
  if (cur.length) blocks.push(cur.join("\n"));
  return blocks;
}

const Block = memo(function Block({ src, live }: { src: string; live: boolean }) {
  return (
    <ReactMarkdown remarkPlugins={[remarkGfm]} rehypePlugins={live ? REHYPE_LIVE : REHYPE}>
      {src}
    </ReactMarkdown>
  );
});

/** Incremental markdown for the streaming reply: each block is memoized, so only
 *  the last (growing) block re-parses per frame instead of the whole message. The
 *  growing block skips language auto-detection; a block that is final (a later one has
 *  started, or `streaming` is false) re-renders once with it. */
export const StreamingMarkdown = memo(function StreamingMarkdown({
  children,
  streaming = true,
}: {
  children: string;
  /** False once this text can no longer grow (e.g. a tool step followed it). */
  streaming?: boolean;
}) {
  const blocks = splitBlocks(children);
  return (
    <div className={PROSE}>
      {blocks.map((b, i) => (
        // Each block fades in once when it first mounts, then grows in place without
        // re-animating — so the reply reveals smoothly block-by-block instead of snapping in.
        // Index key is stable (streaming only appends blocks); reduced-motion disables the fade.
        // biome-ignore lint/suspicious/noArrayIndexKey: streaming blocks only append, so the index is a stable identity
        <div key={i} className="animate-in fade-in duration-300 motion-reduce:animate-none">
          <Block src={b} live={streaming && i === blocks.length - 1} />
        </div>
      ))}
    </div>
  );
});
