// Shared query definitions for data several views observe, so every observer agrees
// on the same key, fetcher and refresh policy.
import { queryOptions, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "./api";

/** While an embedding build runs, a slow safety-net refetch in case a push event is
 *  missed (for example after a webview reload mid-build). */
const EMBED_SAFETY_NET_MS = 60_000;

/**
 * Embedding-build status. `embedding_status` runs two full-table COUNT(*) scans
 * (~0.9s on a large index), so this query never refetches on its own when idle:
 *  - staleTime is infinite, so mounting a view reuses the cached value;
 *  - live progress arrives through the `embed:progress` event (main.tsx writes it into
 *    the cache), and `embed:done` / `index:done` invalidate it for one authoritative
 *    refetch;
 *  - only while a build is running is there a slow safety-net poll, and the poll is
 *    off (`false`) whenever `running` is false.
 */
export const embedStatusQuery = queryOptions({
  queryKey: ["embed_status"],
  queryFn: api.embeddingStatus,
  staleTime: Number.POSITIVE_INFINITY,
  refetchInterval: (q) => (q.state.data?.running ? EMBED_SAFETY_NET_MS : false),
});

export function useEmbedStatus() {
  return useQuery(embedStatusQuery);
}

/** Cache slot for the last `embed:error` message (main.tsx writes it). */
export const EMBED_ERROR_KEY = ["embed_error"] as const;

/** Cache slot for the last `index:error` message (main.tsx writes it). */
export const INDEX_ERROR_KEY = ["index_error"] as const;

/** Read a push-only error slot. main.tsx writes these from backend events, so the query
 *  function only reads what is already cached and never calls the backend. */
function usePushedError(key: readonly string[]): string | null {
  const queryClient = useQueryClient();
  const q = useQuery<string | null>({
    queryKey: key,
    queryFn: () => queryClient.getQueryData<string | null>(key) ?? null,
    staleTime: Number.POSITIVE_INFINITY,
  });
  return q.data ?? null;
}

/** Why the last semantic-index build stopped, or null. Cleared once a build makes
 *  progress again. */
export function useEmbedError(): string | null {
  return usePushedError(EMBED_ERROR_KEY);
}

/** Why the last full reindex failed, or null. Cleared once a later full reindex
 *  finishes cleanly. A failed scan still ends with an (empty) `index:done`, so views
 *  check this before reading an empty index as "no history". */
export function useIndexError(): string | null {
  return usePushedError(INDEX_ERROR_KEY);
}
