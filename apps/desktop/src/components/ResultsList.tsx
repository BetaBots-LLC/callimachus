import { useRef } from "react";
import { useQuery } from "@tanstack/react-query";
import { useVirtualizer } from "@tanstack/react-virtual";
import { api, SOURCE_LABELS, type SearchHit, type ThreadSummary } from "../lib/api";
import { useUi } from "../store/ui";
import { formatTime, renderSnippet, shortPath } from "../lib/format";
import { Badge } from "@/components/ui/badge";
import { cn } from "@/lib/utils";

type ResultItem = SearchHit | ThreadSummary;

export function ResultsList() {
  const query = useUi((s) => s.query);
  const sources = useUi((s) => s.sources);
  const includeSubagents = useUi((s) => s.includeSubagents);
  const hybrid = useUi((s) => s.hybrid);
  const selectedThreadId = useUi((s) => s.selectedThreadId);
  const selectThread = useUi((s) => s.selectThread);

  const filters = { sources, includeSubagents, hybrid, limit: 200 };
  const searching = query.length > 0;

  const { data, isLoading, error } = useQuery<ResultItem[]>({
    queryKey: ["results", query, sources, includeSubagents, hybrid],
    queryFn: async () =>
      searching ? await api.searchThreads(query, filters) : await api.recentThreads(filters),
  });

  const parentRef = useRef<HTMLDivElement>(null);
  const items = data ?? [];
  const virtualizer = useVirtualizer({
    count: items.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => 92,
    overscan: 10,
  });

  if (isLoading) return <div className="p-6 text-sm text-muted-foreground">Loading…</div>;
  if (error) return <div className="p-6 text-sm text-destructive">{String(error)}</div>;
  if (items.length === 0) {
    return (
      <div className="p-6 text-sm text-muted-foreground">
        {searching ? "No matches." : "No threads indexed yet — hit Reindex."}
      </div>
    );
  }

  return (
    <div ref={parentRef} className="h-full overflow-y-auto">
      <div className="relative w-full" style={{ height: virtualizer.getTotalSize() }}>
        {virtualizer.getVirtualItems().map((vrow) => {
          const item = items[vrow.index];
          const threadId = "threadId" in item ? item.threadId : item.id;
          const ts = "ts" in item ? item.ts : item.updatedAt;
          const active = threadId === selectedThreadId;
          return (
            <div
              key={vrow.key}
              data-index={vrow.index}
              ref={virtualizer.measureElement}
              className="absolute left-0 top-0 w-full"
              style={{ transform: `translateY(${vrow.start}px)` }}
            >
              <button
                onClick={() => selectThread(threadId)}
                className={cn(
                  "block w-full cursor-pointer border-b px-4 py-2.5 text-left hover:bg-muted/50",
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
            </div>
          );
        })}
      </div>
    </div>
  );
}
