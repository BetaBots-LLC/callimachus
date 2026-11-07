import {
  type ReactNode,
  type Ref,
  type UIEvent,
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { keepPreviousData, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useVirtualizer } from "@tanstack/react-virtual";
import {
  api,
  type FactHit,
  SOURCE_LABELS,
  type SearchHit,
  type SearchResponse,
  type ThreadDetail,
  type ThreadSummary,
} from "../lib/api";
import {
  type DatePreset,
  RESULTS_LIST_ID,
  resultOptionId,
  resultsNav,
  searchRange,
  useUi,
} from "../store/ui";
import {
  MAX_HITS_PER_THREAD,
  resultsKey,
  useBuildSemanticIndex,
  useBuildStarting,
  useEmbedStatus,
  useIndexRewriting,
  useJobsStatus,
  useReindex,
  useReindexStarting,
  userReindexing,
} from "../lib/queries";
import { baseName, formatTime, leadToMatch, rangePhrase, renderSnippet } from "../lib/format";
import { commitSearch, noteSearch } from "../lib/telemetry";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Spinner } from "@/components/ui/spinner";
import { Pin, Star } from "lucide-react";
import { cn } from "@/lib/utils";
import { Loading } from "./Loading";

type ResultItem = SearchHit | ThreadSummary;

/** The backend's SearchHit has no tool name yet (a hit on a tool call reads "Assistant:",
 *  any result "Tool output:"); read it when a backend sends one so a tool hit can say
 *  "Bash output:" instead. */
type Hit = SearchHit & { toolName?: string | null };

// The facts group stays a short preface to the thread rows, never a second result list.
const MAX_FACTS = 5;
// With a date range the facts are filtered here by when they were distilled (search_facts
// takes no range), so a wider page is asked for to still fill the group.
const FACTS_FETCH_DATED = 40;

/** One list row: a thread (recent list, `file:` lookup) or a thread's best search hit
 *  plus every matched message of that thread, in rank order. */
type Row =
  | { kind: "thread"; thread: ThreadSummary }
  | { kind: "hit"; hit: Hit; matchIds: number[] };

/** Fold search hits into one row per thread, keeping rank order: the backend returns up
 *  to 3 hits per thread (MAX_HITS_PER_THREAD), and "auth" filled the top 10 rows with only
 *  7 threads when every hit was its own row with the same title. */
function groupRows(items: ResultItem[]): Row[] {
  const rows: Row[] = [];
  const byThread = new Map<number, number[]>();
  for (const item of items) {
    if (!("messageId" in item)) {
      rows.push({ kind: "thread", thread: item });
      continue;
    }
    const ids = byThread.get(item.threadId);
    if (ids) {
      ids.push(item.messageId);
      continue;
    }
    const matchIds = [item.messageId];
    byThread.set(item.threadId, matchIds);
    rows.push({ kind: "hit", hit: item, matchIds });
  }
  return rows;
}

/** Where a hit matched, since most matches are tool output (120 of 200 for "auth"). */
function hitOrigin(hit: Hit): string {
  if (hit.role === "user") return "You:";
  if (hit.role === "tool") return hit.toolName ? `${hit.toolName} output:` : "Tool output:";
  if (hit.toolName) return `${hit.toolName} call:`;
  if (hit.role === "assistant") return "Assistant:";
  return `${hit.role.charAt(0).toUpperCase()}${hit.role.slice(1)}:`;
}

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

// Whether the last hybrid search found no usable model ("unavailable"). Kept past turning
// semantic off, so the keyword empty state does not offer "Try semantic search" when that
// would only come back keyword-only again. The next hybrid search that runs clears it.
let semanticUnavailable = false;

