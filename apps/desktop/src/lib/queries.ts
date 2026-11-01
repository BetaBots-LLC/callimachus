// Shared query definitions for data several views observe, so every observer agrees
// on the same key, fetcher and refresh policy.
import {
  type QueryClient,
  queryOptions,
  useIsMutating,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";
import { api, type JobsStatus } from "./api";

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

/** Read a push-only cache slot. Event handlers and long-running mutations write these
 *  with setQueryData, so the query function only reads what is already cached and never
 *  calls the backend. Living in the cache, the value survives a view remount. */
export function usePushed<T>(key: readonly string[]): T | null {
  const queryClient = useQueryClient();
  const q = useQuery<T | null>({
    queryKey: key,
    queryFn: () => queryClient.getQueryData<T | null>(key) ?? null,
    staleTime: Number.POSITIVE_INFINITY,
  });
  return q.data ?? null;
}

function usePushedError(key: readonly string[]): string | null {
  return usePushed<string>(key);
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

/** Re-check interval for `jobs_status` while a job runs. It answers from memory, so this
 *  is cheap, and it is never polled while every job is idle. */
const JOB_POLL_MS = 1_500;

/** Whether any background job (reindex, semantic build, distill, index rewrite) holds its
 *  flag. Reads every field, so a job the backend adds later counts too. */
export function anyJobRunning(s: JobsStatus | null | undefined): boolean {
  return !!s && Object.values(s).some((v) => v === true);
}

/** Background-job flags. Polls only while one is set, so buttons that must not overlap a
 *  job re-enable when it ends; a job started while all were idle shows up on the next
 *  fetch (mount, or a fresh check before acting). */
export const jobsStatusQuery = queryOptions({
  queryKey: ["jobs_status"],
  queryFn: api.jobsStatus,
  refetchInterval: (q) => (anyJobRunning(q.state.data) ? JOB_POLL_MS : false),
});

export function useJobsStatus() {
  return useQuery(jobsStatusQuery);
}

/** A fresh `jobs_status`, also refreshing every observer of the shared query. */
export function fetchJobsStatus(queryClient: QueryClient): Promise<JobsStatus> {
  return queryClient.fetchQuery({ ...jobsStatusQuery, staleTime: 0 });
}

/** Mutation-key prefix shared by Shrink index and Reclaim space (CleanupCard). */
export const REWRITE_KEY = ["index_rewrite"] as const;

/** Whether Shrink index / Reclaim space is rewriting the file. Reindex and the semantic
 *  build return without doing anything meanwhile, and distill does nothing, so their
 *  buttons disable on this. The mutation cache covers a rewrite started from this window
 *  (`jobs_status` is polled only once it has seen a job running). */
export function useIndexRewriting(): boolean {
  // Both hooks run on every render (no short-circuit), per the rules of hooks.
  const started = useIsMutating({ mutationKey: REWRITE_KEY }) > 0;
  const reported = useJobsStatus().data?.rewrite === true;
  return started || reported;
}

/** Whether `index_source` would refuse to start now: another reindex holds the flag, or
 *  Shrink index / Reclaim space is rewriting the file. */
export function reindexBlocked(s: JobsStatus | null | undefined): boolean {
  return !!s && (s.index || s.rewrite);
}

/** Resolve once a single-source reindex could start, polling only while it couldn't. */
export async function waitForReindexSlot(queryClient: QueryClient): Promise<void> {
  while (reindexBlocked(await fetchJobsStatus(queryClient))) {
    await new Promise((resolve) => setTimeout(resolve, JOB_POLL_MS));
  }
}
