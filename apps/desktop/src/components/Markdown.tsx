import { type ComponentProps, memo } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeHighlight from "rehype-highlight";
import { unified } from "unified";
import remarkParse from "remark-parse";
import remarkRehype from "remark-rehype";
import rehypeStringify from "rehype-stringify";

// Syntax-highlight fenced code; detect the language when unlabeled, ignore unknown.
const REHYPE: ComponentProps<typeof ReactMarkdown>["rehypePlugins"] = [
  [rehypeHighlight, { detect: true, ignoreMissing: true }],
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
  .use(rehypeHighlight, { detect: true, ignoreMissing: true })
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
          // text, not a broken anchor.
          p: ({ children }) => <>{children}</>,
          a: ({ children }) => <>{children}</>,
        }}
      >
        {children}
      </ReactMarkdown>
    </span>
  );
});

/** Wrap arbitrary text (e.g. tool output) as one fenced code block so it renders
 *  monospaced + syntax-highlighted through the Markdown pipeline. The fence is one
 *  backtick longer than any run already in the text, so embedded fences can't escape. */
export function asCodeBlock(text: string, lang = ""): string {
  const longest = (text.match(/`+/g) ?? []).reduce((m, s) => Math.max(m, s.length), 0);
  const fence = "`".repeat(Math.max(3, longest + 1));
  return `${fence}${lang}\n${text}\n${fence}`;
}

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
  return (
    <ReactMarkdown remarkPlugins={[remarkGfm]} rehypePlugins={REHYPE}>
      {src}
    </ReactMarkdown>
  );
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
        // biome-ignore lint/suspicious/noArrayIndexKey: streaming blocks only append, so the index is a stable identity
        <Block key={i} src={b} />
      ))}
    </div>
  );
});
