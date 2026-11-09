import { type ReactNode, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Menu as MenuPrimitive } from "@base-ui/react/menu";
import { ChevronDown, EyeOff, Pencil, Pin, Plus } from "lucide-react";
import {
  api,
  type LinkKind,
  type MemoryFact,
  type ProjectMemory,
  type RepoFactChange,
  type RepoPublishPlan,
} from "../../lib/api";
import { shortPath } from "../../lib/format";
import { useEmbedStatus, useIndexRewriting } from "../../lib/queries";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Spinner } from "@/components/ui/spinner";
import { Switch } from "@/components/ui/switch";
import { Progress, ProgressLabel, ProgressValue } from "@/components/ui/progress";
import { Item } from "@/components/ui/item";
import { cn } from "@/lib/utils";
import { Markdown } from "../Markdown";
import { MemorySetupCard } from "../MemorySetupCard";
import { ConflictReview } from "./ConflictReview";
import { DisputePanel, FactMeta, linkRefusal } from "./FactTruth";
import { FlaggedGroup, ReplacedGroup } from "./MemoryHistory";
import { SectionNote, SectionSkeleton } from "./ProjectSection";
import { useOpenThread } from "./shared";

/**
 * The Project page's Memory section: the decisions / gotchas / open TODOs distilled across
 * ALL of a project's threads, with a coverage chip + a background "Build memory" distill, an
 * LLM brief, and a "Write memory file" action that drops a `.callimachus/memory.md` agents
 * can read. "Publish to repo" writes the shareable facts to `.callimachus/memory/*.jsonl` in
 * the checkout, after a preview, for the user to commit; facts imported from those files
 * carry a "From the repo" badge, and the ones this machine published a "Shared" one.
 *
 * Each fact shows when it was decided (its session's date) and by which agent, and where it
 * stands: what it replaces, what disputes it, how many sessions confirmed it. Facts a newer
 * one replaced and facts flagged stale or wrong are left out of memory and listed apart
 * (Replaced, Flagged), where the user can undo the replacement or keep the fact.
 *
 * Never gated: TODOs, Remember and harvested Claude Code memory need no engine. While LLM
 * distillation is off the memory card sits above the list and the LLM-only actions (Build
 * memory, brief, conflicts) are hidden or disabled, since they would only fail.
 */
