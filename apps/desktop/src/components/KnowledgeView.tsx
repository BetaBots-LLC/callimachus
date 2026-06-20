import { useRef, useState } from "react";
import { keepPreviousData, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useVirtualizer } from "@tanstack/react-virtual";
import { useDebouncedCallback } from "@tanstack/react-pacer";
import { Check } from "lucide-react";
import { api, SOURCE_LABELS, type SourceKind, type TodoFact } from "../lib/api";
import { useUi } from "../store/ui";
import { shortPath } from "../lib/format";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Item, ItemActions, ItemContent, ItemDescription, ItemTitle } from "@/components/ui/item";
import { Loading } from "./Loading";

type Mode = "todos" | "decision" | "gotcha";
const MODES: { id: Mode; label: string }[] = [
  { id: "todos", label: "TODOs" },
  { id: "decision", label: "Decisions" },
  { id: "gotcha", label: "Gotchas" },
];

type RowItem = {
  id: number;
  threadId: number;
  text: string;
  source: SourceKind;
  title: string | null;
  projectPath: string | null;
  meta?: string;
};

/**
 * Knowledge tab. TODOs mode filters your open TODOs (free tier, client-side). Decisions
 * /Gotchas modes semantically recall distilled facts across every thread. Header (search
 * + mode + count) is sticky; the list below is virtualized so it scales to any size.
 */
export function KnowledgeView() {
  const [query, setQuery] = useState("");
  const [committed, setCommitted] = useState("");
  const [mode, setMode] = useState<Mode>("todos");
  const selectThread = useUi((s) => s.selectThread);
  const setView = useUi((s) => s.setView);
  // Recall embeds the query (model inference), so debounce it.
  const commit = useDebouncedCallback((v: string) => setCommitted(v.trim()), { wait: 350 });
  const qc = useQueryClient();
  // Optimistically drop the todo so the check feels instant (no spinner waiting on
  // the write + refetch); roll back if the write fails.
  const markDone = useMutation({
    mutationFn: (id: number) => api.setTodoDone(id, true),
    onMutate: async (id: number) => {
      const key = ["open_todos", committed];
      await qc.cancelQueries({ queryKey: ["open_todos"] });
      const prev = qc.getQueryData<TodoFact[]>(key);
      if (prev)
        qc.setQueryData<TodoFact[]>(
          key,
          prev.filter((t) => t.id !== id),
        );
      return { key, prev };
    },
    onError: (_e, _id, ctx) => {
      if (ctx?.prev) qc.setQueryData(ctx.key, ctx.prev);
    },
    onSettled: () => qc.invalidateQueries({ queryKey: ["open_todos"] }),
  });

  // Server-side text search so it scales past the page limit (users with lots of todos).
  const todos = useQuery({
    queryKey: ["open_todos", committed],
    queryFn: () => api.listOpenTodos(committed || undefined),
    placeholderData: keepPreviousData,
  });
  const recall = useQuery({
    queryKey: ["recall", mode, committed],
    queryFn: () =>
      mode === "decision" ? api.recallDecisions(committed) : api.recallGotchas(committed),
    enabled: mode !== "todos" && committed.length > 0,
    placeholderData: keepPreviousData,
  });

  const items: RowItem[] =
    mode === "todos"
      ? (todos.data ?? []).map((t) => ({
          id: t.id,
          threadId: t.threadId,
          text: t.text,
          source: t.source,
          title: t.title,
          projectPath: t.projectPath,
        }))
      : (recall.data ?? []).map((h) => ({
          id: h.id,
          threadId: h.threadId,
          text: h.text,
          source: h.source,
          title: h.title,
          projectPath: h.projectPath,
          meta: `${Math.round(h.similarity * 100)}%`,
        }));

  const parentRef = useRef<HTMLDivElement>(null);
  const virtualizer = useVirtualizer({
    count: items.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => 70,
    overscan: 8,
  });

  const open = (threadId: number) => {
    selectThread(threadId);
    setView("search");
  };

  const loading = mode === "todos" ? todos.isLoading : !!committed && recall.data === undefined;
  const count =
    mode === "todos"
      ? todos.data
        ? committed
          ? `${items.length} matches`
          : `${items.length} open`
        : ""
      : items.length
        ? `${items.length} matches`
        : "";

  return (
    <div className="mx-auto flex h-full w-full max-w-3xl flex-col p-6">
      {/* Sticky header — search, mode, and count stay put while the list scrolls. */}
      <div className="shrink-0 space-y-3 pb-3">
        <Input
          value={query}
          autoFocus
          placeholder={mode === "todos" ? "Filter your open TODOs…" : "Recall across your history…"}
          onChange={(e) => {
            setQuery(e.currentTarget.value);
            commit(e.currentTarget.value);
          }}
        />
        <div className="flex items-center gap-1.5">
          {MODES.map((m) => (
            <Button
              key={m.id}
              size="xs"
              variant={mode === m.id ? "default" : "outline"}
              onClick={() => setMode(m.id)}
            >
              {m.label}
            </Button>
          ))}
          {count && <span className="ml-auto text-xs text-muted-foreground">{count}</span>}
        </div>
      </div>

      <div ref={parentRef} className="min-h-0 flex-1 overflow-y-auto">
        {loading ? (
          <Loading label={mode === "todos" ? "Loading…" : "Recalling…"} />
        ) : mode !== "todos" && !committed ? (
          <p className="px-1 text-sm text-muted-foreground">
            Type to recall {mode === "decision" ? "decisions" : "gotchas"} across every thread.
          </p>
        ) : items.length === 0 ? (
          <Empty mode={mode} filtering={committed.length > 0} />
        ) : (
          <div className="relative w-full" style={{ height: virtualizer.getTotalSize() }}>
            {virtualizer.getVirtualItems().map((vrow) => {
              const item = items[vrow.index];
              return (
                <div
                  key={vrow.key}
                  data-index={vrow.index}
                  ref={virtualizer.measureElement}
                  className="absolute left-0 top-0 w-full pb-2"
                  style={{ transform: `translateY(${vrow.start}px)` }}
                >
                  <Row
                    item={item}
                    onClick={() => open(item.threadId)}
                    onDone={mode === "todos" ? () => markDone.mutate(item.id) : undefined}
                  />
                </div>
              );
            })}
          </div>
        )}
      </div>
    </div>
  );
}

