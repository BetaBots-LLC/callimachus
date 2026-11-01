import { QueryClient } from "@tanstack/react-query";

// The app's single QueryClient. It lives in its own module (not main.tsx) so that
// non-React code, such as the chat stream in store/chat.ts, can invalidate queries
// without importing the app entry point.
export const queryClient = new QueryClient({
  defaultOptions: {
    // Window refocus would otherwise fire a ~5-query burst, each serialized behind
    // the app's single DB connection. A short stale window keeps it calm. Local invoke
    // calls mostly fail deterministically, so one quick retry (not the default 3 with
    // exponential backoff, ~7s) surfaces errors fast instead of looking "stuck".
    queries: { refetchOnWindowFocus: false, staleTime: 5_000, retry: 1, retryDelay: 400 },
  },
});

/**
 * Stale window for heavy, index-derived aggregates (index_stats, coach_overview,
 * recurring_issues, spend, projects). Each costs hundreds of ms to over a second of SQL,
 * and they only change when the index does, which main.tsx already signals by
 * invalidating them on `index:done` and marking them stale on `index:updated`. So a tab
 * revisit within this window reuses the cache instead of re-running the scan.
 */
export const AGGREGATE_STALE_MS = 5 * 60_000;

/** Index-derived aggregates that carry AGGREGATE_STALE_MS and rely on explicit
 *  invalidation to refresh. */
export const AGGREGATE_KEYS = [
  "coach_overview",
  "recurring_issues",
  "spend",
  "projects",
  "cleanup",
] as const;

/**
 * Mark index_stats and every aggregate stale after a write that changes the thread set
 * without an `index:done` (cleanup deletes, a per-source reindex, a saved chat). Without
 * this, the 5-minute stale window keeps listing deleted threads, or missing new ones, in
 * Stats / Coach / Projects. Only mounted views refetch now; the rest reload on next visit.
 */
export function invalidateIndexDerived() {
  for (const key of ["index_stats", ...AGGREGATE_KEYS]) {
    void queryClient.invalidateQueries({ queryKey: [key] });
  }
}