export function MemorySection({ project }: { project: string }) {
  const qc = useQueryClient();
  const openThread = useOpenThread();

  const knowledge = useQuery({ queryKey: ["knowledge_config"], queryFn: api.knowledgeConfig });
  // Unknown while loading: treat as on, so a distilled user never sees the card flash.
  const distillOn = knowledge.data?.enabled ?? true;

  const memory = useQuery({
    queryKey: ["project_memory", project],
    queryFn: () => api.projectMemory(project),
  });

  const distilling = useQuery({
    queryKey: ["distill_status"],
    queryFn: api.distillingStatus,
    refetchInterval: (q) => (q.state.data ? 1200 : false),
  });
  // Distill is mutually exclusive with the embedding build + reindex (they share the
  // write lock). Track them so the Build-memory button disables instead of no-op'ing.
  const embed = useEmbedStatus(); // event-driven; never polls while idle (lib/queries.ts)
  const indexing = useQuery({
    queryKey: ["index_status"],
    queryFn: api.indexingStatus,
    refetchInterval: (q) => (q.state.data ? 2000 : false),
  });
  const rewriting = useIndexRewriting(); // Shrink index / Reclaim space
  const progress = useQuery<{ done: number; total: number } | null>({
    queryKey: ["distill_progress"],
    queryFn: () => qc.getQueryData<{ done: number; total: number }>(["distill_progress"]) ?? null,
    staleTime: Number.POSITIVE_INFINITY,
  });

  const build = useMutation({
    mutationFn: () => api.distillProject(project),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["distill_status"] }),
  });
  const cancel = useMutation({
    mutationFn: api.cancelDistill,
    onSuccess: () => qc.invalidateQueries({ queryKey: ["distill_status"] }),
  });
  const brief = useMutation({ mutationFn: () => api.projectBrief(project) });
  // What agents see renders from the same facts and files, so every write here refreshes it
  // too: with both sections open, its panes would otherwise show the old facts, and after
  // Update AGENTS.md a hook that now stays silent would still show its injection.
  const refreshAgentView = () => qc.invalidateQueries({ queryKey: ["agent_view", project] });
  const writeFile = useMutation({
    mutationFn: () => api.writeProjectMemoryFile(project, true),
    onSuccess: () => refreshAgentView(),
  });
  const agentsFile = useMutation({
    mutationFn: () => api.writeAgentMemoryFile(project, "AGENTS.md"),
    onSuccess: () => refreshAgentView(),
  });
  const conflicts = useMutation({ mutationFn: () => api.detectConflicts(project) });
  const repo = useQuery({
    queryKey: ["repo_memory_status", project],
    queryFn: () => api.repoMemoryStatus(project),
    enabled: !!project,
  });
  const shared = new Set(repo.data?.sharedIds ?? []);
  // The project the publish preview is open for (null = closed), keyed like `remembering`.
  const [publishing, setPublishing] = useState<string | null>(null);
  const [published, setPublished] = useState<RepoPublishPlan | null>(null);
  // The project the inline "Remember" form is open for (null = closed). Keyed on the
  // project, so switching projects closes it instead of saving to the wrong one.
  const [remembering, setRemembering] = useState<string | null>(null);

  // Curation actions (pin / hide / edit). Each updates the cached memory optimistically
  // so the UI reacts instantly, rolls back + surfaces the error if the write fails, and
  // resyncs on settle. (Writes go through the app's single writer; a failure here means
  // a real backend error, which we now show instead of silently freezing.)
  const memKey = ["project_memory", project] as const;
  const refreshMem = () =>
    Promise.all([qc.invalidateQueries({ queryKey: memKey }), refreshAgentView()]);
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
            superseded: prev.superseded && map(prev.superseded),
            flagged: prev.flagged && map(prev.flagged),
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
  // Links and flags move facts between lists (a replaced one leaves Decisions for Replaced),
  // so these refetch instead of guessing the new lists.
  const settled = {
    onMutate: () => setCurationError(null),
    onError: (e: unknown) => setCurationError(`Couldn't save: ${e}`),
    onSettled: () => refreshMem(),
  };
  const link = useMutation({
    mutationFn: (v: { fromId: number; toId: number; kind: LinkKind }) =>
      api.linkFacts(v.fromId, v.toId, v.kind),
    ...settled,
    onSuccess: (o) => {
      const refused = linkRefusal(o);
      if (refused) setCurationError(refused);
    },
  });
  const unlink = useMutation({
    mutationFn: (v: { fromId: number; toId: number; kind: LinkKind }) =>
      api.unlinkFacts(v.fromId, v.toId, v.kind),
    ...settled,
  });
  const keepFlagged = useMutation({
    mutationFn: (id: number) => api.resolveFactFlags(id),
    ...settled,
  });
  // Locally remember hidden facts so the conflicts panel (a mutation result, not a
  // query) updates immediately: hiding a decision resolves any conflict it's in.
  const [hiddenIds, setHiddenIds] = useState<Set<number>>(() => new Set());
  const factActions: FactActions = {
    onOpen: (threadId) => openThread(threadId),
    onPin: (id, pinned) => pin.mutate({ id, pinned }),
    onHide: (id) => {
      setHiddenIds((prev) => new Set(prev).add(id));
      hideFact.mutate(id);
    },
    onEdit: (id, text) => editFact.mutate({ id, text }),
    onLink: (fromId, toId, kind) => link.mutate({ fromId, toId, kind }),
    onUnlink: (fromId, toId, kind) => unlink.mutate({ fromId, toId, kind }),
    isShared: (id) => shared.has(id),
  };
  // Review conflicts' "Newer replaces older": true once the link is stored, so the pair
  // leaves the list only then.
  const replace = async (newerId: number, olderId: number) => {
    try {
      const o = await link.mutateAsync({ fromId: newerId, toId: olderId, kind: "supersedes" });
      return o.outcome !== "rejected";
    } catch {
      return false;
    }
  };

  const mem = memory.data;
  const isDistilling = !!distilling.data;
  const pct =
    progress.data && progress.data.total > 0
      ? Math.round((progress.data.done / progress.data.total) * 100)
      : 0;
  const empty = mem && !mem.decisions.length && !mem.gotchas.length && !mem.openTodos.length;
  // Nothing live, but flagged or replaced facts are listed around the empty state: say so
  // instead of "nothing distilled yet" beside them.
  const historyOnly = empty && !!(mem.flagged?.length || mem.superseded?.length);
  // A reindex / embedding build / index rewrite is running → distill would no-op (shared
  // write lock).
  const otherBusy = embed.data?.running || indexing.data || rewriting;
  const otherBusyLabel = embed.data?.running
    ? "Embedding… (wait)"
    : indexing.data
      ? "Indexing… (wait)"
      : "Rewriting index… (wait)";

  // The More menu closes on click, so its trigger carries the pending state.
  const morePending = brief.isPending
    ? "Summarizing…"
    : conflicts.isPending
      ? "Reviewing…"
      : writeFile.isPending || agentsFile.isPending
        ? "Writing…"
        : null;

  return (
    <div className="space-y-4">
      <div className="space-y-3">
        {/* One primary action, one secondary, and the rest behind More, so the toolbar
            fits on one row at every width (six buttons wrapped at 1100 and 1440). */}
        <div className="flex flex-wrap items-center gap-2">
          {isDistilling ? (
            <Button size="sm" variant="secondary" onClick={() => cancel.mutate()}>
              Cancel distill
            </Button>
          ) : !distillOn ? null : (
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
            onClick={() => setRemembering(project)}
            disabled={!project || remembering === project}
            title="Record a decision or gotcha for this project yourself"
          >
            <Plus /> Remember
          </Button>
          <DropdownMenu>
            <DropdownMenuTrigger
              render={<Button size="sm" variant="outline" disabled={!project} />}
            >
              {morePending ? (
                <>
                  <Spinner /> {morePending}
                </>
              ) : (
                <>
                  More <ChevronDown className="size-3.5" />
                </>
              )}
            </DropdownMenuTrigger>
            <DropdownMenuContent align="start" className="min-w-48">
              <DropdownMenuItem
                onClick={() => brief.mutate()}
                disabled={!project || brief.isPending || !!empty || isDistilling || !distillOn}
              >
                Synthesize brief
              </DropdownMenuItem>
              <DropdownMenuItem
                onClick={() => conflicts.mutate()}
                disabled={
                  !project || conflicts.isPending || !mem || mem.decisions.length < 2 || !distillOn
                }
              >
                Review conflicts
              </DropdownMenuItem>
              <MenuPrimitive.Separator className="-mx-1 my-1 h-px bg-border" />
              <MenuPrimitive.Group>
                <DropdownMenuLabel>Export</DropdownMenuLabel>
                <DropdownMenuItem
                  onClick={() => writeFile.mutate()}
                  disabled={!project || writeFile.isPending || !!empty}
                >
                  Write memory file
                </DropdownMenuItem>
                <DropdownMenuItem
                  onClick={() => agentsFile.mutate()}
                  disabled={!project || agentsFile.isPending || !!empty}
                  title="Write/refresh the memory block in this repo's AGENTS.md"
                >
                  Update AGENTS.md
                </DropdownMenuItem>
                <DropdownMenuItem
                  onClick={() => {
                    setPublished(null);
                    setPublishing(project);
                  }}
                  disabled={!project || !repo.data?.root || publishing === project}
                  title={
                    repo.data && !repo.data.root
                      ? "This project has no git checkout on this machine to publish into"
                      : "Write this project's shareable facts to .callimachus/memory/ for you to commit"
                  }
                >
                  Publish to repo…
                </DropdownMenuItem>
              </MenuPrimitive.Group>
            </DropdownMenuContent>
          </DropdownMenu>
          {mem && (
            <Badge
              variant="outline"
              className="ml-auto shrink-0 tabular-nums"
              title="Sessions distilled into memory, of this project's sessions"
            >
              {mem.distilledCount}/{mem.threadCount} distilled
            </Badge>
          )}
        </div>

        {isDistilling && (
          <Progress value={pct} className="gap-1.5">
            <ProgressLabel className="text-xs font-normal text-muted-foreground">
              Distilling this project's threads…
            </ProgressLabel>
            <ProgressValue className="text-xs" />
          </Progress>
        )}
        {(writeFile.data || agentsFile.data) && (
          <p className="truncate text-xs text-muted-foreground">
            Wrote {writeFile.data || agentsFile.data}
          </p>
        )}
        {published && published.root === repo.data?.root && (
          <p className="text-xs text-muted-foreground">
            Published to <span className="font-mono">{shortPath(published.dir)}</span>. Commit it to
            share these facts with everyone who works in this repo.
          </p>
        )}
        {curationError && (
          <p role="alert" className="text-xs text-destructive">
            {curationError}
          </p>
        )}
      </div>

      <div>
        {memory.isPending ? (
          <SectionSkeleton rows={3} />
        ) : memory.isError ? (
          <SectionNote tone="error">Couldn't load the memory: {String(memory.error)}</SectionNote>
        ) : mem ? (
          <div className="space-y-5">
            {remembering === project && (
              <RememberForm project={project} onClose={() => setRemembering(null)} />
            )}
            {publishing === project && (
              <PublishPanel
                project={project}
                onClose={() => setPublishing(null)}
                onPublished={(plan) => {
                  setPublishing(null);
                  setPublished(plan);
                  void repo.refetch();
                }}
              />
            )}
            {!distillOn && <MemorySetupCard checklistLink />}
            {brief.data && (
              <div className="rounded-lg border p-3">
                <Markdown>{brief.data}</Markdown>
              </div>
            )}
            {conflicts.data && (
              <ConflictReview
                // A new review starts with nothing settled.
                key={conflicts.submittedAt}
                conflicts={conflicts.data}
                hiddenIds={hiddenIds}
                onHide={factActions.onHide}
                onReplace={replace}
              />
            )}
            <FlaggedGroup
              facts={mem.flagged ?? []}
              onKeep={(id) => keepFlagged.mutate(id)}
              onHide={factActions.onHide}
            />
            {empty ? (
              historyOnly ? (
                <p className="px-1 text-sm text-muted-foreground">
                  Nothing in memory right now. The facts listed here are flagged or replaced, so
                  agents don't see them.
                </p>
              ) : distillOn ? (
                <p className="px-1 text-sm text-muted-foreground">
                  Nothing distilled for this project yet. Click <b>Build memory</b> to distill its{" "}
                  {mem.pendingCount} {mem.pendingCount === 1 ? "session" : "sessions"} into
                  decisions, gotchas and TODOs.
                </p>
              ) : (
                <p className="px-1 text-sm text-muted-foreground">
                  No decisions, gotchas or open TODOs for this project yet. Click <b>Remember</b> to
                  record one yourself.
                </p>
              )
            ) : (
              <>
                <Section title="Decisions" facts={mem.decisions} actions={factActions} />
                <Section title="Gotchas" facts={mem.gotchas} actions={factActions} />
                <Section title="Open TODOs" facts={mem.openTodos} actions={factActions} />
              </>
            )}
            <ReplacedGroup
              facts={mem.superseded ?? []}
              onUndo={(newerId, olderId) =>
                unlink.mutate({ fromId: newerId, toId: olderId, kind: "supersedes" })
              }
            />
          </div>
        ) : null}
      </div>
    </div>
  );
}

/** Write-back: record a decision or gotcha for the project by hand. Saved pinned (the
 *  user's own words are trusted), so it ranks first and survives a re-distill. */
function RememberForm({ project, onClose }: { project: string; onClose: () => void }) {
  const qc = useQueryClient();
  const [kind, setKind] = useState<"decision" | "gotcha">("decision");
  const [text, setText] = useState("");
  const save = useMutation({
    mutationFn: () => api.remember(project, kind, text.trim()),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["project_memory", project] });
      void qc.invalidateQueries({ queryKey: ["agent_view", project] });
      onClose();
    },
  });
  const submit = () => {
    if (text.trim() && !save.isPending) save.mutate();
  };

  return (
    <Item variant="outline" className="items-stretch gap-2 ring-1 ring-ring/30">
      <div className="flex min-w-0 flex-1 flex-col gap-2">
        <div className="flex gap-1">
          {(["decision", "gotcha"] as const).map((k) => (
            <Button
              key={k}
              size="xs"
              variant={kind === k ? "secondary" : "ghost"}
              aria-pressed={kind === k}
              onClick={() => setKind(k)}
            >
              {k === "decision" ? "Decision" : "Gotcha"}
            </Button>
          ))}
        </div>
        <textarea
          // biome-ignore lint/a11y/noAutofocus: focus the field the user just opened
          autoFocus
          value={text}
          onChange={(e) => setText(e.currentTarget.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) submit();
            if (e.key === "Escape") onClose();
          }}
          placeholder={
            kind === "decision"
              ? "e.g. Store timestamps as UTC seconds, not ISO strings"
              : "e.g. The dev server has to run on port 1420 for Tauri to find it"
          }
          aria-label={kind === "decision" ? "Decision" : "Gotcha"}
          className="min-h-16 resize-y bg-transparent text-sm leading-relaxed outline-none placeholder:text-muted-foreground"
        />
        <p className="text-xs text-muted-foreground">
          Pinned to {shortPath(project)}'s memory, so recall and memory files include it.
        </p>
        {save.isError && (
          <p role="alert" className="text-xs text-destructive">
            Couldn't save: {String(save.error)}
          </p>
        )}
      </div>
      <div className="flex shrink-0 flex-col gap-1">
        <Button size="xs" onClick={submit} disabled={!text.trim() || save.isPending}>
          {save.isPending ? "Saving…" : "Save"}
        </Button>
        <Button size="xs" variant="ghost" onClick={onClose} disabled={save.isPending}>
          Cancel
        </Button>
      </div>
    </Item>
  );
}

