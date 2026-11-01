// Lazy stand-ins for ./Markdown, for the components on the startup path (ThreadView and
// KnowledgeSection sit inside the Search view, which is in the entry chunk). The markdown
// parse + highlight stack (react-markdown, unified, highlight.js) is about a third of the
// startup JS, yet nothing on the first screen renders markdown. Importing it through these
// wrappers moves it out of the entry: App starts loading it right after first paint (see
// preloadMarkdown), and anything rendered before it lands shows as plain text for a moment.
//
// Views that are already lazy chunks (Chat, Ask, Projects) import ./Markdown directly.

import { lazy, memo, Suspense } from "react";

/** Start loading the markdown chunk. Idempotent: the module loader caches the import. */
export const preloadMarkdown = () => import("./Markdown");

const MarkdownImpl = lazy(() => preloadMarkdown().then((m) => ({ default: m.Markdown })));
const InlineMarkdownImpl = lazy(() =>
  preloadMarkdown().then((m) => ({ default: m.InlineMarkdown })),
);

// The placeholder only shows until the chunk arrives, so a prefix is plenty; mounting a
// multi-MB message as one text node here would cost more than the chunk saves.
const PLACEHOLDER_CHARS = 4_000;

function placeholder(text: string): string {
  return text.length > PLACEHOLDER_CHARS ? `${text.slice(0, PLACEHOLDER_CHARS)}…` : text;
}

/** ./Markdown's Markdown, loaded on first use. Each instance has its own Suspense
 *  boundary, so a row waiting on the chunk never blanks the whole view. */
export const Markdown = memo(function LazyMarkdown({ children }: { children: string }) {
  return (
    <Suspense
      fallback={
        <div className="whitespace-pre-wrap wrap-break-word text-sm">{placeholder(children)}</div>
      }
    >
      <MarkdownImpl>{children}</MarkdownImpl>
    </Suspense>
  );
});

/** ./Markdown's InlineMarkdown, loaded on first use. */
export const InlineMarkdown = memo(function LazyInlineMarkdown({ children }: { children: string }) {
  return (
    <Suspense fallback={<span>{placeholder(children)}</span>}>
      <InlineMarkdownImpl>{children}</InlineMarkdownImpl>
    </Suspense>
  );
});
