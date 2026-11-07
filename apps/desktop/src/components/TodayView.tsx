import { type KeyboardEvent, useEffect, useId, useState } from "react";
import { keepPreviousData, useMutation, useQuery } from "@tanstack/react-query";
import { AlertTriangle, Check, CircleDot, Copy, History, TerminalSquare } from "lucide-react";
import { api, SOURCE_LABELS, type Today, type TodayThread } from "../lib/api";
import { copyText } from "../lib/clipboard";
import {
  baseName,
  formatDuration,
  formatSince,
  formatTime,
  snapshotEventText,
} from "../lib/format";
import { CAN_LAUNCH_CLI_FALLBACK } from "../lib/platform";
import { AGGREGATE_STALE_MS, queryClient } from "../lib/queryClient";
import { errorText, notify } from "../lib/toast";
import { useUi } from "../store/ui";
import { openProjectSection } from "./project/sections";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { Loading } from "./Loading";

/**
 * Today: what happened since the app was last opened, grouped by repo, with the commits
 * each session produced and what came up along the way (new TODOs, errors seen again,
 * snapshots). Built from the index alone; nothing here calls an LLM.
 */

type Range = "open" | "24h" | "7d";
const RANGES: { id: Range; label: string; secs: number }[] = [
  { id: "open", label: "Since last open", secs: 0 },
  { id: "24h", label: "24 hours", secs: 86_400 },
  { id: "7d", label: "7 days", secs: 7 * 86_400 },
];

// The window "since last open" starts where the first load of the session said it does.
// The app marks itself opened right after that load, so asking for `today(null)` again
// (a refetch on revisit) would start from that mark and come back empty.
let lastOpenSince: number | null = null;
let markedOpen = false;

// With close-to-tray the webview lives for days, so "once per process" is not "once per
// open". Hiding the window after Today was seen marks the open (the next cold start counts
// from there), and showing it again after more than an hour starts a new window: the next
// load asks for `today(null)`, which now starts at that mark, and marks again.
const NEW_OPEN_AFTER_MS = 60 * 60_000;
let hiddenAt: number | null = null;
if (typeof document !== "undefined") {
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "hidden") {
      hiddenAt = Date.now();
      if (markedOpen) api.markAppOpened().catch(() => {});
      return;
    }
    const away = hiddenAt === null ? 0 : Date.now() - hiddenAt;
    hiddenAt = null;
    if (away <= NEW_OPEN_AFTER_MS) return;
    lastOpenSince = null;
    markedOpen = false;
    void queryClient.invalidateQueries({ queryKey: ["today", "open"] });
  });
}
// The range picked this session, kept across tab switches (the view unmounts).
let sessionRange: Range = "open";

type Panel = "todos" | "issues" | "snapshots";

