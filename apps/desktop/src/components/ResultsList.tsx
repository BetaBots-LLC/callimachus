import { type UIEvent, useEffect, useLayoutEffect, useRef, useState } from "react";
import { keepPreviousData, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useVirtualizer } from "@tanstack/react-virtual";
import {
  api,
  SOURCE_LABELS,
  type SearchHit,
  type SearchResponse,
  type ThreadDetail,
  type ThreadSummary,
} from "../lib/api";
import { useUi } from "../store/ui";
import {
  resultsKey,
  useBuildSemanticIndex,
  useEmbedStatus,
  useIndexRewriting,
} from "../lib/queries";
import { formatTime, renderSnippet, shortPath } from "../lib/format";
import { commitSearch, noteSearch } from "../lib/telemetry";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Star } from "lucide-react";
import { cn } from "@/lib/utils";
import { Loading } from "./Loading";

type ResultItem = SearchHit | ThreadSummary;

// First page, then the most a list ever shows. Every 200-hit search fetches 800 rows with
// snippet() (4x overfetch for the per-thread cap), and snippet() is about 45% of keyword
// latency, while a typical session never scrolls past the first few dozen results. A
// first page of 60 saved 75-100 ms (30-38%) per debounced keystroke on common terms.
const FIRST_PAGE = 60;
const FULL_PAGE = 200;
// Ask for the full page this close to the end of the first one (about 10 rows).
const NEAR_END_PX = 900;
// While hybrid results come back keyword-only because the model is still loading, re-ask
// this often, for at most this many fetches of the same key (about 30 s), so the list
// upgrades on its own once the model is ready but never polls forever if it cannot load.
// "unavailable" (no cached model, or it failed to load) never polls: each retry would be
// another full keyword search on the read pool, and only a build can change the answer.
const SEMANTIC_RETRY_MS = 2_000;
const SEMANTIC_RETRY_MAX = 15;

/** What the results query caches: the page plus why the semantic pass was skipped, if it
 *  was (then the page is keyword-only). */
type ResultsData = { hits: ResultItem[]; semanticSkipped: SearchResponse["semanticSkipped"] };

const semanticSkipped = (data: ResultsData | undefined) => data?.semanticSkipped ?? null;

