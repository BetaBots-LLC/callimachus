import { useState } from "react";
import { keepPreviousData, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useDebouncedCallback } from "@tanstack/react-pacer";
import { Check, ChevronRight } from "lucide-react";
import { api, type CoachFact, type ProjectInfo, SOURCE_LABELS, type TodoFact } from "../../lib/api";
import { AGGREGATE_STALE_MS } from "../../lib/queryClient";
import { invalidateDomain, MEMORY_WRITE_KEY } from "../../lib/keys";
import { baseName, shortPath } from "../../lib/format";
import { errorText } from "../../lib/toast";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { ErrorsSection } from "./ErrorsSection";
import { RowList, SectionNote, SectionSkeleton } from "./ProjectSection";
import { focusSuccessor, fullDate, relativeAge, useOpenThread } from "./shared";

/** Rows drawn before "Show all": the backend returns up to 500 open TODOs. */
const TODO_PAGE = 60;

/** "All projects": every repo with its counts (pick one to open it), the open TODOs across
 *  all of them (filterable and checkable), the decisions and gotchas distilled this week, and
 *  the errors that recur across repos. The last two are what the Coach tab showed. */
export function AllProjects({
  projects,
  onOpen,
}: {
  projects: ProjectInfo[] | undefined;
  /** Open a repo's page. The page's own handler, not a bare store write: it also has to
   *  leave All mode and remember the pick when the repo is the one shown before. */
  onOpen: (project: string) => void;
}) {
  return (
    <div className="space-y-6">
      <section aria-labelledby="all-projects-title">
        <h2 id="all-projects-title" className="mb-2 px-1 text-sm font-semibold">
          Projects
          {projects && (
            <span className="ml-2 font-normal tabular-nums text-muted-foreground">
              {projects.length}
            </span>
          )}
        </h2>
        <ProjectList projects={projects} onOpen={onOpen} />
      </section>
      <section aria-labelledby="all-todos-title">
        <h2 id="all-todos-title" className="mb-2 px-1 text-sm font-semibold">
          Open TODOs
        </h2>
        <TodoList />
      </section>
      <section aria-labelledby="all-week-title">
        <h2 id="all-week-title" className="mb-2 px-1 text-sm font-semibold">
          This week
          <span className="ml-2 font-normal text-muted-foreground">
            Decisions and gotchas distilled in the last 7 days
          </span>
        </h2>
        <ThisWeek />
      </section>
      <section aria-labelledby="all-errors-title">
        <h2 id="all-errors-title" className="mb-2 px-1 text-sm font-semibold">
          Recurring errors across repos
          <span className="ml-2 font-normal text-muted-foreground">Last 180 days</span>
        </h2>
        <ErrorsSection />
      </section>
    </div>
  );
}

function ProjectList({
  projects,
  onOpen,
}: {
  projects: ProjectInfo[] | undefined;
  onOpen: (project: string) => void;
}) {
  if (!projects) return <SectionSkeleton rows={4} />;
  if (projects.length === 0) {
    return (
      <SectionNote>
        No projects yet. A project appears once an indexed session ran inside a folder.
      </SectionNote>
    );
  }
  return (
    <RowList label="Projects">
      {projects.map((p) => (
        <li key={p.project}>
          <button
            type="button"
            onClick={() => onOpen(p.project)}
            className="flex w-full cursor-pointer items-center gap-3 px-3 py-2 text-left outline-none transition-colors hover:bg-muted/40 focus-visible:bg-muted/40 focus-visible:ring-[3px] focus-visible:ring-inset focus-visible:ring-ring/50"
          >
            <span className="min-w-0 flex-1">
              <span className="block truncate text-sm font-medium">{baseName(p.project)}</span>
              <span className="block truncate text-[13px] text-muted-foreground" title={p.project}>
                {shortPath(p.project)}
              </span>
            </span>
            <span className="hidden w-24 shrink-0 text-right text-[13px] tabular-nums text-muted-foreground @min-[34rem]/page:block">
              {p.threadCount.toLocaleString()} {p.threadCount === 1 ? "session" : "sessions"}
            </span>
            <span
              className="hidden w-24 shrink-0 text-right text-[13px] tabular-nums text-muted-foreground @min-[34rem]/page:block"
              title="Sessions distilled into memory"
            >
              {p.distilledCount.toLocaleString()} distilled
            </span>
            <span
              className="w-20 shrink-0 text-right text-[13px] tabular-nums text-muted-foreground"
              title={fullDate(p.lastActivity)}
            >
              {relativeAge(p.lastActivity)}
            </span>
            <ChevronRight aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
          </button>
        </li>
      ))}
    </RowList>
  );
}