export function TodayView() {
  const [range, setRangeState] = useState<Range>(sessionRange);
  const setRange = (r: Range) => {
    sessionRange = r;
    setRangeState(r);
  };
  const report = useQuery({
    queryKey: ["today", range],
    queryFn: async () => {
      const secs = RANGES.find((r) => r.id === range)?.secs ?? 0;
      const since = range === "open" ? lastOpenSince : Math.floor(Date.now() / 1000) - secs;
      const data = await api.today(since);
      if (range === "open" && lastOpenSince === null) lastOpenSince = data.since;
      return data;
    },
    // Watcher writes mark it stale (AGGREGATE_KEYS); a revisit then refreshes it.
    staleTime: AGGREGATE_STALE_MS,
    placeholderData: keepPreviousData,
  });

  // The first successful load of an open moves "last open" to now, after the window on
  // screen has been read, so the next launch starts where this one did. Keyed on the data
  // too: a new open (the window shown again after an hour) refetches while still a success.
  // biome-ignore lint/correctness/useExhaustiveDependencies: dataUpdatedAt is the trigger (a refetch after a new open), read through the module flag
  useEffect(() => {
    if (!report.isSuccess || markedOpen) return;
    markedOpen = true;
    api.markAppOpened().catch(() => {}); // an older backend without it just rejects
  }, [report.isSuccess, report.dataUpdatedAt]);

  const [openPanel, setPanel] = useState<Panel | null>(null);
  const panelId = useId();

  if (report.isPending) return <Loading label="Loading today…" className="h-full" />;
  if (report.isError) {
    return (
      <div role="alert" className="mx-auto w-full max-w-3xl space-y-3 p-6 text-sm">
        <p className="text-destructive">
          Couldn't load today's activity: {errorText(report.error)}
        </p>
        <Button size="sm" variant="outline" onClick={() => void report.refetch()}>
          Try again
        </Button>
      </div>
    );
  }

  const data = report.data;
  const sessions = data.groups.reduce((n, g) => n + g.threads.length, 0);
  // gitlink can link one commit to two sessions, so commits count once by sha.
  const commits = new Set(
    data.groups.flatMap((g) => g.threads.flatMap((t) => t.commits)).map((c) => c.sha),
  ).size;
  // A panel whose kind dropped to nothing (another range, a refetch) closes, since its
  // strip button, the only control that closes it, is gone too.
  const panel = openPanel && panelCount(data, openPanel) > 0 ? openPanel : null;
  const since = formatSince(data.since);
  const nothing =
    sessions === 0 &&
    data.newTodos.length === 0 &&
    data.issuesSeenAgain.length === 0 &&
    data.snapshots.length === 0;

  return (
    // A block scroller, not a flex column: flex children with overflow-hidden shrink to
    // nothing once the page is taller than the window.
    <div className="h-full overflow-y-auto" aria-busy={report.isFetching}>
      <div
        className={cn(
          "mx-auto w-full max-w-3xl p-6 transition-opacity",
          report.isPlaceholderData && "opacity-60",
        )}
      >
        <header className="flex flex-wrap items-center gap-x-4 gap-y-3 pb-5">
          <div className="min-w-0 flex-1">
            <h1 className="text-lg font-semibold tracking-tight text-balance">Since {since}</h1>
            {!nothing && (
              <p className="text-sm text-muted-foreground tabular-nums">
                {plural(sessions, "session")} in {plural(data.groups.length, "repo")}
                {commits > 0 && ` · ${plural(commits, "commit")}`}
                {data.truncated && ` · showing the newest ${sessions.toLocaleString()}`}
              </p>
            )}
          </div>
          <RangePicker range={range} onChange={setRange} />
          <CopyStandup report={data} disabled={nothing} />
        </header>

        {nothing ? (
          <EmptyToday since={since} />
        ) : (
          <>
            <Strip report={data} panel={panel} onToggle={setPanel} panelId={panelId} />
            {panel && <StripPanel id={panelId} panel={panel} report={data} />}
            {sessions === 0 ? (
              <p className="text-sm text-muted-foreground">No sessions since {since}.</p>
            ) : (
              <SessionGroups report={data} />
            )}
            {data.commitsPartial && (
              <p className="pt-4 text-xs text-muted-foreground">
                Some repos show no commits: reading their git history took longer than 2 seconds.
              </p>
            )}
          </>
        )}
      </div>
    </div>
  );
}

function panelCount(report: Today, panel: Panel): number {
  if (panel === "todos") return report.newTodos.length;
  if (panel === "issues") return report.issuesSeenAgain.length;
  return report.snapshots.length;
}

function plural(n: number, word: string): string {
  return `${n.toLocaleString()} ${word}${n === 1 ? "" : "s"}`;
}

function RangePicker({ range, onChange }: { range: Range; onChange: (r: Range) => void }) {
  return (
    <fieldset
      aria-label="Time window"
      className="m-0 flex h-8 min-w-0 shrink-0 items-center gap-0.5 rounded-lg border p-[3px]"
    >
      {RANGES.map((r) => (
        <Button
          key={r.id}
          size="xs"
          variant={range === r.id ? "secondary" : "ghost"}
          aria-pressed={range === r.id}
          className={cn(range !== r.id && "text-muted-foreground")}
          onClick={() => onChange(r.id)}
        >
          {r.label}
        </Button>
      ))}
    </fieldset>
  );
}

/** Plain markdown for a standup note: each repo, its sessions and commits, then the new
 *  TODOs. Copied straight from the click, since WebKit allows clipboard writes only
 *  during the gesture. */
