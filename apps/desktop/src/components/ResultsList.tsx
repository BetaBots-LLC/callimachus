import { useRef } from "react";
import { keepPreviousData, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useVirtualizer } from "@tanstack/react-virtual";
import { api, SOURCE_LABELS, type SearchHit, type ThreadSummary } from "../lib/api";
import { useUi } from "../store/ui";
import { formatTime, renderSnippet, shortPath } from "../lib/format";
import { Badge } from "@/components/ui/badge";
import { Loader2, Star } from "lucide-react";
import { cn } from "@/lib/utils";

type ResultItem = SearchHit | ThreadSummary;

export function ResultsList() {
  const query = useUi((s) => s.query);
  const sources = useUi((s) => s.sources);
  const includeSubagents = useUi((s) => s.includeSubagents);
  const hybrid = useUi((s) => s.hybrid);
  const starredOnly = useUi((s) => s.starredOnly);
  const selectedTags = useUi((s) => s.selectedTags);
  const selectedThreadId = useUi((s) => s.selectedThreadId);
  const selectThread = useUi((s) => s.selectThread);

  const filters = {
    sources,
    includeSubagents,
    hybrid,
    limit: 200,
    starred: starredOnly ? true : null,
    tags: selectedTags,
  };
  const searching = query.length > 0;

  const { data, isLoading, isFetching, error } = useQuery<ResultItem[]>({
    queryKey: ["results", query, sources, includeSubagents, hybrid, starredOnly, selectedTags],
    queryFn: async () =>
      searching ? await api.searchThreads(query, filters) : await api.recentThreads(filters),
    // Keep the current results visible while the next search loads (no empty flash).
    placeholderData: keepPreviousData,
  });

  const parentRef = useRef<HTMLDivElement>(null);
  const items = data ?? [];
  const virtualizer = useVirtualizer({
    count: items.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => 92,
    overscan: 10,
  });

  if (isLoading)
    return (
      <div className="flex items-center justify-center gap-2 p-6 text-sm text-muted-foreground">
        <Loader2 className="size-4 animate-spin" />
        {searching ? "Searching…" : "Loading…"}
      </div>
    );
  if (error) return <div className="p-6 text-sm text-destructive">{String(error)}</div>;
  if (items.length === 0) {
    return (
      <div className="p-6 text-sm text-muted-foreground">
        {searching ? "No matches." : "No threads indexed yet — hit Reindex."}
      </div>
    );
  }

  return (
    <div className="relative h-full">
      {isFetching && (
        <div className="absolute inset-x-0 top-0 z-20 h-0.5 animate-pulse bg-primary/70" />
      )}
      <div ref={parentRef} className="h-full overflow-y-auto">
        <div className="relative w-full" style={{ height: virtualizer.getTotalSize() }}>
          {virtualizer.getVirtualItems().map((vrow) => {
            const item = items[vrow.index];
            const threadId = "threadId" in item ? item.threadId : item.id;
            // Search hits carry the matched message — open the thread scrolled to it.
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
                  onClick={() => selectThread(threadId, messageId)}
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
      </div>
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
      queryClient.invalidateQueries({ queryKey: ["thread", threadId] });
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