/** "Publish to repo": a preview of what `.callimachus/memory/` would hold and what changes,
 *  with the same default as `cal memory publish` and its `--all` as a switch. Nothing is
 *  written until the user clicks Publish, and nothing is committed. */
function PublishPanel({
  project,
  onClose,
  onPublished,
}: {
  project: string;
  onClose: () => void;
  onPublished: (plan: RepoPublishPlan) => void;
}) {
  const [all, setAll] = useState(false);
  const preview = useQuery({
    queryKey: ["repo_memory_preview", project, all],
    queryFn: () => api.previewRepoMemory(project, all),
    staleTime: 0,
    gcTime: 0,
  });
  const publish = useMutation({
    mutationFn: () => api.publishRepoMemory(project, all),
    onSuccess: (plan) => onPublished(plan),
  });
  const plan = preview.data;
  const changes = plan ? plan.added.length + plan.updated.length + plan.removed.length : 0;

  return (
    <Item variant="outline" className="items-stretch gap-3 ring-1 ring-ring/30">
      <div className="flex min-w-0 flex-1 flex-col gap-2.5">
        <div>
          <p className="text-sm font-medium">Publish to repo</p>
          <p className="text-xs text-muted-foreground">
            Writes{" "}
            {plan ? (
              <span className="font-mono">{shortPath(plan.dir)}</span>
            ) : (
              ".callimachus/memory/"
            )}{" "}
            for you to review and commit. Teammates with Callimachus get these facts as notes from
            the repo. Nothing publishes on its own.
          </p>
        </div>
        <label className="flex items-center gap-2 text-xs text-muted-foreground">
          <Switch checked={all} onCheckedChange={(v) => setAll(!!v)} size="sm" />
          Include distilled decisions and gotchas (default: pinned facts and the ones you or an
          agent recorded)
        </label>
        {preview.isPending ? (
          <p className="flex items-center gap-2 text-xs text-muted-foreground">
            <Spinner /> Comparing with the files…
          </p>
        ) : preview.isError ? (
          <p role="alert" className="text-xs text-destructive">
            Couldn't preview: {String(preview.error)}
          </p>
        ) : plan ? (
          <div className="space-y-1.5 text-xs">
            <p className="tabular-nums text-muted-foreground">
              After publishing: {plan.decisions} {plan.decisions === 1 ? "decision" : "decisions"},{" "}
              {plan.gotchas} {plan.gotchas === 1 ? "gotcha" : "gotchas"}
              {plan.project && (
                <>
                  {" "}
                  for <span className="font-mono">{plan.project}</span>
                </>
              )}
            </p>
            {plan.changed ? (
              <>
                <p className="tabular-nums">
                  {plan.added.length} added, {plan.updated.length} updated, {plan.removed.length}{" "}
                  removed
                  {changes === 0 && " (the files only need sorting by uid)"}
                </p>
                <ChangeList sign="+" label="Added" changes={plan.added} />
                <ChangeList sign="~" label="Updated" changes={plan.updated} />
                <ChangeList sign="-" label="Removed" changes={plan.removed} />
              </>
            ) : (
              <p className="text-muted-foreground">
                The files already hold exactly this. Nothing to publish.
              </p>
            )}
          </div>
        ) : null}
        {publish.isError && (
          <p role="alert" className="text-xs text-destructive">
            Couldn't publish: {String(publish.error)}
          </p>
        )}
      </div>
      <div className="flex shrink-0 flex-col gap-1">
        <Button
          size="xs"
          onClick={() => publish.mutate()}
          disabled={!plan?.changed || publish.isPending || preview.isFetching}
        >
          {publish.isPending
            ? "Publishing…"
            : changes > 0
              ? `Publish ${changes} ${changes === 1 ? "change" : "changes"}`
              : "Publish"}
        </Button>
        <Button size="xs" variant="ghost" onClick={onClose} disabled={publish.isPending}>
          Cancel
        </Button>
      </div>
    </Item>
  );
}

