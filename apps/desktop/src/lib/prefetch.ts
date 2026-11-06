// Warming lazy view chunks before the user opens them. App.tsx splits every view except
// Search out of the entry bundle; the first visit to a cold one showed "Loading…" for
// 0.3-0.5 s in the prod mock (26-49 ms once warm).

type Loader = () => Promise<unknown>;

/**
 * Run `cb` once the main thread is idle, or after `timeoutMs` at the latest. Returns a
 * cancel function for an effect cleanup. WKWebView (macOS) has no requestIdleCallback, so
 * there it falls back to the fixed delay; intent prefetch (below) covers that window.
 */
export function whenIdle(cb: () => void, timeoutMs: number): () => void {
  if (typeof window.requestIdleCallback === "function") {
    const id = window.requestIdleCallback(cb, { timeout: timeoutMs });
    return () => window.cancelIdleCallback(id);
  }
  const id = setTimeout(cb, timeoutMs);
  return () => clearTimeout(id);
}

/** Start a chunk download; a real failure resurfaces on visit, inside the error boundary. */
export function warm(load: Loader | undefined) {
  load?.().catch(() => {});
}

/**
 * Props for a tab button that warm its view's chunk on hover or keyboard focus, which
 * lands 100-300 ms before the click. Repeat calls are free: the module loader caches the
 * import. Pass undefined for a view that is already in the entry (Search).
 */
export function prefetchOnIntent(load: Loader | undefined) {
  if (!load) return {};
  return { onPointerEnter: () => warm(load), onFocus: () => warm(load) };
}