export function ResultsList() {
  const query = useUi((s) => s.query);
  const sources = useUi((s) => s.sources);
  const includeSubagents = useUi((s) => s.includeSubagents);
  const hybrid = useUi((s) => s.hybrid);
  const starredOnly = useUi((s) => s.starredOnly);
  const selectedTags = useUi((s) => s.selectedTags);
  const selectedThreadId = useUi((s) => s.selectedThreadId);
  const selectThread = useUi((s) => s.selectThread);
  const searchActivityAt = useUi((s) => s.searchActivityAt);

  // `file:embed/mod.rs` → code-aware search (threads mentioning that path). It has no
  // limit, so it never pages.
  const fileFilter = query.startsWith("file:") ? query.slice(5).trim() : null;
  const searching = query.length > 0 && fileFilter === null;

  // The full page applies only to the query + filters it was asked for, so any change
  // starts over at the cheap first page without an effect to reset it.
  const filterKey = JSON.stringify([
    query,
    sources,
    includeSubagents,
    hybrid,
    starredOnly,
    selectedTags,
  ]);
  const [expandedFor, setExpandedFor] = useState<string | null>(null);
  const limit = fileFilter === null && expandedFor === filterKey ? FULL_PAGE : FIRST_PAGE;

  const filters = {
    sources,
    includeSubagents,
    hybrid,
    limit,
    starred: starredOnly ? true : null,
    tags: selectedTags,
  };

  const qc = useQueryClient();
  const { data, isLoading, isFetching, isPlaceholderData, error, dataUpdatedAt, refetch } =
    useQuery<ResultsData>({
      queryKey: resultsKey(
        query,
        sources,
        includeSubagents,
        hybrid,
        starredOnly,
        selectedTags,
        fileFilter === null ? limit : null,
      ),
      queryFn: async ({ queryKey }) => {
        if (fileFilter) return { hits: await api.searchByFile(fileFilter), semanticSkipped: null };
        if (!searching) return { hits: await api.recentThreads(filters), semanticSkipped: null };
        const res = await api.searchThreads(query, filters);
        // A newer search interrupted this one, so its empty result must not stand in for
        // this query. Once it is cached, drop it if nothing shows it (typing on), so coming
        // back to it keeps the previous rows as placeholder instead of flashing "No
        // matches."; if something still shows it, mark it stale so it searches again.
        if (res.superseded) {
          setTimeout(() => {
            const cached = qc.getQueryCache().find({ queryKey, exact: true });
            if (cached && cached.getObserversCount() === 0) {
              qc.removeQueries({ queryKey, exact: true });
            } else {
              void qc.invalidateQueries({ queryKey, exact: true });
            }
          }, 0);
        }
        return { hits: res.hits, semanticSkipped: res.semanticSkipped };
      },
      // Keep the current results visible while the next search (or the full page) loads,
      // so rows never flash empty.
      placeholderData: keepPreviousData,
      // Keyword-only fallback results must not sit in the cache under the hybrid key.
      staleTime: (q) => (semanticSkipped(q.state.data) ? 0 : 5_000),
      refetchInterval: (q) =>
        semanticSkipped(q.state.data) === "loading" && q.state.dataUpdateCount < SEMANTIC_RETRY_MAX
          ? SEMANTIC_RETRY_MS
          : false,
    });

  // Analytics: one search_performed per query that settles (unchanged for 1.5 s, or a
  // result gets opened), never one per debounced keystroke and never from the queryFn.
  // The key is an in-memory de-duplication id only; the query text is never sent.
  // Placeholder data is the previous query's results, so it is skipped. `results` counts
  // the rows fetched: a full first page reports 60 (the 50-199 bucket), and 200 only when
  // the full page loads before the search settles.
  useEffect(() => {
    if (!searching || !data || isPlaceholderData) return;
    noteSearch(filterKey, {
      hybrid,
      has_filters: sources.length > 0 || starredOnly || selectedTags.length > 0 || includeSubagents,
      results: data.hits.length,
    });
  }, [
    data,
    isPlaceholderData,
    searching,
    filterKey,
    sources,
    includeSubagents,
    hybrid,
    starredOnly,
    selectedTags,
  ]);

  const parentRef = useRef<HTMLDivElement>(null);
  const items = data?.hits ?? [];
  const virtualizer = useVirtualizer({
    count: items.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => 92,
    overscan: 10,
  });

  // A different query or filter starts at the top. Without this the list kept the old
  // scroll offset, and a shorter new list then clamped it to the very end. Compared with
  // the last key, not just keyed on it: React re-runs every effect when the hidden Search
  // tab (an Activity) is shown again, whatever its deps, which would reset the list on
  // every return to the tab.
  const lastScrollTop = useRef(0);
  const scrolledFor = useRef(filterKey);
  useEffect(() => {
    if (scrolledFor.current === filterKey) return;
    scrolledFor.current = filterKey;
    if (parentRef.current) parentRef.current.scrollTop = 0;
    lastScrollTop.current = 0;
  }, [filterKey]);
  // App hides the Search tab with display: none while another tab is open. Chromium keeps
  // a hidden scroller's offset, but not every engine is guaranteed to, so put the last
  // offset seen while visible back when the tab is shown (layout effects re-run then).
  useLayoutEffect(() => {
    const el = parentRef.current;
    if (el && el.scrollTop === 0 && lastScrollTop.current > 0) el.scrollTop = lastScrollTop.current;
  }, []);

  // Scrolled near the end of the first page: ask for the full page. Only a downward user
  // scroll triggers it (not the browser clamping the offset when a list shrinks), so a
  // short list that fits on screen never costs a second query. A search page can come back
  // short of 60 (the per-thread cap) while more threads match, so for a search any first
  // page qualifies; the recent list is exact.
  function onScroll(e: UIEvent<HTMLDivElement>) {
    // clientHeight is 0 while hidden: an engine resetting the offset then is not the reader
    // scrolling, and must not overwrite the offset restored on reveal.
    if (e.currentTarget.clientHeight === 0) return;
    const top = e.currentTarget.scrollTop;
    const down = top > lastScrollTop.current;
    lastScrollTop.current = top;
    if (!down || limit !== FIRST_PAGE || fileFilter !== null || isPlaceholderData) return;
    if (!searching && items.length < FIRST_PAGE) return;
    // Scroll geometry, not getVirtualItems(): this handler runs before the virtualizer has
    // seen the same event, so its range is one event behind, and a single jump to the end
    // (End key, scrollbar drag) would never register.
    const el = e.currentTarget;
    if (top + el.clientHeight >= el.scrollHeight - NEAR_END_PX) setExpandedFor(filterKey);
  }

  if (isLoading) return <Loading label={searching ? "Searching…" : "Loading…"} />;
  if (error) return <div className="p-6 text-sm text-destructive">{String(error)}</div>;
  if (items.length === 0) {
    return (
      <div className="p-6 text-sm text-muted-foreground">
        {searching ? "No matches." : "No threads indexed yet — hit Reindex."}
      </div>
    );
  }

  // The watcher saw new transcripts after these results were fetched (main.tsx no longer
  // re-runs a typed search on its own). Placeholder data belongs to another key.
  const newActivity = query.length > 0 && !isPlaceholderData && searchActivityAt > dataUpdatedAt;
  const capped = fileFilter === null && items.length >= FULL_PAGE;

  return (
    <div className="flex h-full flex-col">
      {hybrid && searching && semanticSkipped(data) === "loading" && (
        <div role="status" className="border-b px-4 py-1.5 text-xs text-muted-foreground">
          Semantic index is loading, keyword results shown
        </div>
      )}
      {hybrid && searching && semanticSkipped(data) === "unavailable" && <SemanticUnavailable />}
      <div className="relative min-h-0 flex-1">
        {isFetching && (
          <div className="absolute inset-x-0 top-0 z-20 h-0.5 animate-pulse bg-primary/70" />
        )}
        {newActivity && (
          <div className="pointer-events-none absolute inset-x-0 top-2 z-30 flex justify-center">
            <Button
              size="xs"
              variant="outline"
              className="pointer-events-auto bg-background shadow-sm"
              disabled={isFetching}
              onClick={() => void refetch()}
            >
              New activity. Refresh results
            </Button>
          </div>
        )}
        <div ref={parentRef} onScroll={onScroll} className="h-full overflow-y-auto">
          <div className="relative w-full" style={{ height: virtualizer.getTotalSize() }}>
            {virtualizer.getVirtualItems().map((vrow) => {
              const item = items[vrow.index];
              const threadId = "threadId" in item ? item.threadId : item.id;
              // Search hits carry the matched message: open the thread scrolled to it.
              const messageId = "messageId" in item ? item.messageId : undefined;
              const ts = "ts" in item ? item.ts : item.updatedAt;
              const active = threadId === selectedThreadId;
              return (
                <div
                  key={vrow.key}
                  data-index={vrow.index}
                  ref={virtualizer.measureElement}
                  className="group absolute left-0 top-0 w-full"
                  style={{ transform: `translateY(${vrow.start}px)` }}
                >
                  <button
                    type="button"
                    onClick={() => {
                      commitSearch(); // opening a result means the search was "performed"
                      selectThread(threadId, messageId);
                    }}
                    className={cn(
                      "block w-full cursor-pointer border-b px-4 py-2.5 pr-9 text-left hover:bg-muted/50",
                      active && "bg-muted shadow-[inset_3px_0_0_var(--primary)]",
                    )}
                  >
                    <div className="flex items-center gap-2">
                      <Badge variant="outline" className="shrink-0 text-[0.62rem] uppercase">
                        {SOURCE_LABELS[item.source]}
                      </Badge>
                      <span className="flex-1 truncate text-sm font-medium">
                        {item.title || "Untitled thread"}
                      </span>
                      <span className="shrink-0 text-[0.7rem] text-muted-foreground">
                        {formatTime(ts)}
                      </span>
                    </div>
                    {"snippet" in item ? (
                      <div
                        className="mt-1 line-clamp-2 text-[0.82rem] text-muted-foreground [&_mark]:rounded-sm [&_mark]:bg-primary/25 [&_mark]:text-foreground"
                        // biome-ignore lint/security/noDangerouslySetInnerHtml: renderSnippet escapes &, < and > first and only adds <mark> tags
                        dangerouslySetInnerHTML={{ __html: renderSnippet(item.snippet) }}
                      />
                    ) : (
                      <div className="mt-1 text-[0.72rem] text-muted-foreground">
                        {item.messageCount} messages
                      </div>
                    )}
                    <div className="mt-1 text-[0.72rem] text-muted-foreground">
                      {shortPath(item.projectPath)}
                    </div>
                  </button>
                  {"starred" in item && <StarButton threadId={item.id} starred={item.starred} />}
                </div>
              );
            })}
          </div>
          {capped && (
            <p className="px-4 py-3 text-center text-xs text-muted-foreground">
              {query.length > 0
                ? `Showing the top ${FULL_PAGE}. Refine the query to see more.`
                : `Showing the ${FULL_PAGE} most recent threads. Search to find older ones.`}
            </p>
          )}
        </div>
      </div>
    </div>
  );
}

