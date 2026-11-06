import { useLayoutEffect, useRef, useState } from "react";
import { keepPreviousData, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useVirtualizer } from "@tanstack/react-virtual";
import { useDebouncedCallback } from "@tanstack/react-pacer";
import { Check } from "lucide-react";
import { api, SOURCE_LABELS, type SourceKind, type TodoFact } from "../lib/api";
import { useUi } from "../store/ui";
import { shortPath } from "../lib/format";
import {
  embedStatusQuery,
  useBuildSemanticIndex,
  useBuildStarting,
  useIndexRewriting,
} from "../lib/queries";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Item, ItemActions, ItemContent, ItemDescription, ItemTitle } from "@/components/ui/item";
import { Spinner } from "@/components/ui/spinner";
import { Loading } from "./Loading";
import { MemorySetupCard } from "./MemorySetupCard";

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
 * Not gated: TODOs need no engine, and recall still finds Remember notes and harvested
 * Claude Code memory while distillation is off, so the recall modes show the memory card
 * above their results instead of hiding them.
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
  const knowledge = useQuery({ queryKey: ["knowledge_config"], queryFn: api.knowledgeConfig });
  const distillOff = knowledge.data?.enabled === false;
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
  // The memory card scrolls with the results (in the sticky header it could fill a short
  // window and leave the list zero height), so the virtualizer offsets rows by its height.
  const showCard = mode !== "todos" && distillOff;
  const cardRef = useRef<HTMLDivElement>(null);
  const [cardHeight, setCardHeight] = useState(0);
  useLayoutEffect(() => {
    const el = cardRef.current;
    if (!showCard || !el) {
      setCardHeight(0);
      return;
    }
    const ro = new ResizeObserver(() => setCardHeight(el.offsetHeight));
    ro.observe(el);
    setCardHeight(el.offsetHeight);
    return () => ro.disconnect();
  }, [showCard]);
  const virtualizer = useVirtualizer({
    count: items.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => 70,
    overscan: 8,
    scrollMargin: cardHeight,
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
        {showCard && (
          // The card renders nothing once dismissed, so the gap applies only while it shows.
          <div ref={cardRef} className="has-[section]:pb-3">
            <MemorySetupCard />
          </div>
        )}
        {loading ? (
          <Loading label={mode === "todos" ? "Loading…" : "Recalling…"} />
        ) : mode !== "todos" && !committed ? (
          <p className="px-1 text-sm text-muted-foreground">
            Type to recall {mode === "decision" ? "decisions" : "gotchas"} across every thread.
          </p>
        ) : items.length === 0 ? (
          <Empty mode={mode} filtering={committed.length > 0} distillOff={distillOff} />
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
                  style={{
                    transform: `translateY(${vrow.start - virtualizer.options.scrollMargin}px)`,
                  }}
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

function Empty({
  mode,
  filtering,
  distillOff,
}: {
  mode: Mode;
  filtering: boolean;
  distillOff: boolean;
}) {
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
  return <RecallEmpty distillOff={distillOff} />;
}

/** No recall hits. Recall embeds the query, and with no embedding model on disk it returns
 *  nothing at all (and imported notes are never embedded), so "nothing recalled" would
 *  imply the notes do not exist. An index with nothing embedded stands in for "no model":
 *  building the semantic index is what downloads it. */
function RecallEmpty({ distillOff }: { distillOff: boolean }) {
  const embed = useQuery(embedStatusQuery);
  const build = useBuildSemanticIndex();
  const starting = useBuildStarting();
  const rewriting = useIndexRewriting();
  const running = embed.data?.running === true;
  if (embed.data && (embed.data.done === 0 || running)) {
    return (
      <div className="flex flex-wrap items-center gap-x-3 gap-y-2 px-1">
        <p className="text-sm text-muted-foreground">
          Recall searches by meaning, so it needs the semantic index. Build it, then search again.
        </p>
        <Button
          size="sm"
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
  // The memory card above already explains what turning distillation on adds.
  return (
    <p className="px-1 text-sm text-muted-foreground">
      {distillOff
        ? "Nothing recalled from your Remember notes or Claude Code memory."
        : "Nothing recalled. Distill more threads (open a thread, then Distill) or build a project's memory in Projects."}
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