function CopyStandup({ report, disabled }: { report: Today; disabled: boolean }) {
  const [state, setState] = useState<"idle" | "copied">("idle");
  const copy = () => {
    copyText(standupMarkdown(report)).then(
      () => {
        setState("copied");
        setTimeout(() => setState("idle"), 1500);
      },
      (e) => notify.error(`Couldn't copy the standup: ${errorText(e)}`),
    );
  };
  return (
    <Button size="sm" variant="outline" onClick={copy} disabled={disabled}>
      {state === "copied" ? <Check /> : <Copy />}
      {state === "copied" ? "Copied" : "Copy as standup"}
    </Button>
  );
}

export function standupMarkdown(report: Today): string {
  const lines = [`## Since ${formatSince(report.since)}`];
  for (const group of report.groups) {
    lines.push("", `### ${group.project ? baseName(group.project) : "No project"}`);
    // A commit linked to two sessions of the repo is listed once, under the first.
    const listed = new Set<string>();
    for (const t of group.threads) {
      lines.push(
        `- ${t.title || "Untitled session"} (${SOURCE_LABELS[t.source] ?? t.source}, ${formatDuration(t.durationSecs)})`,
      );
      for (const c of t.commits) {
        if (listed.has(c.sha)) continue;
        listed.add(c.sha);
        lines.push(`  - \`${c.shortSha}\`${c.subject ? ` ${c.subject}` : ""}`);
      }
    }
  }
  if (report.newTodos.length > 0) {
    lines.push("", "### New TODOs");
    for (const todo of report.newTodos) {
      lines.push(`- ${todo.text}${todo.project ? ` (${baseName(todo.project)})` : ""}`);
    }
  }
  return lines.join("\n");
}

function EmptyToday({ since }: { since: string }) {
  const openSearch = useUi((s) => s.openSearch);
  const setView = useUi((s) => s.setView);
  const link = "h-auto p-0 align-baseline text-sm";
  return (
    <div role="status" className="py-10 text-sm text-muted-foreground">
      <p>
        Nothing since {since}. Open{" "}
        <Button variant="link" className={link} onClick={() => openSearch()}>
          Search
        </Button>{" "}
        or{" "}
        <Button variant="link" className={link} onClick={() => setView("projects")}>
          Projects
        </Button>
        .
      </p>
    </div>
  );
}

/** One row of disclosure buttons for what came up besides the sessions; the list for the
 *  pressed one opens below. Kinds with nothing to show are left out. */
function Strip({
  report,
  panel,
  onToggle,
  panelId,
}: {
  report: Today;
  panel: Panel | null;
  onToggle: (p: Panel | null) => void;
  panelId: string;
}) {
  const items: { id: Panel; n: number; label: string; icon: typeof CircleDot }[] = [
    {
      id: "todos" as const,
      n: report.newTodos.length,
      label: report.newTodos.length === 1 ? "new TODO" : "new TODOs",
      icon: CircleDot,
    },
    {
      id: "issues" as const,
      n: report.issuesSeenAgain.length,
      label: report.issuesSeenAgain.length === 1 ? "error seen again" : "errors seen again",
      icon: AlertTriangle,
    },
    {
      id: "snapshots" as const,
      n: report.snapshots.length,
      label: report.snapshots.length === 1 ? "snapshot" : "snapshots",
      icon: History,
    },
  ].filter((i) => i.n > 0);
  if (items.length === 0) return null;
  return (
    <div className="flex flex-wrap gap-2 pb-3">
      {items.map(({ id, n, label, icon: Icon }) => (
        <Button
          key={id}
          size="sm"
          variant={panel === id ? "secondary" : "outline"}
          aria-expanded={panel === id}
          aria-controls={panel === id ? panelId : undefined}
          onClick={() => onToggle(panel === id ? null : id)}
        >
          <Icon className={cn(id === "issues" && "text-amber-600 dark:text-amber-400")} />
          <span className="tabular-nums">{n}</span> {label}
        </Button>
      ))}
    </div>
  );
}

