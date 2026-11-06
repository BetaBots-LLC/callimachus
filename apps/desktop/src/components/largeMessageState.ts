import { createContext } from "react";

/** Above this, parsing markdown synchronously can jank the main thread (e.g. a
 *  pasted/packed-context message), so Markdown renders it as plain, collapsed text and
 *  ThreadView's row estimate treats it as a collapsed block. */
export const MAX_MARKDOWN_CHARS = 16_000;

/** How far a LargeMessage has been opened: whether it is expanded, and the end offset of
 *  each revealed chunk. */
export type LargeMessageState = { open: boolean; ends: number[] };

/** Per-message LargeMessage state, keyed by message id. A virtualized list remounts a row
 *  each time it scrolls back into view, so a row's own useState would forget an expanded,
 *  half-revealed message; the list provides this map (one per thread) and each row reads
 *  its saved state on mount. A plain mutable Map, not React state: nothing renders from
 *  it except a row that is mounting. Lives outside Markdown.tsx so the list can provide it
 *  without importing the lazily loaded markdown chunk. */
export const LargeMessageStates = createContext<Map<number, LargeMessageState> | null>(null);
