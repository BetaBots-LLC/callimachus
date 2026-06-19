import { useRef, useState } from "react";
import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { useVirtualizer } from "@tanstack/react-virtual";
import { useDebouncedCallback } from "@tanstack/react-pacer";
import { api, SOURCE_LABELS, type SourceKind } from "../lib/api";
import { useUi } from "../store/ui";
import { shortPath } from "../lib/format";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
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
                  <Row item={item} onClick={() => open(item.threadId)} />
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
      Nothing recalled. Distillation must be on and some threads distilled (open a thread → Distill).
    </p>
  );
}

function Row({ item, onClick }: { item: RowItem; onClick: () => void }) {
  return (
    <button
      type="button"
      onClick={onClick}
      className="block w-full cursor-pointer rounded-lg border px-3 py-2.5 text-left transition-colors hover:bg-muted/50"
    >
      <div className="text-sm">{item.text}</div>
      <div className="mt-1.5 flex items-center gap-2 text-[0.72rem] text-muted-foreground">
        <Badge variant="outline" className="shrink-0 text-[0.6rem] uppercase">
          {SOURCE_LABELS[item.source]}
        </Badge>
        <span className="truncate">{item.title || "Untitled thread"}</span>
        {item.projectPath && <span className="shrink-0">· {shortPath(item.projectPath)}</span>}
        {item.meta && <span className="ml-auto shrink-0">{item.meta}</span>}
      </div>
    </button>
  );
}