function StripPanel({ id, panel, report }: { id: string; panel: Panel; report: Today }) {
  const openThread = useUi((s) => s.openThread);
  // The repo's key, never the stored path: a worktree session's snapshot stores the
  // worktree path, which is no key, so the Project page opened on it (and remembered it as
  // Search's scope) matched nothing.
  const projectOf = (snap: Today["snapshots"][number]) => snap.projectKey || null;
  const row =
    "flex w-full cursor-pointer items-baseline gap-3 px-3 py-2 text-left text-sm outline-none hover:bg-muted/50 focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring/60";
  return (
    <ul id={id} className="mb-5 divide-y overflow-hidden rounded-lg border">
      {panel === "todos" &&
        report.newTodos.map((todo) => (
          <li key={todo.id}>
            <button
              type="button"
              className={row}
              onClick={() => openThread(todo.threadId)}
              title="Open the session it came from"
            >
              <span className="min-w-0 flex-1">{todo.text}</span>
              {todo.project && (
                <span className="shrink-0 text-xs text-muted-foreground">
                  {baseName(todo.project)}
                </span>
              )}
            </button>
          </li>
        ))}
      {panel === "issues" &&
        report.issuesSeenAgain.map((issue) => (
          <li key={issue.signature}>
            <button
              type="button"
              className={row}
              disabled={!issue.project}
              onClick={() => issue.project && openProjectSection(issue.project, "errors")}
              title={issue.project ? "Open the project's recurring errors" : undefined}
            >
              <span className="min-w-0 flex-1 truncate font-mono text-xs">{issue.signature}</span>
              <span className="shrink-0 text-xs text-muted-foreground tabular-nums">
                {plural(issue.count, "time")}
                {issue.project && ` · ${baseName(issue.project)}`} · {formatTime(issue.lastSeen)}
              </span>
            </button>
          </li>
        ))}
      {panel === "snapshots" &&
        report.snapshots.map((snap) => {
          const project = projectOf(snap);
          return (
            <li key={snap.id}>
              <button
                type="button"
                className={row}
                disabled={!project && snap.threadId == null}
                onClick={() => {
                  if (project) openProjectSection(project, "snapshots");
                  else if (snap.threadId != null) openThread(snap.threadId);
                }}
                title={project ? "Open the project's snapshots" : "Open the session"}
              >
                {/* An auto snapshot's label only repeats its event ("auto · PreCompact"),
                    so it goes by its session, with the event in words beside it. */}
                <span className="min-w-0 flex-1 truncate">
                  {(snap.event === "manual" && snap.label) ||
                    snap.threadTitle ||
                    `Snapshot #${snap.id}`}
                </span>
                <span className="shrink-0 text-xs text-muted-foreground tabular-nums">
                  {snapshotEventText(snap.event)} · {Math.max(1, Math.round(snap.chars / 1000))}k
                  chars · {formatTime(snap.createdAt)}
                </span>
              </button>
            </li>
          );
        })}
    </ul>
  );
}

/** ArrowUp / ArrowDown move between the session rows (their title buttons), across repos;
 *  Tab still walks every control in order. */
function moveRowFocus(e: KeyboardEvent<HTMLElement>) {
  if (e.key !== "ArrowDown" && e.key !== "ArrowUp") return;
  const rows = [...e.currentTarget.querySelectorAll<HTMLElement>("[data-today-row]")];
  const at = rows.indexOf(document.activeElement as HTMLElement);
  if (at < 0) return;
  e.preventDefault();
  rows[Math.min(rows.length - 1, Math.max(0, at + (e.key === "ArrowDown" ? 1 : -1)))]?.focus();
}

function SessionGroups({ report }: { report: Today }) {
  const openProject = useUi((s) => s.openProject);
  // Fixed for the session, like ThreadView: a Linux box without a terminal hides Resume.
  const cliLaunch = useQuery({
    queryKey: ["cli_launch_supported"],
    queryFn: api.cliLaunchSupported,
    staleTime: Number.POSITIVE_INFINITY,
  });
  const canLaunch = cliLaunch.data ?? CAN_LAUNCH_CLI_FALLBACK;
  return (
    // biome-ignore lint/a11y/noStaticElementInteractions: only forwards arrow keys between the row buttons it contains
    <div className="space-y-6" onKeyDown={moveRowFocus}>
      {report.groups.map((group) => {
        // The backend groups sessions with no project under "": no page to open for them.
        const name = group.project ? baseName(group.project) : "No project";
        return (
          <section key={group.project} aria-label={name}>
            <div className="flex items-center gap-2 pb-1.5">
              <h2
                className="min-w-0 truncate text-sm font-semibold"
                title={group.project || undefined}
              >
                {name}
              </h2>
              <span className="text-xs text-muted-foreground tabular-nums">
                {plural(group.threads.length, "session")}
              </span>
              {group.project && (
                <Button
                  size="xs"
                  variant="ghost"
                  className="ml-auto text-muted-foreground"
                  onClick={() => openProject(group.project)}
                >
                  Open project
                </Button>
              )}
            </div>
            <ul className="divide-y rounded-lg border">
              {group.threads.map((t) => (
                <SessionRow key={t.id} thread={t} canLaunch={canLaunch} />
              ))}
            </ul>
          </section>
        );
      })}
    </div>
  );
}