function TodoList() {
  const qc = useQueryClient();
  const [query, setQuery] = useState("");
  const [committed, setCommitted] = useState("");
  const [showAll, setShowAll] = useState(false);
  const commit = useDebouncedCallback((v: string) => setCommitted(v.trim()), { wait: 250 });
  // Server-side text search, so it reaches past the page limit.
  const todos = useQuery({
    queryKey: ["open_todos", committed],
    queryFn: () => api.listOpenTodos(committed || undefined),
    placeholderData: keepPreviousData,
  });
  // Drop the TODO at once and roll back if the write fails, so the check feels instant.
  const markDone = useMutation({
    mutationKey: MEMORY_WRITE_KEY,
    mutationFn: (id: number) => api.setTodoDone(id, true),
    onMutate: async (id: number) => {
      const key = ["open_todos", committed];
      await qc.cancelQueries({ queryKey: ["open_todos"] });
      const prev = qc.getQueryData<TodoFact[]>(key);
      if (prev) {
        qc.setQueryData<TodoFact[]>(
          key,
          prev.filter((t) => t.id !== id),
        );
      }
      return { key, prev };
    },
    onError: (_e, _id, ctx) => {
      if (ctx?.prev) qc.setQueryData(ctx.key, ctx.prev);
    },
    // Every view of the memory, Today's "new TODOs" strip included.
    onSettled: () => invalidateDomain("memory"),
  });

  const rows = todos.data ?? [];
  const shown = showAll ? rows : rows.slice(0, TODO_PAGE);
  return (
    <div className="space-y-2">
      <div className="flex items-center gap-2">
        <Input
          value={query}
          onChange={(e) => {
            setQuery(e.currentTarget.value);
            commit(e.currentTarget.value);
          }}
          placeholder="Filter open TODOs"
          aria-label="Filter open TODOs"
          className="h-8 max-w-sm"
        />
        {todos.data && (
          <span className="text-xs tabular-nums text-muted-foreground">
            {committed ? `${rows.length} matching` : `${rows.length} open`}
          </span>
        )}
      </div>
      {markDone.isError && (
        <SectionNote tone="error">Couldn't mark it done: {errorText(markDone.error)}</SectionNote>
      )}
      {todos.isPending ? (
        <SectionSkeleton rows={3} />
      ) : todos.isError ? (
        <SectionNote tone="error">Couldn't load the TODOs: {errorText(todos.error)}</SectionNote>
      ) : rows.length === 0 ? (
        <SectionNote>
          {committed
            ? "No open TODO matches that."
            : "No open TODOs. Markdown checkboxes and TODO or FIXME lines from your sessions show up here."}
        </SectionNote>
      ) : (
        <>
          <RowList label="Open TODOs">
            {shown.map((t) => (
              <TodoRow
                key={t.id}
                todo={t}
                onDone={(row) => {
                  // The row leaves the list at once, with the focused check button in it: the
                  // next row's check takes the focus, so a run of TODOs closes key by key.
                  focusSuccessor(row, { row: "[data-todo-done]", fallback: "input" });
                  markDone.mutate(t.id);
                }}
              />
            ))}
          </RowList>
          {rows.length > shown.length && (
            <Button size="xs" variant="ghost" onClick={() => setShowAll(true)}>
              Show all {rows.length}
            </Button>
          )}
        </>
      )}
    </div>
  );
}

