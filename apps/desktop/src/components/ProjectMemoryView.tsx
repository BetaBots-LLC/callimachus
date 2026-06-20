import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
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

  const mem = memory.data;
  const isDistilling = !!distilling.data;
  const pct =
    progress.data && progress.data.total > 0
      ? Math.round((progress.data.done / progress.data.total) * 100)
      : 0;
  const empty = mem && !mem.decisions.length && !mem.gotchas.length && !mem.openTodos.length;
  const openInSearch = (threadId: number) => {
    selectThread(threadId);
    setView("search");
  };

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
              disabled={!project || !mem || mem.pendingCount === 0}
            >
              {mem && mem.pendingCount > 0
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
            {empty ? (
              <p className="px-1 text-sm text-muted-foreground">
                Nothing distilled for this project yet. Click <b>Build memory</b> to distill its{" "}
                {mem?.pendingCount ?? 0} thread(s) into decisions, gotchas, and TODOs. Needs
                distillation enabled in Settings.
              </p>
            ) : (
              <>
                <Section title="Decisions" facts={mem.decisions} onOpen={openInSearch} />
                <Section title="Gotchas" facts={mem.gotchas} onOpen={openInSearch} />
                <Section title="Open TODOs" facts={mem.openTodos} onOpen={openInSearch} />
              </>
            )}
          </div>
        ) : null}
      </div>
    </div>
  );
}

function Section({
  title,
  facts,
  onOpen,
}: {
  title: string;
  facts: MemoryFact[];
  onOpen: (threadId: number) => void;
}) {
  if (!facts.length) return null;
  return (
    <div>
      <div className="mb-1.5 text-[0.7rem] font-medium uppercase tracking-wide text-muted-foreground">
        {title} ({facts.length})
      </div>
      <ul className="space-y-1.5">
        {facts.map((f) => (
          <li key={f.id}>
            <button
              type="button"
              onClick={() => onOpen(f.threadId)}
              className="block w-full cursor-pointer rounded-md border px-3 py-2 text-left text-sm transition-colors hover:bg-muted/50"
            >
              {f.text}
              {f.title && <span className="ml-1 text-xs text-muted-foreground">· {f.title}</span>}
            </button>
          </li>
        ))}
      </ul>
    </div>
  );
}