function SessionRow({ thread, canLaunch }: { thread: TodayThread; canLaunch: boolean }) {
  const openThread = useUi((s) => s.openThread);
  const canResume = canLaunch && (thread.source === "claude_code" || thread.source === "codex");
  const resume = useMutation({
    mutationFn: () => api.resumeThread(thread.id),
    onError: (e) => notify.error(`Couldn't resume the session: ${errorText(e)}`),
  });
  const title = thread.title || "Untitled session";
  return (
    <li className="px-3 py-2.5">
      <div className="flex items-start gap-3">
        <div className="min-w-0 flex-1">
          <button
            type="button"
            data-today-row
            onClick={() => openThread(thread.id)}
            className="block max-w-full cursor-pointer truncate rounded-sm text-left text-sm font-medium outline-none hover:underline hover:underline-offset-4 focus-visible:ring-2 focus-visible:ring-ring/60"
          >
            {title}
          </button>
          <p className="mt-0.5 text-xs text-muted-foreground tabular-nums">
            {SOURCE_LABELS[thread.source] ?? thread.source} · {formatDuration(thread.durationSecs)}{" "}
            · {plural(thread.messages, "message")} · {formatTime(thread.lastActive)}
          </p>
        </div>
        <div className="flex shrink-0 items-center gap-1.5">
          {canResume && (
            <Button
              size="xs"
              variant="outline"
              onClick={() => resume.mutate()}
              disabled={resume.isPending}
              title={`Resume "${title}" in a terminal`}
            >
              <TerminalSquare />
              Resume
            </Button>
          )}
          <Button
            size="xs"
            variant="outline"
            onClick={() => openThread(thread.id)}
            aria-label={`Open "${title}"`}
          >
            Open
          </Button>
        </div>
      </div>
      {thread.commits.length > 0 && (
        <div className="mt-2 flex flex-wrap gap-1.5">
          {thread.commits.map((c) => (
            <CommitChip key={c.sha} sha={c.sha} shortSha={c.shortSha} subject={c.subject} />
          ))}
        </div>
      )}
    </li>
  );
}

/** A produced commit: its short sha and subject. Clicking copies the full sha. */
function CommitChip({
  sha,
  shortSha,
  subject,
}: {
  sha: string;
  shortSha: string;
  subject: string | null;
}) {
  const [copied, setCopied] = useState(false);
  const copy = () => {
    copyText(sha).then(
      () => {
        setCopied(true);
        setTimeout(() => setCopied(false), 1500);
      },
      (e) => notify.error(`Couldn't copy the commit sha: ${errorText(e)}`),
    );
  };
  return (
    <button
      type="button"
      onClick={copy}
      title={copied ? "Copied" : `Copy ${sha}`}
      aria-label={`Copy commit ${shortSha}${subject ? `: ${subject}` : ""}`}
      className="inline-flex max-w-full cursor-pointer items-center gap-1.5 rounded-md border bg-muted/40 px-1.5 py-0.5 text-xs outline-none hover:bg-muted focus-visible:ring-2 focus-visible:ring-ring/60"
    >
      <span className="inline-flex w-[7ch] justify-center font-mono text-[11px] text-primary">
        {copied ? <Check className="inline size-3" /> : shortSha}
      </span>
      {subject && <span className="max-w-[44ch] truncate text-muted-foreground">{subject}</span>}
    </button>
  );
}