function Empty({ mode, filtering }: { mode: Mode; filtering: boolean }) {
  if (mode === "todos") {
    return filtering ? (
      <p className="px-1 text-sm text-muted-foreground">No TODOs match.</p>
    ) : (
      <div className="px-1">
        <h2 className="mb-1 text-base font-semibold">No open TODOs</h2>
        <p className="text-sm text-muted-foreground">
          Action items (markdown checkboxes + TODO/FIXME) show up here.
        </p>
      </div>
    );
  }
  return (
    <p className="px-1 text-sm text-muted-foreground">
      Nothing recalled. Distillation must be on and some threads distilled (open a thread →
      Distill).
    </p>
  );
}

function Row({
  item,
  onClick,
  onDone,
}: {
  item: RowItem;
  onClick: () => void;
  onDone?: () => void;
}) {
  return (
    <Item
      variant="outline"
      role="button"
      tabIndex={0}
      onClick={onClick}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          onClick();
        }
      }}
      className="cursor-pointer items-start gap-2.5 transition-colors hover:bg-muted/40 focus-visible:bg-muted/40"
    >
      <ItemContent className="min-w-0 gap-1">
        {/* overflow-wrap:anywhere so a long unbroken token (URL/path) can't force x-scroll */}
        <ItemTitle className="block w-full text-sm font-normal leading-snug wrap-anywhere">
          {item.text}
        </ItemTitle>
        <ItemDescription className="flex min-w-0 items-center gap-2 text-[0.72rem]">
          <Badge variant="outline" className="shrink-0 text-[0.6rem] uppercase">
            {SOURCE_LABELS[item.source]}
          </Badge>
          <span className="min-w-0 flex-1 truncate">
            {item.title || "Untitled thread"}
            {item.projectPath ? ` · ${shortPath(item.projectPath)}` : ""}
          </span>
        </ItemDescription>
      </ItemContent>
      <ItemActions className="shrink-0 gap-1 self-start">
        {item.meta && (
          <span className="text-xs tabular-nums text-muted-foreground">{item.meta}</span>
        )}
        {onDone && (
          <button
            type="button"
            title="Mark done"
            onClick={(e) => {
              e.stopPropagation();
              onDone();
            }}
            className="grid size-7 place-items-center rounded-md text-muted-foreground transition-colors hover:bg-emerald-500/15 hover:text-emerald-600 dark:hover:text-emerald-400"
          >
            <Check className="size-4" />
          </button>
        )}
      </ItemActions>
    </Item>
  );
}