/** Hybrid search ran keyword-only because no embedding model is cached (or it failed to
 *  load). Nothing changes that until a build runs, so offer one instead of polling. */
function SemanticUnavailable() {
  const build = useBuildSemanticIndex();
  const running = useEmbedStatus().data?.running === true;
  // Build returns without doing anything while Shrink index / Reclaim space runs.
  const rewriting = useIndexRewriting();
  return (
    <div
      role="status"
      className="flex items-center gap-2 border-b px-4 py-1.5 text-xs text-muted-foreground"
    >
      <span className="flex-1">Semantic index isn't built. Build it to search by meaning.</span>
      <Button
        size="xs"
        variant="outline"
        onClick={() => build.mutate()}
        disabled={running || build.isPending || rewriting}
      >
        {running ? "Building…" : "Build semantic index"}
      </Button>
    </div>
  );
}

/** Star toggle overlaid on a recent-thread row (visible on hover, or always when starred). */
function StarButton({ threadId, starred }: { threadId: number; starred: boolean }) {
  const queryClient = useQueryClient();
  const toggle = useMutation({
    mutationFn: () => api.setStar(threadId, !starred),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["results"] });
      // Patch the open thread (if cached) rather than refetching its whole transcript.
      queryClient.setQueryData<ThreadDetail | null>(["thread", threadId], (d) =>
        d ? { ...d, starred: !starred } : d,
      );
    },
  });
  return (
    <button
      type="button"
      onClick={() => toggle.mutate()}
      title={starred ? "Unstar" : "Star"}
      className={cn(
        "absolute right-1.5 top-2 rounded p-1 transition-opacity hover:bg-muted",
        starred
          ? "text-primary opacity-100"
          : "text-muted-foreground opacity-0 group-hover:opacity-100",
      )}
    >
      <Star className={cn("size-3.5", starred && "fill-current")} />
    </button>
  );
}
