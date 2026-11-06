// Shared query definitions for data several views observe, so every observer agrees
// on the same key, fetcher and refresh policy.
import {
  type QueryClient,
  queryOptions,
  useIsMutating,
  useMutation,
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

/** Mutation keys of the two job starters, so every button that starts one (the search
 *  bar, the results list, the palette) sees the same pending start: with separate
 *  isPending flags a second click elsewhere ran a second start before jobs_status
 *  reported the job. */
export const REINDEX_KEY = ["reindex"] as const;
export const BUILD_EMBEDDINGS_KEY = ["build_embeddings"] as const;

/** Whether a Reindex start from any button in this window is still in flight. */
export function useReindexStarting(): boolean {
  return useIsMutating({ mutationKey: REINDEX_KEY }) > 0;
}

/** Whether a semantic-index build start from any button in this window is in flight. */
export function useBuildStarting(): boolean {
  return useIsMutating({ mutationKey: BUILD_EMBEDDINGS_KEY }) > 0;
}

/** Start a semantic index build (the search bar's button, and the results list when a
 *  hybrid search finds no model). main.tsx toasts when it finishes. */
export function useBuildSemanticIndex() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationKey: BUILD_EMBEDDINGS_KEY,
    mutationFn: api.buildEmbeddings,
    // A retry starts clean; a new failure arrives as a fresh embed:error.
    onMutate: () => {
      queryClient.setQueryData(EMBED_ERROR_KEY, null);
      markUserStarted("embed");
    },
    // embed_status never refetches on its own while idle; pick up running:true now so the
    // progress bar appears.
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["embed_status"] });
      void confirmUserStarted(queryClient, "embed"); // a no-op start (job busy) gets no toast
    },
    onError: () => unmarkUserStarted("embed"),
  });
}

/** Start a full reindex in the background (the search bar's Reindex button, and the
 *  empty-index state of the results list). It never blocks the UI: callers reflect the
 *  job flag, and main.tsx refreshes results and stats on index:done and toasts. */
export function useReindex() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationKey: REINDEX_KEY,
    mutationFn: api.indexAll,
    onMutate: () => markUserStarted("index"), // main.tsx toasts when it finishes
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["index_status"] });
      // Also refreshes jobs_status, which then polls until the reindex ends.
      void confirmUserStarted(queryClient, "index"); // a no-op start (job busy) gets no toast
    },
    onError: () => unmarkUserStarted("index"),
  });
}

/**
 * Which background jobs the user started from the UI this session, so main.tsx can toast
 * their completion and stay quiet about the launch scan or an agent's writes. A plain set,
 * not query cache data: a cache entry with no observer is garbage-collected after gcTime,
 * which would silently drop the toast of any job longer than five minutes.
 */
const userStarted = new Set<UserJob>();
export type UserJob = "index" | "embed";
/** Call from the mutation that starts the job (onMutate). */
export function markUserStarted(job: UserJob) {
  userStarted.add(job);
}
/** Call when the start was refused or failed, so a later unrelated run gets no toast. */
export function unmarkUserStarted(job: UserJob) {
  userStarted.delete(job);
}
/** Read-and-clear, for the done/error listener. */
export function takeUserStarted(job: UserJob): boolean {
  return userStarted.delete(job);
}
/**
 * index_all and build_embeddings return without starting anything while another job holds
 * the index. After the command resolves, keep the flag only if the job is actually running.
 */
export async function confirmUserStarted(queryClient: QueryClient, job: UserJob): Promise<boolean> {
  const jobs = await fetchJobsStatus(queryClient).catch(() => null);
  if (jobs && !jobs[job]) {
    userStarted.delete(job);
    return false;
  }
  return true;
}

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

/** Mutation-key prefix shared by Shrink index and Reclaim space (IndexSection). */
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

/** A reindex the user started (Reindex, Onboarding, a source's Reindex). The quiet launch
 *  catch-up scan is a separate flag (`catchUp`, its own job), so it never drives
 *  "Indexing…", the progress bar or a disabled Reindex button: it runs on every launch and
 *  usually takes well under 2 s, so showing it read as a stuck 0% reindex. A Reindex
 *  clicked during the catch-up holds `index` while it waits for the scan, and shows as
 *  indexing from the click. */
export function userReindexing(s: JobsStatus | null | undefined): boolean {
  return !!s?.index;
}

/** The most hits the backend keeps per thread (MAX_HITS_PER_THREAD in search/mod.rs), so
 *  a thread with this many matches may have more: the UI says "3+" there. */
export const MAX_HITS_PER_THREAD = 3;

/** The ResultsList query key. The query text sits at index 1 ("" for the recent list), and
 *  main.tsx's live refresh relies on that to tell the cheap recent list from a search. */
export function resultsKey(query: string, ...rest: unknown[]) {
  return ["results", query, ...rest] as const;
}

/** Whether a cached results key is a typed search (or `file:` lookup), not the recent list. */
export function isSearchResultsKey(key: readonly unknown[]): boolean {
  return key[0] === "results" && typeof key[1] === "string" && key[1] !== "";
}