/** Up to five of a publish's changes of one sort, then a count of the rest. */
function ChangeList({
  sign,
  label,
  changes,
}: {
  sign: string;
  label: string;
  changes: RepoFactChange[];
}) {
  if (!changes.length) return null;
  const shown = changes.slice(0, 5);
  return (
    <ul aria-label={label} className="space-y-0.5">
      {shown.map((c) => (
        <li key={c.uid} className="flex min-w-0 gap-2">
          <span aria-hidden className="w-3 shrink-0 font-mono text-muted-foreground">
            {sign}
          </span>
          <span className="shrink-0 text-muted-foreground">{c.kind}</span>
          <span className="truncate">{c.text}</span>
        </li>
      ))}
      {changes.length > shown.length && (
        <li className="pl-5 text-muted-foreground">+{changes.length - shown.length} more</li>
      )}
    </ul>
  );
}

interface FactActions {
  onOpen: (threadId: number) => void;
  onPin: (id: number, pinned: boolean) => void;
  onHide: (id: number) => void;
  onEdit: (id: number, text: string) => void;
  /** The user links two facts (`supersedes`: the first replaces the second). */
  onLink: (fromId: number, toId: number, kind: LinkKind) => void;
  /** The user undoes a link; a `contradicts` goes both ways. */
  onUnlink: (fromId: number, toId: number, kind: LinkKind) => void;
  /** Whether the repo's `.callimachus/memory/` files also hold this fact. */
  isShared: (id: number) => boolean;
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
      <div className="divide-y divide-border overflow-hidden rounded-xl border bg-card">
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
  // Harvested from Claude Code's auto memory (originRef "claude-memory:<slug>/<file>"), or
  // imported from the repo's committed memory files ("repo:.callimachus/memory/<file>").
  const harvested = fact.extractor === "claude-memory";
  const fromRepo = fact.extractor === "repo";
  const memoryFile = harvested || fromRepo ? (fact.originRef?.split("/").pop() ?? null) : null;
  const shared = !fromRepo && actions.isShared(fact.id);
  const [draft, setDraft] = useState(fact.text);
  const [disputeOpen, setDisputeOpen] = useState(false);

