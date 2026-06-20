import { type ReactNode, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Pencil, Pin, Trash2 } from "lucide-react";
import { api, type MemoryFact, type ProjectMemory } from "../lib/api";
import { useUi } from "../store/ui";
import { shortPath } from "../lib/format";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Progress, ProgressLabel, ProgressValue } from "@/components/ui/progress";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Item, ItemActions, ItemContent, ItemDescription, ItemTitle } from "@/components/ui/item";
import { cn } from "@/lib/utils";
import { Loading } from "./Loading";
import { Markdown } from "./Markdown";

/**
 * Project Memory: the decisions / gotchas / open TODOs distilled across ALL of a project's
 * threads, with a coverage chip + a background "Build memory" distill, an LLM brief, and a
 * "Write memory file" action that drops a `.callimachus/memory.md` agents can read.
 */
export function ProjectMemoryView() {
  const qc = useQueryClient();
  const selectThread = useUi((s) => s.selectThread);
  const setView = useUi((s) => s.setView);
  const selectedProject = useUi((s) => s.selectedProject);
  const openProject = useUi((s) => s.openProject);

  const projects = useQuery({ queryKey: ["projects"], queryFn: api.listProjects });
  const project = selectedProject ?? projects.data?.[0]?.project ?? null;

  const memory = useQuery({
    queryKey: ["project_memory", project],
    queryFn: () => api.projectMemory(project as string),
    enabled: !!project,
  });

  const distilling = useQuery({
    queryKey: ["distill_status"],
    queryFn: api.distillingStatus,
    refetchInterval: (q) => (q.state.data ? 1200 : false),
  });
  // Distill is mutually exclusive with the embedding build + reindex (they share the
  // write lock). Track them so the Build-memory button disables instead of no-op'ing.
  const embed = useQuery({
    queryKey: ["embed_status"],
    queryFn: api.embeddingStatus,
    refetchInterval: (q) => (q.state.data?.running ? 3000 : false),
  });
  const indexing = useQuery({
    queryKey: ["index_status"],
    queryFn: api.indexingStatus,
    refetchInterval: (q) => (q.state.data ? 2000 : false),
  });
  const progress = useQuery<{ done: number; total: number } | null>({
    queryKey: ["distill_progress"],
    queryFn: () => qc.getQueryData<{ done: number; total: number }>(["distill_progress"]) ?? null,
    staleTime: Number.POSITIVE_INFINITY,
  });

  const build = useMutation({
    mutationFn: () => api.distillProject(project as string),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["distill_status"] }),
  });
  const cancel = useMutation({
    mutationFn: api.cancelDistill,
    onSuccess: () => qc.invalidateQueries({ queryKey: ["distill_status"] }),
  });
  const brief = useMutation({ mutationFn: () => api.projectBrief(project as string) });
  const writeFile = useMutation({
    mutationFn: () => api.writeProjectMemoryFile(project as string, true),
  });
  const conflicts = useMutation({ mutationFn: () => api.detectConflicts(project as string) });

  // Curation actions (pin / hide / edit). Each updates the cached memory optimistically
  // so the UI reacts instantly, rolls back + surfaces the error if the write fails, and
  // resyncs on settle. (Writes go through the app's single writer; a failure here means
  // a real backend error, which we now show instead of silently freezing.)
  const memKey = ["project_memory", project] as const;
  const refreshMem = () => qc.invalidateQueries({ queryKey: memKey });
  const [curationError, setCurationError] = useState<string | null>(null);

  function optimistic<V>(mapFor: (v: V) => (facts: MemoryFact[]) => MemoryFact[]) {
    return {
      onMutate: async (v: V): Promise<{ prev?: ProjectMemory }> => {
        setCurationError(null);
        await qc.cancelQueries({ queryKey: memKey });
        const prev = qc.getQueryData<ProjectMemory>(memKey);
        if (prev) {
          const map = mapFor(v);
          qc.setQueryData<ProjectMemory>(memKey, {
            ...prev,
            decisions: map(prev.decisions),
            gotchas: map(prev.gotchas),
            openTodos: map(prev.openTodos),
          });
        }
        return { prev };
      },
      onError: (e: unknown, _v: V, ctx: { prev?: ProjectMemory } | undefined) => {
        if (ctx?.prev) qc.setQueryData(memKey, ctx.prev);
        setCurationError(`Couldn't save: ${e}`);
      },
      onSettled: () => refreshMem(),
    };
  }

  const pin = useMutation({
    mutationFn: (v: { id: number; pinned: boolean }) => api.setFactPinned(v.id, v.pinned),
    ...optimistic(
      (v: { id: number; pinned: boolean }) => (fs) =>
        fs.map((f) => (f.id === v.id ? { ...f, pinned: v.pinned } : f)),
    ),
  });
  const hideFact = useMutation({
    mutationFn: (id: number) => api.hideFact(id, true),
    ...optimistic((id: number) => (fs) => fs.filter((f) => f.id !== id)),
  });
  const editFact = useMutation({
    mutationFn: (v: { id: number; text: string }) => api.editFact(v.id, v.text),
    ...optimistic(
      (v: { id: number; text: string }) => (fs) =>
        fs.map((f) => (f.id === v.id ? { ...f, text: v.text } : f)),
    ),
  });
  // Locally remember hidden facts so the conflicts panel (a mutation result, not a
  // query) updates immediately — hiding a decision resolves any conflict it's in.
  const [hiddenIds, setHiddenIds] = useState<Set<number>>(() => new Set());
  const factActions: FactActions = {
    onOpen: (threadId) => {
      selectThread(threadId);
      setView("search");
    },
    onPin: (id, pinned) => pin.mutate({ id, pinned }),
    onHide: (id) => {
      setHiddenIds((prev) => new Set(prev).add(id));
      hideFact.mutate(id);
    },
    onEdit: (id, text) => editFact.mutate({ id, text }),
  };

  const mem = memory.data;
  // A conflict is resolved once either of its two decisions is hidden.
  const visibleConflicts = (conflicts.data ?? []).filter(
    (c) => !hiddenIds.has(c.aId) && !hiddenIds.has(c.bId),
  );
  const isDistilling = !!distilling.data;
  const pct =
    progress.data && progress.data.total > 0
      ? Math.round((progress.data.done / progress.data.total) * 100)
      : 0;
  const empty = mem && !mem.decisions.length && !mem.gotchas.length && !mem.openTodos.length;
  // A reindex / embedding build is running → distill would no-op (shared write lock).
  const otherBusy = embed.data?.running || indexing.data;
  const otherBusyLabel = embed.data?.running ? "Embedding… (wait)" : "Indexing… (wait)";

  return (
    <div className="mx-auto flex h-full w-full max-w-3xl flex-col p-6">
      <div className="shrink-0 space-y-3 pb-3">
        <div className="flex items-center gap-2">
          <Select value={project ?? ""} onValueChange={(v) => v && openProject(v)}>
            <SelectTrigger size="sm" className="min-w-0 flex-1">
              <SelectValue placeholder="Pick a project" />
            </SelectTrigger>
            <SelectContent>
              {projects.data?.map((p) => (
                <SelectItem key={p.project} value={p.project}>
                  {shortPath(p.project)} · {p.distilledCount}/{p.threadCount}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          {mem && (
            <Badge variant="outline" className="shrink-0">
              {mem.distilledCount}/{mem.threadCount} distilled
            </Badge>
          )}
        </div>

        <div className="flex flex-wrap items-center gap-2">
          {isDistilling ? (
            <Button size="sm" variant="secondary" onClick={() => cancel.mutate()}>
              Cancel distill
            </Button>
          ) : (
            <Button
              size="sm"
              onClick={() => build.mutate()}
              disabled={!project || !mem || mem.pendingCount === 0 || otherBusy}
            >
              {otherBusy
                ? otherBusyLabel
                : mem && mem.pendingCount > 0
                  ? `Build memory (${mem.pendingCount} to distill)`
                  : "Memory up to date"}
            </Button>
          )}
          <Button
            size="sm"
            variant="outline"
            onClick={() => brief.mutate()}
            disabled={!project || brief.isPending || empty || isDistilling}
          >
            {brief.isPending ? "Summarizing…" : "Synthesize brief"}
          </Button>
          <Button
            size="sm"
            variant="ghost"
            onClick={() => writeFile.mutate()}
            disabled={!project || writeFile.isPending || empty}
          >
            {writeFile.isPending ? "Writing…" : "Write memory file"}
          </Button>
          <Button
            size="sm"
            variant="ghost"
            onClick={() => conflicts.mutate()}
            disabled={!project || conflicts.isPending || !mem || mem.decisions.length < 2}
          >
            {conflicts.isPending ? "Reviewing…" : "Review conflicts"}
          </Button>
        </div>

        {isDistilling && (
          <Progress value={pct} className="gap-1.5">
            <ProgressLabel className="text-xs font-normal text-muted-foreground">
              Distilling this project's threads…
            </ProgressLabel>
            <ProgressValue className="text-xs" />
          </Progress>
        )}
        {writeFile.data && (
          <p className="truncate text-xs text-muted-foreground">Wrote {writeFile.data}</p>
        )}
        {curationError && <p className="text-xs text-destructive">{curationError}</p>}
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto">
        {!project ? (
          <p className="px-1 text-sm text-muted-foreground">No projects indexed yet.</p>
        ) : memory.isLoading ? (
          <Loading label="Loading memory…" />
        ) : mem ? (
          <div className="space-y-5">
            {brief.data && (
              <div className="rounded-lg border p-3">
                <Markdown>{brief.data}</Markdown>
              </div>
            )}
            {conflicts.data &&
              (visibleConflicts.length > 0 ? (
                <div className="space-y-3 rounded-lg border border-amber-500/50 bg-amber-500/5 p-3">
                  <div className="text-[0.7rem] font-medium uppercase tracking-wide text-amber-600 dark:text-amber-400">
                    Possible conflicts ({visibleConflicts.length})
                  </div>
                  {visibleConflicts.map((c) => (
                    <div key={`${c.aId}-${c.bId}`} className="space-y-1.5 text-sm">
                      <p className="text-muted-foreground">{c.reason}</p>
                      {[
                        { id: c.aId, text: c.aText },
                        { id: c.bId, text: c.bText },
                      ].map((d) => (
                        <div
                          key={d.id}
                          className="flex items-start gap-2 rounded-md border px-2 py-1 text-xs"
                        >
                          <span className="flex-1">{d.text}</span>
                          <button
                            type="button"
                            onClick={() => factActions.onHide(d.id)}
                            className="shrink-0 cursor-pointer text-muted-foreground hover:text-destructive"
                          >
                            hide
                          </button>
                        </div>
                      ))}
                    </div>
                  ))}
                </div>
              ) : (
                <p className="px-1 text-xs text-muted-foreground">
                  No conflicting decisions found.
                </p>
              ))}
            {empty ? (
              <p className="px-1 text-sm text-muted-foreground">
                Nothing distilled for this project yet. Click <b>Build memory</b> to distill its{" "}
                {mem?.pendingCount ?? 0} thread(s) into decisions, gotchas, and TODOs. Needs
                distillation enabled in Settings.
              </p>
            ) : (
              <>
                <Section title="Decisions" facts={mem.decisions} actions={factActions} />
                <Section title="Gotchas" facts={mem.gotchas} actions={factActions} />
                <Section title="Open TODOs" facts={mem.openTodos} actions={factActions} />
              </>
            )}
          </div>
        ) : null}
      </div>
    </div>
  );
}

interface FactActions {
  onOpen: (threadId: number) => void;
  onPin: (id: number, pinned: boolean) => void;
  onHide: (id: number) => void;
  onEdit: (id: number, text: string) => void;
}

function Section({
  title,
  facts,
  actions,
}: {
  title: string;
  facts: MemoryFact[];
  actions: FactActions;
}) {
  if (!facts.length) return null;
  return (
    <section>
      <h3 className="mb-2 flex items-baseline gap-2 px-1 text-[0.7rem] font-medium uppercase tracking-wider text-muted-foreground">
        {title}
        <span className="font-mono text-[0.65rem] tabular-nums text-muted-foreground/60">
          {facts.length}
        </span>
      </h3>
      <div className="flex flex-col gap-1.5">
        {facts.map((f) => (
          <FactRow key={f.id} fact={f} actions={actions} />
        ))}
      </div>
    </section>
  );
}

/** One fact: click to open its thread; hover reveals pin / edit / delete; inline edit. */
function FactRow({ fact, actions }: { fact: MemoryFact; actions: FactActions }) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(fact.text);

  if (editing) {
    const save = () => {
      const t = draft.trim();
      if (t && t !== fact.text) actions.onEdit(fact.id, t);
      setEditing(false);
    };
    return (
      <Item variant="outline" className="items-stretch gap-2 ring-1 ring-ring/30">
        <textarea
          // biome-ignore lint/a11y/noAutofocus: focus the edit field the user just opened
          autoFocus
          value={draft}
          onChange={(e) => setDraft(e.currentTarget.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) save();
            if (e.key === "Escape") {
              setDraft(fact.text);
              setEditing(false);
            }
          }}
          className="min-h-16 flex-1 resize-y bg-transparent text-sm leading-relaxed outline-none placeholder:text-muted-foreground"
        />
        <div className="flex shrink-0 flex-col gap-1">
          <Button size="xs" onClick={save}>
            Save
          </Button>
          <Button
            size="xs"
            variant="ghost"
            onClick={() => {
              setDraft(fact.text);
              setEditing(false);
            }}
          >
            Cancel
          </Button>
        </div>
      </Item>
    );
  }

  return (
    <Item
      variant="outline"
      role="button"
      tabIndex={0}
      onClick={() => actions.onOpen(fact.threadId)}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          actions.onOpen(fact.threadId);
        }
      }}
      className={cn(
        "cursor-pointer items-start transition-colors hover:bg-muted/40 focus-visible:bg-muted/40",
        fact.pinned && "border-amber-500/25 bg-amber-500/4",
      )}
    >
      <ItemContent>
        <ItemTitle className="block w-full font-normal leading-relaxed line-clamp-none">
          {fact.pinned && (
            <Pin className="mr-1.5 inline size-3 -translate-y-px fill-amber-500 text-amber-500" />
          )}
          {fact.text}
        </ItemTitle>
        {fact.title && <ItemDescription className="line-clamp-1">{fact.title}</ItemDescription>}
      </ItemContent>
      {/* Reserved width + invisible→visible keeps the row from reflowing and the
          actions from ever getting "stuck"; opacity gives a smooth fade-in.
          Revealed on hover and on keyboard focus-within (a11y). */}
      <ItemActions className="invisible self-start gap-0.5 opacity-0 transition-opacity duration-150 group-hover/item:visible group-hover/item:opacity-100 group-focus-within/item:visible group-focus-within/item:opacity-100">
        <IconBtn
          title={fact.pinned ? "Unpin" : "Pin"}
          onClick={() => actions.onPin(fact.id, !fact.pinned)}
        >
          <Pin className={cn("size-3.5", fact.pinned && "fill-amber-500 text-amber-500")} />
        </IconBtn>
        <IconBtn title="Edit" onClick={() => setEditing(true)}>
          <Pencil className="size-3.5" />
        </IconBtn>
        <IconBtn title="Delete" onClick={() => actions.onHide(fact.id)}>
          <Trash2 className="size-3.5" />
        </IconBtn>
      </ItemActions>
    </Item>
  );
}

function IconBtn({
  title,
  onClick,
  children,
}: {
  title: string;
  onClick: () => void;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      title={title}
      // The row (Item) is clickable; stop the action click from opening the thread.
      onClick={(e) => {
        e.stopPropagation();
        onClick();
      }}
      className="grid size-7 place-items-center rounded-md text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
    >
      {children}
    </button>
  );
}
