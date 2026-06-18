import { memo } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";

const PROSE =
  "prose prose-sm dark:prose-invert max-w-none break-words prose-p:my-2 prose-pre:my-2 prose-pre:overflow-x-auto prose-pre:rounded-md prose-pre:bg-muted prose-pre:p-3 prose-code:rounded prose-code:bg-muted prose-code:px-1 prose-code:py-0.5 prose-code:before:content-none prose-code:after:content-none prose-pre:prose-code:bg-transparent prose-pre:prose-code:p-0";

// Above this, parsing markdown synchronously can jank the main thread (e.g. a
// pasted/packed-context message). Render those as plain, collapsed text instead.
const MAX_MARKDOWN_CHARS = 16_000;

/** Static markdown (committed messages). Memoized so a parent re-render that
 *  doesn't change `children` skips the (expensive) re-parse. Very large messages
 *  fall back to a collapsed plain block to avoid a parse freeze. */
export const Markdown = memo(function Markdown({ children }: { children: string }) {
  if (children.length > MAX_MARKDOWN_CHARS) {
    return (
      <details className="rounded-md border bg-muted/30 px-3 py-2 text-sm">
        <summary className="cursor-pointer select-none text-muted-foreground">
          Large message ({Math.round(children.length / 1000)}k chars) — click to expand
        </summary>
        <pre className="mt-2 max-h-[60vh] overflow-auto whitespace-pre-wrap wrap-break-word text-[0.85rem]">
          {children}
        </pre>
      </details>
    );
  }
  return (
    <div className={PROSE}>
      <ReactMarkdown remarkPlugins={[remarkGfm]}>{children}</ReactMarkdown>
    </div>
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

const Block = memo(function Block({ src }: { src: string }) {
  return <ReactMarkdown remarkPlugins={[remarkGfm]}>{src}</ReactMarkdown>;
});

/** Incremental markdown for the streaming reply: each block is memoized, so only
 *  the last (growing) block re-parses per frame instead of the whole message. */
export const StreamingMarkdown = memo(function StreamingMarkdown({
  children,
}: {
  children: string;
}) {
  const blocks = splitBlocks(children);
  return (
    <div className={PROSE}>
      {blocks.map((b, i) => (
        <Block key={i} src={b} />
      ))}
    </div>
  );
});