function TodoRow({ todo, onDone }: { todo: TodoFact; onDone: (row: Element | null) => void }) {
  const openThread = useOpenThread();
  return (
    <li className="flex items-start gap-1 px-1.5 py-1 transition-colors hover:bg-muted/40 has-[>button:focus-visible]:bg-muted/40">
      <button
        type="button"
        onClick={() => openThread(todo.threadId)}
        title="Open the session it came from"
        className="min-w-0 flex-1 cursor-pointer rounded-md px-1.5 py-1.5 text-left outline-none focus-visible:ring-[3px] focus-visible:ring-ring/50"
      >
        <span className="block text-sm leading-snug wrap-anywhere">{todo.text}</span>
        <span className="mt-0.5 block truncate text-[13px] text-muted-foreground">
          {[
            todo.projectPath ? baseName(todo.projectPath) : null,
            SOURCE_LABELS[todo.source],
            todo.title || "Untitled session",
            relativeAge(todo.createdAt),
          ]
            .filter(Boolean)
            .join(" · ")}
        </span>
      </button>
      <Button
        size="icon-sm"
        variant="ghost"
        title="Mark done"
        aria-label={`Mark done: ${todo.text}`}
        data-todo-done=""
        onClick={(e) => onDone(e.currentTarget)}
        className="mt-1 mr-0.5 hover:bg-emerald-500/15 hover:text-emerald-700 dark:hover:text-emerald-400"
      >
        <Check />
      </Button>
    </li>
  );
}

/** How many of the week's facts show before "Show all": the backend returns up to 40 of
 *  each kind. */
const WEEK_PAGE = 8;

/** The decisions and gotchas distilled in the last 7 days, across repos and newest first
 *  (`coach_overview`, the query Stats' heatmap shares). */
function ThisWeek() {
  const openThread = useOpenThread();
  const [showAll, setShowAll] = useState(false);
  const overview = useQuery({
    queryKey: ["coach_overview"],
    queryFn: api.coachOverview,
    staleTime: AGGREGATE_STALE_MS, // refreshed by index events (main.tsx)
  });
  if (overview.isPending) return <SectionSkeleton rows={2} />;
  if (overview.isError) {
    return (
      <SectionNote tone="error">
        Couldn't load this week's decisions and gotchas: {errorText(overview.error)}
      </SectionNote>
    );
  }
  const facts: (CoachFact & { kind: "Decision" | "Gotcha" })[] = [
    ...overview.data.decisions.map((f) => ({ ...f, kind: "Decision" as const })),
    ...overview.data.gotchas.map((f) => ({ ...f, kind: "Gotcha" as const })),
  ].sort((a, b) => b.createdAt - a.createdAt);
  if (facts.length === 0) {
    return (
      <SectionNote>
        No decisions or gotchas distilled in the last 7 days. They come from sessions the memory
        engine has read.
      </SectionNote>
    );
  }
  const shown = showAll ? facts : facts.slice(0, WEEK_PAGE);
  return (
    <div className="space-y-2">
      <RowList label="Decisions and gotchas this week">
        {shown.map((f) => (
          <li key={f.id}>
            <button
              type="button"
              onClick={() => openThread(f.threadId)}
              title="Open the session it came from"
              className="flex w-full cursor-pointer items-start gap-2.5 px-3 py-2 text-left outline-none transition-colors hover:bg-muted/40 focus-visible:bg-muted/40 focus-visible:ring-[3px] focus-visible:ring-inset focus-visible:ring-ring/50"
            >
              <Badge
                variant="outline"
                className="mt-0.5 w-16 shrink-0 justify-center text-[0.65rem]"
              >
                {f.kind}
              </Badge>
              <span className="min-w-0 flex-1">
                <span className="block text-sm leading-snug wrap-anywhere">{f.text}</span>
                <span className="mt-0.5 block truncate text-[13px] text-muted-foreground">
                  {[
                    f.project ? baseName(f.project) : null,
                    f.title || "Untitled session",
                    relativeAge(f.createdAt),
                  ]
                    .filter(Boolean)
                    .join(" · ")}
                </span>
              </span>
            </button>
          </li>
        ))}
      </RowList>
      {facts.length > shown.length && (
        <Button size="xs" variant="ghost" onClick={() => setShowAll(true)}>
          Show all {facts.length}
        </Button>
      )}
    </div>
  );
}