export function ResultsList() {
  const query = useUi((s) => s.query);
  const sources = useUi((s) => s.sources);
  const includeSubagents = useUi((s) => s.includeSubagents);
  const hybrid = useUi((s) => s.hybrid);
  const starredOnly = useUi((s) => s.starredOnly);
  const selectedTags = useUi((s) => s.selectedTags);
  const project = useUi((s) => s.project);
  const datePreset = useUi((s) => s.datePreset);
  const after = useUi((s) => s.after);
  const before = useUi((s) => s.before);
  const scopeReady = useUi((s) => s.scopeReady);
  const selectedThreadId = useUi((s) => s.selectedThreadId);
  const selectThread = useUi((s) => s.selectThread);
  const searchActivityAt = useUi((s) => s.searchActivityAt);
  const highlightedIndex = useUi((s) => s.highlightedIndex);
  const setHighlightedIndex = useUi((s) => s.setHighlightedIndex);
  const setResultCount = useUi((s) => s.setResultCount);

  // `file:embed/mod.rs` → code-aware search (threads mentioning that path). It has no
  // limit, so it never pages.
  const fileFilter = query.startsWith("file:") ? query.slice(5).trim() : null;
  const searching = query.length > 0 && fileFilter === null;
  // The recent list ignores the semantic switch, so it stays out of that list's key: the
  // switch turning itself on at launch must not refetch (or reset) the recent threads.
  const hybridKey = searching && hybrid;

  // The full page applies only to the query + filters it was asked for, so any change
  // starts over at the cheap first page without an effect to reset it.
  const filterKey = JSON.stringify([
    query,
    sources,
    includeSubagents,
    hybridKey,
    starredOnly,
    selectedTags,
    project,
    datePreset,
    after,
    before,
  ]);
  const [expandedFor, setExpandedFor] = useState<string | null>(null);
  const limit = fileFilter === null && expandedFor === filterKey ? FULL_PAGE : FIRST_PAGE;

  // A preset's bound is taken when the search runs (searchRange), so a refetch tomorrow
  // still means "the last 24 hours"; the key holds the preset name, not the bound.
  const filters = () => ({
    sources,
    includeSubagents,
    hybrid,
    limit,
    starred: starredOnly ? true : null,
    tags: selectedTags,
    project,
    ...searchRange({ datePreset, after, before }),
  });

  const qc = useQueryClient();
  const { data, isLoading, isFetching, isPlaceholderData, error, dataUpdatedAt, refetch } =
    useQuery<ResultsData>({
      queryKey: resultsKey(
        query,
        sources,
        includeSubagents,
        hybridKey,
        starredOnly,
        selectedTags,
        project,
        datePreset,
        after,
        before,
        fileFilter === null ? limit : null,
      ),
      // Not before the remembered repo scope is known: an unscoped first page would show
      // and then be replaced by the scoped one.
      enabled: scopeReady,
      queryFn: async ({ queryKey }) => {
        if (fileFilter) return { hits: await api.searchByFile(fileFilter), semanticSkipped: null };
        if (!searching) return { hits: await api.recentThreads(filters()), semanticSkipped: null };
        const res = await api.searchThreads(query, filters());
        if (hybrid && !res.superseded) {
          semanticUnavailable = res.semanticSkipped === "unavailable";
        }
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
      has_filters:
        sources.length > 0 ||
        starredOnly ||
        selectedTags.length > 0 ||
        includeSubagents ||
        project !== null ||
        datePreset !== "all",
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
    project,
    datePreset,
  ]);

  // Decisions and gotchas matching the query in the same repo scope, shown above the
  // threads. Keyword-only and cheap (search_facts answers in milliseconds); an older
  // backend without the command rejects, and the group simply does not show. A fact has no
  // source, star or tag of its own, so with a source chip, Starred or a tag narrowing the
  // threads the group is left out rather than shown from outside that scope.
  const narrowed = sources.length > 0 || starredOnly || selectedTags.length > 0;
  const factsOn = searching && scopeReady && !narrowed;
  const dated = datePreset !== "all";
  const factsQuery = useQuery({
    queryKey: ["search_facts", query, project, dated],
    queryFn: () => api.searchFacts(query, project, dated ? FACTS_FETCH_DATED : MAX_FACTS),
    enabled: factsOn,
    placeholderData: keepPreviousData,
    staleTime: 5_000,
    retry: false,
  });
  // The date range applies by when a fact was distilled. A hit without `createdAt` (an
  // older backend) cannot be placed in the range, so it is left out while one is set.
  const factRange = searchRange({ datePreset, after, before });
  const inFactRange = (f: FactHit) =>
    !dated ||
    (f.createdAt != null &&
      (factRange.after === null || f.createdAt >= factRange.after) &&
      (factRange.before === null || f.createdAt <= factRange.before));
  const facts: FactHit[] =
    factsOn && !factsQuery.isError
      ? (factsQuery.data ?? []).filter(inFactRange).slice(0, MAX_FACTS)
      : [];
  // The keys wait for this query's facts as they wait for its rows: otherwise index 0 is a
  // fact one moment and a thread the next.
  const factsSettled =
    !factsOn || factsQuery.isError || (factsQuery.isSuccess && !factsQuery.isPlaceholderData);

  const parentRef = useRef<HTMLDivElement>(null);
  const items = data?.hits ?? [];
  const rows = useMemo(() => groupRows(data?.hits ?? []), [data]);
  // The facts group sits above the virtual rows inside the same scroller, so the
  // virtualizer offsets every row by its measured height.
  // A callback ref, since the group's element changes between the empty and list layouts.
  const [factsHeight, setFactsHeight] = useState(0);
  const factsRef = useCallback((el: HTMLDivElement | null) => {
    if (!el) return;
    const update = () => setFactsHeight(el.offsetHeight);
    update();
    const ro = new ResizeObserver(update);
    ro.observe(el);
    return () => {
      ro.disconnect();
      setFactsHeight(0);
    };
  }, []);
  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => parentRef.current,
    scrollMargin: factsHeight,
    // Two lines at py-2: about 56 px, so about 10 rows fit a 760 px window (was 7 at 84).
    estimateSize: () => 56,
    overscan: 10,
  });

  // Keyboard indices run through the facts first, then the thread rows.
  const factCount = factsSettled ? facts.length : 0;
  // Opening a row: the thread, scrolled to its best match, with every match for the
  // thread's match navigation. The highlight follows, so the arrow keys go on from here.
  // A fact opens its thread at the message it came from, when known.
  function openRow(index: number) {
    if (index < factCount) {
      const fact = facts[index];
      commitSearch();
      setHighlightedIndex(index);
      selectThread(fact.threadId, fact.sourceMessageId ?? null);
      return;
    }
    const row = rows[index - factCount];
    if (!row) return;
    commitSearch(); // opening a result means the search was "performed"
    setHighlightedIndex(index);
    if (row.kind === "hit") selectThread(row.hit.threadId, row.hit.messageId, row.matchIds);
    else selectThread(row.thread.id);
  }
  // Enter with no row highlighted opens the top thread: the facts are a preface to the
  // results, not the result the query asked for. With no thread rows, the top fact.
  const openTop = () => openRow(rows.length > 0 ? factCount : 0);
  // Placeholder rows are another query's: the keys must not act on them (ArrowDown+Enter
  // right after typing opened the previous query's top thread). The full page loading for
  // the same query and filters is the exception, since its first rows are the ones shown.
  const freshFor = useRef<string | null>(null);
  if (data && !isPlaceholderData) freshFor.current = filterKey;
  const fresh =
    scopeReady &&
    !isLoading &&
    !error &&
    (!isPlaceholderData || freshFor.current === filterKey) &&
    factsSettled;
  // SearchBar's arrow keys clamp to the row count and Enter opens through resultsNav. Both
  // are withdrawn while the list is hidden (Activity cleans effects up) or unmounted.
  const rowCount = fresh ? factCount + rows.length : 0;
  useEffect(() => {
    setResultCount(rowCount);
    return () => setResultCount(0);
  }, [rowCount, setResultCount]);
  // Enter pressed before this query's rows arrived opens the top row once they do.
  const openFirstPending = useUi((s) => s.openFirstPending);
  const setOpenFirstPending = useUi((s) => s.setOpenFirstPending);
  useEffect(() => {
    if (!openFirstPending || !(fresh || error)) return;
    setOpenFirstPending(false);
    if (rowCount > 0) openTop();
  });
  useEffect(() => {
    resultsNav.open = openRow;
    resultsNav.openTop = openTop;
  });
  useEffect(
    () => () => {
      resultsNav.open = () => {};
      resultsNav.openTop = () => {};
    },
    [],
  );
  // Keep the keyboard-highlighted row in view. Compared with the last index scrolled to,
  // so re-showing the tab (which re-runs effects) does not yank the list back to it.
  const scrolledToIndex = useRef(highlightedIndex);
  useEffect(() => {
    if (scrolledToIndex.current === highlightedIndex) return;
    scrolledToIndex.current = highlightedIndex;
    if (highlightedIndex >= 0 && highlightedIndex < factCount) {
      document
        .getElementById(resultOptionId(highlightedIndex))
        ?.scrollIntoView({ block: "nearest" });
    } else if (highlightedIndex >= factCount && highlightedIndex < factCount + rows.length) {
      virtualizer.scrollToIndex(highlightedIndex - factCount, { align: "auto" });
    }
  }, [highlightedIndex, factCount, rows.length, virtualizer]);

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
    setHighlightedIndex(-1); // the highlighted row belonged to the old list
  }, [filterKey, setHighlightedIndex]);
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

  if (!scopeReady || isLoading) return <Loading label={searching ? "Searching…" : "Loading…"} />;
  if (error) return <div className="p-6 text-sm text-destructive">{String(error)}</div>;
  // Above the empty state too: a keyword-only "No matches" is exactly when the reader needs
  // to know the semantic pass did not run, and how to make it run.
  const banners = hybrid && searching && (
    <>
      {semanticSkipped(data) === "loading" && (
        <div role="status" className="border-b px-4 py-1.5 text-xs text-muted-foreground">
          Semantic index is loading, keyword results shown
        </div>
      )}
      {semanticSkipped(data) === "unavailable" && <SemanticUnavailable />}
    </>
  );
  const factsGroup = facts.length > 0 && (
    <FactsGroup
      ref={factsRef}
      facts={facts}
      navigable={factCount > 0}
      highlightedIndex={highlightedIndex}
      showProject={project === null}
      onOpen={openRow}
    />
  );
  if (items.length === 0) {
    return (
      <div className="flex h-full flex-col">
        {banners}
        {factsGroup && (
          <div
            id={RESULTS_LIST_ID}
            role="listbox"
            aria-label="Search results"
            className="max-h-[50%] shrink-0 overflow-y-auto"
          >
            {factsGroup}
          </div>
        )}
        <EmptyResults searching={searching} fileFilter={fileFilter} />
      </div>
    );
  }

  // The watcher saw new transcripts after these results were fetched (main.tsx no longer
  // re-runs a typed search on its own). Placeholder data belongs to another key.
  const newActivity = query.length > 0 && !isPlaceholderData && searchActivityAt > dataUpdatedAt;
  const capped = fileFilter === null && items.length >= FULL_PAGE;

  return (
    <div className="flex h-full flex-col">
      {banners}
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
          <div
            id={RESULTS_LIST_ID}
            role="listbox"
            aria-label={searching ? "Search results" : "Recent threads"}
          >
            {factsGroup}
            <div
              className="relative w-full"
              // getTotalSize already leaves out the scroll margin (the facts group).
              style={{ height: virtualizer.getTotalSize() }}
            >
              {virtualizer.getVirtualItems().map((vrow) => {
                const row = rows[vrow.index];
                const item = row.kind === "hit" ? row.hit : row.thread;
                const threadId = row.kind === "hit" ? row.hit.threadId : row.thread.id;
                const ts = row.kind === "hit" ? row.hit.ts : row.thread.updatedAt;
                const project = item.projectPath ? baseName(item.projectPath) : null;
                const active = threadId === selectedThreadId;
                const optionIndex = factCount + vrow.index;
                const highlighted = optionIndex === highlightedIndex;
                // Light muted text on the darker selected background measures under 4.5:1,
                // so the selected row's meta text steps up to foreground/70 there.
                const meta = active
                  ? "text-foreground/70 dark:text-muted-foreground"
                  : "text-muted-foreground";
                return (
                  <div
                    key={vrow.key}
                    data-index={vrow.index}
                    ref={virtualizer.measureElement}
                    role="presentation"
                    className="group absolute left-0 top-0 w-full"
                    style={{ transform: `translateY(${vrow.start - factsHeight}px)` }}
                  >
                    <button
                      type="button"
                      id={resultOptionId(optionIndex)}
                      role="option"
                      aria-selected={highlighted}
                      onClick={() => openRow(optionIndex)}
                      className={cn(
                        "block w-full cursor-pointer border-b px-4 py-2 text-left outline-none hover:bg-muted/50 focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring/60",
                        // Room for the star button, which only thread rows have.
                        row.kind === "thread" && "pr-9",
                        // The open row is marked by its background alone. The keyboard ring
                        // is for the row the arrows are on, so it never sits on the open
                        // row, where it read as a stuck focus ring after a click.
                        active &&
                          "bg-foreground/[0.07] hover:bg-foreground/[0.08] dark:bg-muted dark:hover:bg-muted",
                        highlighted && !active && "bg-muted/60 ring-2 ring-inset ring-ring/60",
                      )}
                    >
                      <div className="flex items-baseline gap-2">
                        <span className="min-w-0 flex-1 truncate text-sm font-medium">
                          {item.title || "Untitled thread"}
                        </span>
                        {row.kind === "hit" && row.matchIds.length > 1 && (
                          <span className={cn("shrink-0 text-xs", meta)}>
                            {row.matchIds.length >= MAX_HITS_PER_THREAD
                              ? `${MAX_HITS_PER_THREAD}+`
                              : row.matchIds.length}{" "}
                            matches
                          </span>
                        )}
                        <span className={cn("shrink-0 text-xs tabular-nums", meta)}>
                          {formatTime(ts)}
                        </span>
                      </div>
                      {row.kind === "hit" ? (
                        <div className={cn("mt-0.5 flex gap-1 text-[13px]", meta)}>
                          <span className="min-w-0 flex-1 truncate">
                            <span className="text-foreground/70">{hitOrigin(row.hit)}</span>{" "}
                            <span
                              className="[&_mark]:rounded-sm [&_mark]:bg-primary/25 [&_mark]:text-foreground"
                              // biome-ignore lint/security/noDangerouslySetInnerHtml: renderSnippet escapes &, < and > first and only adds <mark> tags
                              dangerouslySetInnerHTML={{
                                __html: renderSnippet(leadToMatch(row.hit.snippet)),
                              }}
                            />
                          </span>
                          {project && (
                            <span className="shrink-0" title={item.projectPath ?? undefined}>
                              · {project}
                            </span>
                          )}
                        </div>
                      ) : (
                        <div
                          className={cn("mt-0.5 truncate text-[13px]", meta)}
                          title={item.projectPath ?? undefined}
                        >
                          {[
                            project,
                            SOURCE_LABELS[item.source],
                            `${row.thread.messageCount} ${row.thread.messageCount === 1 ? "msg" : "msgs"}`,
                          ]
                            .filter(Boolean)
                            .join(" · ")}
                        </div>
                      )}
                    </button>
                    {row.kind === "thread" && (
                      <StarButton threadId={row.thread.id} starred={row.thread.starred} />
                    )}
                  </div>
                );
              })}
            </div>
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

/** Decisions and gotchas matching the query: options of the results listbox ahead of the
 *  thread rows, so the arrow keys run from the facts into the threads. */
function FactsGroup({
  ref,
  facts,
  navigable,
  highlightedIndex,
  showProject,
  onOpen,
}: {
  ref: Ref<HTMLDivElement>;
  facts: FactHit[];
  /** False while the facts on screen are the previous query's (placeholder). */
  navigable: boolean;
  highlightedIndex: number;
  /** Off when the search is scoped to one repo: every fact is from it. */
  showProject: boolean;
  onOpen: (index: number) => void;
}) {
  return (
    // biome-ignore lint/a11y/useSemanticElements: an ARIA group of options inside the results listbox (listbox > group > option); a fieldset groups form controls, not options
    <div
      ref={ref}
      role="group"
      aria-labelledby="facts-group-label"
      className="border-b bg-muted/30"
    >
      <div
        id="facts-group-label"
        className="px-4 pt-2 pb-1 text-xs font-medium text-muted-foreground"
      >
        Decisions and gotchas
      </div>
      {facts.map((fact, i) => {
        const highlighted = navigable && i === highlightedIndex;
        const gotcha = fact.kind === "gotcha";
        return (
          <button
            key={fact.id}
            type="button"
            id={navigable ? resultOptionId(i) : undefined}
            role="option"
            aria-selected={highlighted}
            // The previous query's facts, still shown while this query's load: they are not
            // this list's options, so they are visibly inert until replaced.
            disabled={!navigable}
            aria-disabled={!navigable || undefined}
            onClick={() => onOpen(i)}
            title={navigable ? "Open the thread this came from" : undefined}
            className={cn(
              "flex w-full items-start gap-2 px-4 py-1.5 text-left outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring/60",
              navigable ? "cursor-pointer hover:bg-muted/60" : "cursor-default opacity-60",
              highlighted && "bg-muted/70 ring-2 ring-inset ring-ring/60",
            )}
          >
            <Badge
              variant="outline"
              className={cn(
                "mt-px w-[4.5rem] shrink-0 font-normal",
                gotcha && "border-amber-500/40 text-amber-700 dark:text-amber-400",
              )}
            >
              {gotcha ? "Gotcha" : "Decision"}
            </Badge>
            <span className="line-clamp-2 min-w-0 flex-1 text-[13px] leading-snug">
              {fact.text}
            </span>
            {(fact.pinned || (showProject && fact.project)) && (
              <span className="mt-px flex shrink-0 items-center gap-1.5 text-xs text-muted-foreground">
                {showProject && fact.project && (
                  <span className="max-w-[12ch] truncate" title={fact.project}>
                    {baseName(fact.project)}
                  </span>
                )}
                {fact.pinned && (
                  <Pin className="size-3 fill-current text-primary" aria-label="Pinned" />
                )}
              </span>
            )}
          </button>
        );
      })}
    </div>
  );
}

const PRESET_PHRASE: Record<Exclude<DatePreset, "custom" | "all">, string> = {
  "24h": "for the last 24 hours",
  "7d": "for the last 7 days",
  "30d": "for the last 30 days",
};

/** Nothing to show: name what narrowed the list and offer the way out (clear the filters,
 *  try semantic search, or reindex an empty index). */
function EmptyResults({
  searching,
  fileFilter,
}: {
  searching: boolean;
  fileFilter: string | null;
}) {
  const query = useUi((s) => s.query);
  const sources = useUi((s) => s.sources);
  const starredOnly = useUi((s) => s.starredOnly);
  const selectedTags = useUi((s) => s.selectedTags);
  const hybrid = useUi((s) => s.hybrid);
  const toggleHybrid = useUi((s) => s.toggleHybrid);
  const clearCollectionFilters = useUi((s) => s.clearCollectionFilters);
  const clearSources = useUi((s) => s.clearSources);
  const project = useUi((s) => s.project);
  const setProject = useUi((s) => s.setProject);
  const datePreset = useUi((s) => s.datePreset);
  const after = useUi((s) => s.after);
  const before = useUi((s) => s.before);
  const setDatePreset = useUi((s) => s.setDatePreset);
  const reindex = useReindex();
  const reindexStarting = useReindexStarting();
  const indexing = userReindexing(useJobsStatus().data);
  const rewriting = useIndexRewriting();
  const embed = useEmbedStatus().data;
  const embedRunning = embed?.running === true;
  // Semantic search can only find something once some messages are embedded; offering it
  // on an empty semantic index just swapped one "No matches" for another.
  const semanticReady = (embed?.done ?? 0) > 0 && !semanticUnavailable;

  const filters = [
    ...sources.map((s) => SOURCE_LABELS[s]),
    ...(starredOnly ? ["starred"] : []),
    ...selectedTags.map((t) => `#${t}`),
  ];
  // "Nothing in payments-api for the last 7 days.": the repo first, then any other filter,
  // then the period, so the sentence names everything that narrowed the list.
  const where = [...(project ? [baseName(project)] : []), ...filters];
  const scope = where.length > 0 ? ` in ${where.join(", ")}` : "";
  const dated = datePreset !== "all";
  const period =
    datePreset === "custom"
      ? rangePhrase(after, before)
      : datePreset === "all"
        ? ""
        : PRESET_PHRASE[datePreset];
  const when = period ? ` ${period}` : "";
  // Published while shown: this Reindex is then the one action, and the thread pane has
  // nothing to point at.
  const nothingIndexed =
    fileFilter === null && !searching && filters.length === 0 && !project && !dated;
  const setIndexEmpty = useUi((s) => s.setIndexEmpty);
  useEffect(() => {
    if (!nothingIndexed) return;
    setIndexEmpty(true);
    return () => setIndexEmpty(false);
  }, [nothingIndexed, setIndexEmpty]);
  const clearFilters = filters.length > 0 && (
    <Button
      size="xs"
      variant="outline"
      onClick={() => {
        clearCollectionFilters();
        clearSources();
      }}
    >
      Clear filters
    </Button>
  );

  // Widening the scope is the likeliest way to a result, so it comes first.
  const widen = (
    <>
      {project && (
        <Button size="xs" variant="outline" onClick={() => setProject(null)}>
          Search all repos
        </Button>
      )}
      {dated && (
        <Button size="xs" variant="outline" onClick={() => setDatePreset("all")}>
          Search all time
        </Button>
      )}
    </>
  );

  let message: string;
  let actions: ReactNode = null;
  if (fileFilter !== null) {
    message = `No threads mention ${fileFilter || "that file"}.`;
  } else if (searching) {
    message = `No matches for "${query}"${scope}${when}.`;
    actions = (
      <>
        {widen}
        {clearFilters}
        {!hybrid && semanticReady && (
          <Button size="xs" variant="outline" onClick={toggleHybrid}>
            Try semantic search
          </Button>
        )}
      </>
    );
  } else if (where.length > 0 || dated) {
    message = `Nothing${scope}${when}.`;
    actions = (
      <>
        {widen}
        {clearFilters}
      </>
    );
  } else {
    message = "Nothing indexed yet. Reindex to scan your agents' history.";
    actions = (
      <Button
        size="xs"
        onClick={() => reindex.mutate()}
        disabled={indexing || reindexStarting || embedRunning || rewriting}
      >
        {indexing ? (
          <>
            <Spinner /> Indexing…
          </>
        ) : (
          "Reindex"
        )}
      </Button>
    );
  }
  return (
    <div role="status" className="flex flex-wrap items-center gap-x-3 gap-y-2 p-6">
      <p className="text-sm text-muted-foreground">{message}</p>
      {actions && <div className="flex gap-2">{actions}</div>}
    </div>
  );
}

/** Hybrid search ran keyword-only because no embedding model is cached (or it failed to
 *  load). Nothing changes that until a build runs, so offer one instead of polling. */
function SemanticUnavailable() {
  const build = useBuildSemanticIndex();
  const starting = useBuildStarting();
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
        disabled={running || starting || rewriting}
      >
        {running ? (
          <>
            <Spinner /> Building…
          </>
        ) : (
          "Build semantic index"
        )}
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
