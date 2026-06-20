import { type ReactNode, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Pencil, Pin, Trash2 } from "lucide-react";
import { api, type MemoryFact } from "../lib/api";
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

  // Curation actions (pin / hide / edit a fact) — all refresh the project's memory.
  const refreshMem = () => qc.invalidateQueries({ queryKey: ["project_memory", project] });
  const pin = useMutation({
    mutationFn: (v: { id: number; pinned: boolean }) => api.setFactPinned(v.id, v.pinned),
    onSuccess: refreshMem,
  });
  const hideFact = useMutation({
    mutationFn: (id: number) => api.hideFact(id, true),
    onSuccess: refreshMem,
  });
  const editFact = useMutation({
    mutationFn: (v: { id: number; text: string }) => api.editFact(v.id, v.text),
    onSuccess: refreshMem,
  });
  const factActions: FactActions = {
    onOpen: (threadId) => {
      selectThread(threadId);
      setView("search");
    },
    onPin: (id, pinned) => pin.mutate({ id, pinned }),
    onHide: (id) => hideFact.mutate(id),
    onEdit: (id, text) => editFact.mutate({ id, text }),
  };

  const mem = memory.data;
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
              (conflicts.data.length > 0 ? (
                <div className="space-y-3 rounded-lg border border-amber-500/50 bg-amber-500/5 p-3">
                  <div className="text-[0.7rem] font-medium uppercase tracking-wide text-amber-600 dark:text-amber-400">
                    Possible conflicts ({conflicts.data.length})
                  </div>
                  {conflicts.data.map((c) => (
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
    <div>
      <div className="mb-1.5 text-[0.7rem] font-medium uppercase tracking-wide text-muted-foreground">
        {title} ({facts.length})
      </div>
      <ul className="space-y-1.5">
        {facts.map((f) => (
          <FactRow key={f.id} fact={f} actions={actions} />
        ))}
      </ul>
    </div>
  );
}

/** One fact: click to open its thread; hover for pin / edit / delete; inline edit. */
function FactRow({ fact, actions }: { fact: MemoryFact; actions: FactActions }) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(fact.text);

  if (editing) {
    return (
      <li className="flex items-start gap-1 rounded-md border p-2">
        <textarea
          // biome-ignore lint/a11y/noAutofocus: focus the edit field the user just opened
          autoFocus
          value={draft}
          onChange={(e) => setDraft(e.currentTarget.value)}
          className="min-h-16 flex-1 resize-y rounded bg-transparent text-sm outline-none"
        />
        <div className="flex shrink-0 flex-col gap-1">
          <Button
            size="xs"
            onClick={() => {
              const t = draft.trim();
              if (t && t !== fact.text) actions.onEdit(fact.id, t);
              setEditing(false);
            }}
          >
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
      </li>
    );
  }

  return (
    <li className="group flex items-start gap-1">
      <button
        type="button"
        onClick={() => actions.onOpen(fact.threadId)}
        className="flex-1 cursor-pointer rounded-md border px-3 py-2 text-left text-sm transition-colors hover:bg-muted/50"
      >
        {fact.pinned && <Pin className="mr-1 inline size-3 fill-current text-amber-500" />}
        {fact.text}
        {fact.title && <span className="ml-1 text-xs text-muted-foreground">· {fact.title}</span>}
      </button>
      <div className="flex shrink-0 flex-col gap-0.5 opacity-0 transition-opacity group-hover:opacity-100">
        <IconBtn
          title={fact.pinned ? "Unpin" : "Pin"}
          onClick={() => actions.onPin(fact.id, !fact.pinned)}
        >
          <Pin className={`size-3.5 ${fact.pinned ? "fill-current text-amber-500" : ""}`} />
        </IconBtn>
        <IconBtn title="Edit" onClick={() => setEditing(true)}>
          <Pencil className="size-3.5" />
        </IconBtn>
        <IconBtn title="Delete" onClick={() => actions.onHide(fact.id)}>
          <Trash2 className="size-3.5" />
        </IconBtn>
      </div>
    </li>
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
      onClick={onClick}
      className="cursor-pointer rounded p-1 text-muted-foreground hover:bg-muted hover:text-foreground"
    >
      {children}
    </button>
  );
}