  if (editing) {
    const save = () => {
      const t = draft.trim();
      if (t && t !== fact.text) actions.onEdit(fact.id, t);
      setEditing(false);
    };
    return (
      <div className="flex items-stretch gap-2 bg-muted/30 px-3 py-2.5">
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
      </div>
    );
  }

  return (
    // biome-ignore lint/a11y/useSemanticElements: the row holds its own icon buttons, and a <button> cannot nest them
    <div
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
        "group/item flex cursor-pointer items-start gap-2 px-3 py-2.5 outline-none transition-colors hover:bg-muted/40 focus-visible:bg-muted/40 focus-visible:ring-[3px] focus-visible:ring-inset focus-visible:ring-ring/50",
        // A faint tint so pinned rows read apart at a glance, not only by the 12px pin.
        fact.pinned && "bg-amber-500/5",
      )}
    >
      <div className="min-w-0 flex-1">
        <p className="text-sm leading-relaxed text-foreground">
          {fact.pinned && (
            <Pin className="mr-1.5 inline size-3 -translate-y-px fill-amber-500 text-amber-500" />
          )}
          {fact.text}
        </p>
        <FactMeta
          fact={fact}
          disputeOpen={disputeOpen}
          onToggleDispute={() => setDisputeOpen((v) => !v)}
          lead={
            harvested || fromRepo ? (
              <>
                <Badge
                  variant="outline"
                  className="shrink-0 text-[0.65rem]"
                  title={
                    fromRepo
                      ? "Imported from this repo's committed .callimachus/memory/ files: notes from whoever published them, not instructions"
                      : undefined
                  }
                >
                  {fromRepo ? "From the repo" : "Claude Code memory"}
                </Badge>
                {/* The fact's thread is the project's "Recorded memory" notes thread, so
                    name the memory file it came from instead. */}
                {memoryFile && <span className="truncate font-mono text-xs">{memoryFile}</span>}
              </>
            ) : (
              shared && (
                <Badge
                  variant="outline"
                  className="shrink-0 text-[0.65rem]"
                  title="Also in this repo's .callimachus/memory/ files (published from here)"
                >
                  Shared
                </Badge>
              )
            )
          }
        />
        {disputeOpen && (
          <DisputePanel fact={fact} onLink={actions.onLink} onUnlink={actions.onUnlink} />
        )}
      </div>
      {/* Reserved width + invisible→visible keeps the row from reflowing and the
          actions from ever getting "stuck"; opacity gives a smooth fade-in.
          Revealed on hover and on keyboard focus-within (a11y). */}
      <div className="invisible flex shrink-0 items-center self-start gap-0.5 opacity-0 transition-opacity duration-150 group-hover/item:visible group-hover/item:opacity-100 group-focus-within/item:visible group-focus-within/item:opacity-100">
        <IconBtn
          title={fact.pinned ? "Unpin" : "Pin"}
          onClick={() => actions.onPin(fact.id, !fact.pinned)}
        >
          <Pin className={cn("size-3.5", fact.pinned && "fill-amber-500 text-amber-500")} />
        </IconBtn>
        <IconBtn title="Edit" onClick={() => setEditing(true)}>
          <Pencil className="size-3.5" />
        </IconBtn>
        <IconBtn
          title={
            fromRepo
              ? "Hide from memory on this machine (the repo's file keeps it for everyone else)"
              : "Hide from memory"
          }
          onClick={() => actions.onHide(fact.id)}
        >
          <EyeOff className="size-3.5" />
        </IconBtn>
      </div>
    </div>
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
      aria-label={title}
      // The row (Item) is clickable; stop the action click from opening the thread.
      onClick={(e) => {
        e.stopPropagation();
        onClick();
      }}
      className="grid size-7 place-items-center rounded-md text-muted-foreground outline-none transition-colors hover:bg-muted hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/50"
    >
      {children}
    </button>
  );
}
