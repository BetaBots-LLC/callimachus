import {
  type ComponentProps,
  type KeyboardEvent,
  type ReactNode,
  useEffect,
  useId,
  useState,
} from "react";
import { keepPreviousData, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  AlertCircle,
  AlertTriangle,
  ArrowUpRight,
  Brain,
  Check,
  ChevronDown,
  ChevronRight,
  CircleDot,
  Copy,
  History,
  Inbox,
  Sparkles,
  X,
} from "lucide-react";
import { api, type ReviewItem, SOURCE_LABELS, type Today, type TodayThread } from "../lib/api";
import { copyText } from "../lib/clipboard";
import {
  baseName,
  formatDuration,
  formatSince,
  formatTime,
  snapshotEventText,
} from "../lib/format";
import { invalidateDomain } from "../lib/keys";
import { AGGREGATE_STALE_MS, queryClient } from "../lib/queryClient";
import { errorText, notify } from "../lib/toast";
import { useUi } from "../store/ui";
import { factDate } from "./project/FactTruth";
import { openProjectSection } from "./project/sections";
import { CommitSha, formatChars, fullDate, useCanLaunchCli } from "./project/shared";
import { Alert, AlertAction, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Card, CardAction, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import {
  Empty,
  EmptyContent,
  EmptyDescription,
  EmptyHeader,
  EmptyMedia,
  EmptyTitle,
} from "@/components/ui/empty";
import {
  Item,
  ItemActions,
  ItemContent,
  ItemDescription,
  ItemFooter,
  ItemGroup,
  ItemSeparator,
  ItemTitle,
} from "@/components/ui/item";
import { Spinner } from "@/components/ui/spinner";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";
import { Loading } from "./Loading";

/**
 * Today: what happened since the app was last opened, grouped by repo, with the commits
 * each session produced and what came up along the way (memory to review, new TODOs, errors
 * seen again, snapshots). Built from the index alone; nothing here calls an LLM.
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

// Review items the user settled (`reviewKey`), hidden until a Today load that leaves them out
// lands: a load that started before the write committed still lists them. Every action is
// stored too (Keep on a recorded or replaced fact through `keep_review_item`), so the backend
// leaves a settled item out, and a key the fresh list lacks is dropped (`forgetSettled`).
// The key names the fact, not the flag: kept for good, a fact flagged again after a Keep
// stayed off Today for as long as the webview lived (days, with close-to-tray).
const keptReview = new Set<string>();
const reviewKey = (i: ReviewItem) => `${i.reason}:${i.factId}:${i.other?.id ?? ""}`;
const toReview = (report: Today) =>
  (report.memoryReview ?? []).filter((i) => !keptReview.has(reviewKey(i)));
function forgetSettled(report: Today) {
  const listed = new Set((report.memoryReview ?? []).map(reviewKey));
  for (const key of keptReview) if (!listed.has(key)) keptReview.delete(key);
}

/** Drop a settled review item from every cached Today window, so the strip, the panel and the
 *  empty state update together instead of after the refetch (which also links commits, for
 *  up to 2 s, and may fail). */
function dropReviewItem(key: string) {
  queryClient.setQueriesData<Today>({ queryKey: ["today"] }, (d) =>
    d?.memoryReview
      ? { ...d, memoryReview: d.memoryReview.filter((i) => reviewKey(i) !== key) }
      : d,
  );
}

/** Where focus goes when a settled review row unmounts with the button that held it (focus
 *  fell to <body>), as MemorySection's focusFact does: the next row's first button, else the
 *  previous row's, else a strip button that stays, else the heading. Null when focus is
 *  elsewhere on the page: a user who moved on keeps their place. */
function focusAfterReview(listId: string, key: string): HTMLElement | null {
  const list = document.getElementById(listId);
  const active = document.activeElement;
  if (active && active !== document.body && !list?.contains(active)) return null;
  const row = list?.querySelector(`[data-review-key="${CSS.escape(key)}"]`);
  const sibling = row?.nextElementSibling ?? row?.previousElementSibling;
  return (
    sibling?.querySelector<HTMLElement>("button:not(:disabled)") ??
    document.querySelector<HTMLElement>("[data-panel]:not([data-panel='memory'])") ??
    document.querySelector<HTMLElement>("[data-today-heading]")
  );
}

type Panel = "memory" | "todos" | "issues" | "snapshots";

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
      forgetSettled(data);
      return data;
    },
    // Watcher writes mark it stale (AGGREGATE_KEYS); a revisit then refreshes it.
    staleTime: AGGREGATE_STALE_MS,
    placeholderData: keepPreviousData,
  });

  // The first successful load of an open moves "last open" to now, after the window on
  // screen has been read, so the next launch starts where this one did. Keyed on the data
  // too: a new open (the window shown again after an hour) refetches while still a success.
  // Only a "since last open" load counts, and not the rows of another window held as its
  // placeholder: marking after a 24 hour load (the first, when "since last open" failed and
  // the picker on the error was used, or any after a new open) moved the mark to now before
  // that window was read, and it then came back empty for good.
  // biome-ignore lint/correctness/useExhaustiveDependencies: dataUpdatedAt is the trigger (a refetch after a new open), read through the module flag
  useEffect(() => {
    if (range !== "open" || report.isPlaceholderData || !report.isSuccess || markedOpen) return;
    markedOpen = true;
    api.markAppOpened().catch(() => {}); // an older backend without it just rejects
  }, [range, report.isPlaceholderData, report.isSuccess, report.dataUpdatedAt]);

  // The last load error, kept while its retry runs. Query puts a query with no data back to
  // pending on a refetch and clears its error, so without this "Try again" (or picking
  // another window) swapped the message for the loading pane: the control just pressed
  // unmounted and focus fell to the page body.
  const [loadError, setLoadError] = useState<unknown>(null);
  if (report.error && report.error !== loadError) setLoadError(report.error);
  else if (!report.error && report.data !== undefined && loadError !== null) setLoadError(null);

  // The first load of the view: nothing to show and nothing failed yet.
  if (report.data === undefined && loadError === null) {
    return <Loading label="Loading today…" className="h-full" />;
  }

  const data = report.data;
  const counts = data ? todayCounts(data) : null;
  // The previous window's rows, shown dimmed while the picked one loads.
  const stale = report.isPlaceholderData;
  const focusHeading = () => document.querySelector<HTMLElement>("[data-today-heading]")?.focus();
  // A retry that works unmounts the message and its button: the heading takes the focus.
  const retry = () =>
    void report.refetch().then((r) => {
      if (!r.isError) focusHeading();
    });

  return (
    // A block scroller, not a flex column: flex children with overflow-hidden shrink to
    // nothing once the page is taller than the window.
    <div className="h-full overflow-y-auto">
      <div className="mx-auto w-full max-w-3xl px-6 pt-5 pb-10">
        <ReportCard />
        {/* One header for every state, so the picker keeps its focus when a window fails or
            a failed one loads. items-start with a heading line as tall as the controls: they
            stay on the heading's line however many lines the summary under it takes. */}
        <header className="flex flex-wrap items-start gap-x-4 gap-y-3 pb-6">
          <div className="min-w-0 flex-1">
            <h1
              data-today-heading
              tabIndex={-1}
              className="text-lg leading-7 font-semibold tracking-tight text-balance outline-none"
            >
              {data ? `Since ${formatSince(data.since)}` : "Today"}
            </h1>
            {stale ? (
              // In place of the counts, which belong to the window being left.
              <p role="status" className="flex items-center gap-1.5 text-sm text-muted-foreground">
                <Spinner className="size-3.5" />
                Loading…
              </p>
            ) : (
              data &&
              counts &&
              counts.sessions > 0 && (
                <p className="text-sm text-muted-foreground tabular-nums">
                  {data.truncated
                    ? `The newest ${plural(counts.sessions, "session")}`
                    : plural(counts.sessions, "session")}{" "}
                  in {plural(data.groups.length, "repo")}
                  {counts.commits > 0 && ` · ${plural(counts.commits, "commit")}`}
                </p>
              )
            )}
          </div>
          <RangePicker range={range} onChange={setRange} />
          <CopyStandup
            report={data}
            // The standup lists sessions and new TODOs; with neither it would copy a lone
            // heading, and while a window loads it would copy the one being left.
            disabled={!counts || counts.standupEmpty || stale}
          />
        </header>

        {data === undefined ? (
          // Wide enough on the right for the button with its spinner.
          <Alert variant="destructive" className="has-data-[slot=alert-action]:pr-32">
            <AlertCircle />
            <AlertTitle>Couldn't load today's activity</AlertTitle>
            <AlertDescription className="wrap-break-word">
              {errorText(report.error ?? loadError)}
            </AlertDescription>
            <AlertAction>
              <Button
                size="sm"
                variant="outline"
                // Focusable while it retries, so the focus is still here if it fails again.
                focusableWhenDisabled
                disabled={report.isFetching}
                className="text-foreground aria-disabled:opacity-50"
                onClick={retry}
              >
                {report.isFetching && <Spinner data-icon="inline-start" />}
                Try again
              </Button>
            </AlertAction>
          </Alert>
        ) : (
          <>
            {report.isRefetchError && (
              // A refetch that fails (the watcher invalidates Today after every index pass;
              // a busy index, a slow git) keeps the loaded page and says so here instead
              // of replacing the page, its open panel and the focus in it.
              <Alert variant="destructive" className="mb-6 has-data-[slot=alert-action]:pr-28">
                <AlertCircle />
                <AlertTitle>Couldn't refresh, so this may be out of date</AlertTitle>
                <AlertDescription className="wrap-break-word">
                  {errorText(report.error)}
                </AlertDescription>
                <AlertAction>
                  <Button
                    size="sm"
                    variant="outline"
                    focusableWhenDisabled
                    disabled={report.isFetching}
                    className="text-foreground aria-disabled:opacity-50"
                    onClick={retry}
                  >
                    Try again
                  </Button>
                </AlertAction>
              </Alert>
            )}
            {/* Dimmed and inert below the header only: the picker just pressed stays crisp,
                and neither a click nor the keyboard can Keep or Resume a row of the window
                being left (a Keep there wrote, and the placeholder row stayed to be kept
                again). */}
            <div
              inert={stale}
              aria-busy={stale || undefined}
              className={cn("transition-opacity", stale && "pointer-events-none opacity-60")}
            >
              <TodayBody
                report={data}
                range={range}
                onRange={(r) => {
                  // From a button inside the body, which goes inert (and, for the empty
                  // state's own button, away) as the window changes.
                  focusHeading();
                  setRange(r);
                }}
              />
            </div>
          </>
        )}
      </div>
    </div>
  );
}

/** What a window holds, counted once for the header and the body. */
function todayCounts(report: Today) {
  const sessions = report.groups.reduce((n, g) => n + g.threads.length, 0);
  // gitlink can link one commit to two sessions, so commits count once by sha.
  const commits = new Set(
    report.groups.flatMap((g) => g.threads.flatMap((t) => t.commits)).map((c) => c.sha),
  ).size;
  const extras =
    toReview(report).length +
    report.newTodos.length +
    report.issuesSeenAgain.length +
    report.snapshots.length;
  return {
    sessions,
    commits,
    extras,
    nothing: sessions === 0 && extras === 0,
    standupEmpty: sessions === 0 && report.newTodos.length === 0,
  };
}

/** Everything under the header for a loaded window: the strip and its panel, then the
 *  sessions by repo. */
function TodayBody({
  report,
  range,
  onRange,
}: {
  report: Today;
  range: Range;
  onRange: (r: Range) => void;
}) {
  const [openPanel, setPanel] = useState<Panel | null>(null);
  const panelId = useId();
  const counts = todayCounts(report);
  const since = formatSince(report.since);
  if (counts.nothing) return <EmptyToday since={since} range={range} onRange={onRange} />;
  // A panel whose kind dropped to nothing (another range, a refetch) closes, since its
  // strip button, the only control that closes it, is gone too.
  const panel = openPanel && panelCount(report, openPanel) > 0 ? openPanel : null;
  // Escape in a panel closes it and hands focus back to the button that opened it.
  const closePanel = () => {
    if (!panel) return;
    setPanel(null);
    document.querySelector<HTMLElement>(`[data-panel="${panel}"]`)?.focus();
  };
  return (
    <>
      {counts.extras > 0 && (
        <div className="mb-6 space-y-2">
          <Strip report={report} panel={panel} onToggle={setPanel} panelId={panelId} />
          {panel && <StripPanel id={panelId} panel={panel} report={report} onClose={closePanel} />}
        </div>
      )}
      {counts.sessions === 0 ? (
        <p className="px-1 text-sm text-muted-foreground">No sessions since {since}.</p>
      ) : (
        <SessionGroups report={report} />
      )}
      {report.commitsPartial && (
        <p className="px-1 pt-4 text-[13px] text-muted-foreground">
          Some repos show no commits: reading their git history took longer than 2 seconds.
        </p>
      )}
    </>
  );
}

/**
 * "Your agents, so far": the first-run nudge toward the Report view, once the first index has
 * finished. It goes away for good once the report has been opened (the checklist's report
 * step is done) or dismissed here (the step is skipped; the checklist and Stats still link
 * to it).
 */
function ReportCard() {
  const qc = useQueryClient();
  const setView = useUi((s) => s.setView);
  const steps = useQuery({ queryKey: ["onboarding_state"], queryFn: api.onboardingState });
  const stats = useQuery({ queryKey: ["db_stats"], queryFn: api.dbStats });
  const indexing = useQuery({
    queryKey: ["index_status"],
    queryFn: api.indexingStatus,
    refetchInterval: (q) => (q.state.data ? 2000 : false),
  });
  const dismiss = useMutation({
    mutationFn: () => api.setOnboardingStep("report", "skipped"),
    onSuccess: (s) => qc.setQueryData(["onboarding_state"], s),
    onError: (e) => notify.error("Couldn't hide the card", { description: errorText(e) }),
  });
  const answered = !steps.isSuccess || (steps.data.report ?? null) !== null;
  const indexed = !!stats.data && stats.data.threads > 0 && indexing.data === false;
  if (answered || !indexed || dismiss.isPending) return null;
  return (
    <section aria-label="Your agents, so far" className="mb-6">
      <Card size="sm">
        <CardHeader>
          {/* Not a heading: it sits above the page's h1. The section's label names it. */}
          <CardTitle className="flex items-center gap-2">
            <Sparkles aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
            Your agents, so far
          </CardTitle>
          <CardDescription className="text-[13px] leading-normal">
            Every session you've indexed on one screen: time, spend, the errors that keep coming
            back and what your agents remember. Copy it as markdown or save a card to share.
          </CardDescription>
          <CardAction className="flex items-center gap-1">
            <Button size="sm" onClick={() => setView("report")}>
              See your report
            </Button>
            <TooltipProvider delay={TOOLTIP_DELAY_MS}>
              <Tooltip>
                <TooltipTrigger
                  render={
                    <Button
                      size="icon-sm"
                      variant="ghost"
                      // The whole hint: the tooltip is visual only, and the title it
                      // replaced was the button's description.
                      aria-label="Dismiss (the report stays in Stats)"
                      onClick={() => {
                        // The card unmounts with the button in it: the heading takes the
                        // focus that would otherwise fall to the page body.
                        document.querySelector<HTMLElement>("[data-today-heading]")?.focus();
                        dismiss.mutate();
                      }}
                    />
                  }
                >
                  <X />
                </TooltipTrigger>
                <TooltipContent>Dismiss (the report stays in Stats)</TooltipContent>
              </Tooltip>
            </TooltipProvider>
          </CardAction>
        </CardHeader>
      </Card>
    </section>
  );
}

/** As the top bar's tooltips (App.tsx): long enough that passing over a button shows nothing. */
const TOOLTIP_DELAY_MS = 200;

function panelCount(report: Today, panel: Panel): number {
  if (panel === "memory") return toReview(report).length;
  if (panel === "todos") return report.newTodos.length;
  if (panel === "issues") return report.issuesSeenAgain.length;
  return report.snapshots.length;
}

/** "4 commits", for the screen only: the no-break space keeps a wrapped line from ending on
 *  the number with its noun alone on the next. */
function plural(n: number, word: string): string {
  return `${n.toLocaleString()}\u00a0${word}${n === 1 ? "" : "s"}`;
}

/** The time window: one Tab stop, arrow keys between the windows. */
function RangePicker({ range, onChange }: { range: Range; onChange: (r: Range) => void }) {
  return (
    <ToggleGroup
      aria-label="Time window"
      variant="outline"
      size="sm"
      spacing={0}
      value={[range]}
      onValueChange={(v) => {
        // Pressing the selected window again reports none; Today always has one.
        const next = v[0] as Range | undefined;
        if (next) onChange(next);
      }}
      className="shrink-0"
    >
      {RANGES.map((r) => (
        <ToggleGroupItem
          key={r.id}
          value={r.id}
          className="text-muted-foreground aria-pressed:text-foreground"
        >
          {r.label}
        </ToggleGroupItem>
      ))}
    </ToggleGroup>
  );
}

/** Plain markdown for a standup note: each repo, its sessions and commits, then the new
 *  TODOs. Copied straight from the click, since WebKit allows clipboard writes only
 *  during the gesture. */
function CopyStandup({ report, disabled }: { report: Today | undefined; disabled: boolean }) {
  const [state, setState] = useState<"idle" | "copied">("idle");
  const copied = state === "copied";
  const copy = () => {
    if (!report) return;
    copyText(standupMarkdown(report)).then(
      () => {
        setState("copied");
        setTimeout(() => setState("idle"), 1500);
      },
      (e) => notify.error(`Couldn't copy the standup: ${errorText(e)}`),
    );
  };
  return (
    <>
      <Button size="sm" variant="outline" onClick={copy} disabled={disabled}>
        {copied ? <Check data-icon="inline-start" /> : <Copy data-icon="inline-start" />}
        {/* Both labels in one cell, so "Copied" does not narrow the button and pull the
            picker beside it sideways. */}
        <span className="grid text-left">
          <span className={cn("col-start-1 row-start-1", copied && "invisible")}>
            Copy as standup
          </span>
          {copied && <span className="col-start-1 row-start-1">Copied</span>}
        </span>
      </Button>
      <span aria-live="polite" className="sr-only">
        {copied ? "Copied the standup" : ""}
      </span>
    </>
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

function EmptyToday({
  since,
  range,
  onRange,
}: {
  since: string;
  range: Range;
  onRange: (r: Range) => void;
}) {
  const openSearch = useUi((s) => s.openSearch);
  const setView = useUi((s) => s.setView);
  return (
    <Empty role="status" className="border py-12">
      <EmptyHeader>
        <EmptyMedia variant="icon">
          <Inbox />
        </EmptyMedia>
        <EmptyTitle>Nothing new</EmptyTitle>
        <EmptyDescription>
          No sessions, TODOs, errors or snapshots since {since}. They show up here as your agents
          work.
        </EmptyDescription>
      </EmptyHeader>
      <EmptyContent className="flex-row flex-wrap justify-center gap-2">
        {/* Widening the window first: the likeliest reason for an empty one is its size. */}
        {range !== "7d" && (
          <Button size="sm" variant="outline" onClick={() => onRange("7d")}>
            Show 7 days
          </Button>
        )}
        <Button size="sm" variant="outline" onClick={() => openSearch()}>
          Open Search
        </Button>
        <Button size="sm" variant="outline" onClick={() => setView("projects")}>
          Open Projects
        </Button>
      </EmptyContent>
    </Empty>
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
  const review = toReview(report).length;
  // The backend lists at most 50 and counts the rest: "50+ facts to review".
  const moreReview = (report.memoryReviewMore ?? 0) > 0;
  type Item = { id: Panel; n: number; more?: boolean; label: string; icon: typeof CircleDot };
  const items: Item[] = [
    {
      id: "memory" as const,
      n: review,
      more: moreReview,
      label: review === 1 && !moreReview ? "fact to review" : "facts to review",
      icon: Brain,
    },
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
    <div className="flex flex-wrap gap-2">
      {items.map(({ id, n, more, label, icon: Icon }) => (
        // Outline in both states: the variant fills itself while aria-expanded, and keeps
        // the border (the borderless secondary read as disabled beside its neighbors).
        <Button
          key={id}
          data-panel={id}
          size="sm"
          variant="outline"
          className="dark:aria-expanded:bg-muted"
          aria-expanded={panel === id}
          aria-controls={panel === id ? panelId : undefined}
          onClick={() => onToggle(panel === id ? null : id)}
        >
          <Icon
            data-icon="inline-start"
            className={cn(
              (id === "issues" || id === "memory") && "text-amber-600 dark:text-amber-400",
            )}
          />
          <span className="tabular-nums">{more ? `${n}+` : n}</span> {label}
          <ChevronDown
            data-icon="inline-end"
            className="text-muted-foreground transition-transform group-aria-expanded/button:rotate-180"
          />
        </Button>
      ))}
    </div>
  );
}

/**
 * The carded list every part of Today uses: a Card around an ItemGroup, each row an Item in
 * its own listitem (`ListRow`), parted by ItemSeparators.
 */
function ItemList({ label, children }: { label: string; children: ReactNode }) {
  return (
    <Card className="gap-0 py-0">
      <ItemGroup aria-label={label} className="gap-0 has-data-[size=sm]:gap-0">
        {children}
      </ItemGroup>
    </Card>
  );
}

/** One row of an ItemList. The separator sits inside the listitem, so a row's next sibling is
 *  the next row (what `focusAfterReview` walks) and the list holds listitems only. */
function ListRow({
  first,
  children,
  ...props
}: { first: boolean; children: ReactNode } & ComponentProps<"div">) {
  return (
    // biome-ignore lint/a11y/useSemanticElements: ItemGroup is a div with role="list", where an li is not allowed
    <div role="listitem" {...props}>
      {!first && <ItemSeparator className="my-0" />}
      {children}
    </div>
  );
}

/** An Item that is a square row of a carded list, tinted on hover like the app's other rows. */
const ROW = "rounded-none transition-colors hover:bg-muted/40";
/** A row whose title is its click target: the title's button is stretched over the whole Item
 *  (`STRETCH`), and whatever else can be pressed in the row sits above it (`ABOVE`). A row
 *  that holds other buttons cannot itself be one. */
const ROW_STRETCHED = cn(ROW, "relative has-[[data-stretch]:focus-visible]:bg-muted/40");
const STRETCH =
  "cursor-pointer text-left outline-none after:absolute after:inset-0 focus-visible:after:ring-[3px] focus-visible:after:ring-ring/50 focus-visible:after:ring-inset";
const ABOVE = "relative z-10";
/** A row that is one button (Item rendered as a button): nothing else to press in it. */
const ROW_BUTTON = cn(ROW, "cursor-pointer text-left focus-visible:bg-muted/40");
/** The second line of a row. */
const ROW_META = "line-clamp-1 text-[13px] tabular-nums";

const PANEL_LABELS: Record<Panel, string> = {
  memory: "Memory to review",
  todos: "New TODOs",
  issues: "Errors seen again",
  snapshots: "Snapshots",
};

/** `aria-controls` of the pressed strip button points here. */
function PanelFrame({
  id,
  onClose,
  children,
}: {
  id: string;
  onClose: () => void;
  children: ReactNode;
}) {
  return (
    // biome-ignore lint/a11y/noStaticElementInteractions: only takes Escape from the controls it contains
    <div
      id={id}
      onKeyDown={(e) => {
        if (e.key !== "Escape") return;
        e.preventDefault();
        onClose();
      }}
    >
      {children}
    </div>
  );
}

function StripPanel({
  id,
  panel,
  report,
  onClose,
}: {
  id: string;
  panel: Panel;
  report: Today;
  onClose: () => void;
}) {
  const openThread = useUi((s) => s.openThread);
  if (panel === "memory") {
    return (
      <PanelFrame id={id} onClose={onClose}>
        <MemoryReviewPanel listId={id} items={toReview(report)} />
      </PanelFrame>
    );
  }
  // The repo's key, never the stored path: a worktree session's snapshot stores the
  // worktree path, which is no key, so the Project page opened on it (and remembered it as
  // Search's scope) matched nothing.
  const projectOf = (snap: Today["snapshots"][number]) => snap.projectKey || null;
  const chevron = (
    <ChevronRight aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
  );
  return (
    <PanelFrame id={id} onClose={onClose}>
      <ItemList label={PANEL_LABELS[panel]}>
        {panel === "todos" &&
          report.newTodos.map((todo, i) => (
            <ListRow key={todo.id} first={i === 0}>
              <Item
                size="sm"
                className={ROW_BUTTON}
                render={
                  <button
                    type="button"
                    onClick={() => openThread(todo.threadId)}
                    title="Open the session it came from"
                  />
                }
              >
                <ItemContent className="min-w-0">
                  {/* Three lines at most, broken anywhere: a TODO can be one long path. */}
                  <ItemTitle
                    className="line-clamp-3 block w-full font-normal wrap-break-word"
                    title={todo.text}
                  >
                    {todo.text}
                  </ItemTitle>
                </ItemContent>
                {todo.project && (
                  <span className="max-w-[40%] shrink-0 truncate text-[13px] text-muted-foreground">
                    {baseName(todo.project)}
                  </span>
                )}
                {chevron}
              </Item>
            </ListRow>
          ))}
        {panel === "issues" &&
          report.issuesSeenAgain.map((issue, i) => {
            // Two lines, as the Project page's recurring errors this row opens: the
            // signature names the error, so it gets the width and the counts go under it.
            const content = (
              <ItemContent className="min-w-0">
                <ItemTitle className="block w-full truncate font-normal" title={issue.signature}>
                  <code className="font-mono text-xs">{issue.signature}</code>
                </ItemTitle>
                <ItemDescription className={ROW_META}>
                  <span className="font-medium text-foreground">
                    {issue.count.toLocaleString()}
                  </span>{" "}
                  {issue.count === 1 ? "time" : "times"}
                  {issue.project && ` · ${baseName(issue.project)}`} · last seen{" "}
                  <span title={fullDate(issue.lastSeen)}>{formatTime(issue.lastSeen)}</span>
                </ItemDescription>
              </ItemContent>
            );
            const project = issue.project;
            return (
              <ListRow key={issue.signature} first={i === 0}>
                {project ? (
                  <Item
                    size="sm"
                    className={ROW_BUTTON}
                    render={
                      <button
                        type="button"
                        onClick={() => openProjectSection(project, "errors")}
                        title="Open the project's recurring errors"
                      />
                    }
                  >
                    {content}
                    {chevron}
                  </Item>
                ) : (
                  // Nothing to open: text to read, so no button, pointer or hover.
                  <Item size="sm" className="rounded-none">
                    {content}
                  </Item>
                )}
              </ListRow>
            );
          })}
        {panel === "snapshots" &&
          report.snapshots.map((snap, i) => {
            const project = projectOf(snap);
            // An auto snapshot's label only repeats its event ("auto · PreCompact"), so it
            // goes by its session, with the event in words under it.
            const name =
              (snap.event === "manual" && snap.label) || snap.threadTitle || `Snapshot #${snap.id}`;
            const content = (
              <ItemContent className="min-w-0">
                <ItemTitle className="block w-full truncate font-normal" title={name}>
                  {name}
                </ItemTitle>
                <ItemDescription className={ROW_META}>
                  {project && `${baseName(project)} · `}
                  {snapshotEventText(snap.event)} · {formatChars(snap.chars)} ·{" "}
                  <span title={fullDate(snap.createdAt)}>{formatTime(snap.createdAt)}</span>
                </ItemDescription>
              </ItemContent>
            );
            const threadId = snap.threadId;
            return (
              <ListRow key={snap.id} first={i === 0}>
                {project || threadId != null ? (
                  <Item
                    size="sm"
                    className={ROW_BUTTON}
                    render={
                      <button
                        type="button"
                        onClick={() => {
                          if (project) openProjectSection(project, "snapshots");
                          else if (threadId != null) openThread(threadId);
                        }}
                        title={project ? "Open the project's snapshots" : "Open the session"}
                      />
                    }
                  >
                    {content}
                    {chevron}
                  </Item>
                ) : (
                  <Item size="sm" className="rounded-none">
                    {content}
                  </Item>
                )}
              </ListRow>
            );
          })}
      </ItemList>
    </PanelFrame>
  );
}

/** Why a fact is listed, in words: "Flagged stale by an agent", "Replaced by an agent". */
function reviewHeadline(item: ReviewItem): string {
  const who =
    item.source === "agent"
      ? "an agent"
      : item.source === "distill"
        ? "a memory pass"
        : item.source === "repo"
          ? "a repo import"
          : item.source === "review"
            ? "a conflict review"
            : "you";
  switch (item.reason) {
    case "flagged":
      return `Flagged ${item.flag ?? "stale"} by ${who}`;
    case "disputed":
      return "Disputed";
    case "replaced":
      return `Replaced by ${who}`;
    default:
      return `Recorded by ${who}`;
  }
}

/**
 * Memory to review: what agents and memory passes did to project memory that nobody looked
 * at, each with the actions that settle it. Keep clears a flag, keeps both sides of a
 * dispute, or accepts a recorded fact or a replacement; Undo restores a replaced fact (every
 * replacement of it); Hide drops the fact from memory. The text opens the project's Memory
 * section, where the full history is.
 */
type ReviewAction = "keep" | "undo" | "hide";
/** Each reason's two actions, in the order of every row: the one that lets the fact (or the
 *  change to it) stand, outlined, then the one that takes it back, quiet. "Keep" means a
 *  different thing per reason, so each says what it does in a tooltip and to a screen reader. */
const REVIEW_ACTIONS: Record<
  ReviewItem["reason"],
  [action: ReviewAction, label: string, hint: string][]
> = {
  flagged: [
    ["keep", "Keep", "It still holds: clear the flag"],
    ["hide", "Hide", "Hide from memory"],
  ],
  disputed: [
    ["keep", "Keep both", "They don't conflict: drop the dispute"],
    ["hide", "Hide", "Hide this one from memory; the other stays"],
  ],
  replaced: [
    ["keep", "Keep", "The replacement stands"],
    ["undo", "Undo", "Restore it: undo every replacement of it"],
  ],
  recorded: [
    ["keep", "Keep", "It belongs in memory"],
    ["hide", "Hide", "Hide from memory"],
  ],
};

function MemoryReviewPanel({ listId, items }: { listId: string; items: ReviewItem[] }) {
  const act = useMutation({
    mutationFn: async ({ item, action }: { item: ReviewItem; action: ReviewAction }) => {
      if (action === "hide") return api.hideFact(item.factId, true);
      if (action === "undo") return api.restoreFact(item.factId);
      if (item.reason === "flagged") return api.resolveFactFlags(item.factId);
      if (item.reason === "disputed" && item.other) {
        return api.unlinkFacts(item.other.id, item.factId, "contradicts");
      }
      if (item.reason === "recorded" || item.reason === "replaced") {
        return api.keepReviewItem(item.reason, item.factId, item.other?.id);
      }
    },
    onSuccess: (_r, { item }) => {
      const key = reviewKey(item);
      // Picked while the row is still mounted; the target's own row (or the strip button,
      // or the heading) stays mounted through the re-render, so it can take focus now.
      const next = focusAfterReview(listId, key);
      keptReview.add(key);
      dropReviewItem(key);
      next?.focus();
      void invalidateDomain("memory", { project: item.project });
    },
    onError: (e) => notify.error("Couldn't update memory", { description: errorText(e) }),
  });
  const busy = (item: ReviewItem) =>
    !!act.isPending && !!act.variables && reviewKey(act.variables.item) === reviewKey(item);
  return (
    <TooltipProvider delay={TOOLTIP_DELAY_MS}>
      <ItemList label={PANEL_LABELS.memory}>
        {items.map((item, i) => {
          const project = item.project;
          return (
            <ListRow key={reviewKey(item)} first={i === 0} data-review-key={reviewKey(item)}>
              <Item
                size="sm"
                className={cn(project ? ROW_STRETCHED : "rounded-none", "items-start")}
              >
                <ItemContent className="min-w-0">
                  <ItemTitle className="line-clamp-none block w-full font-normal wrap-break-word">
                    {project ? (
                      <button
                        type="button"
                        data-stretch
                        className={STRETCH}
                        onClick={() => openProjectSection(project, "memory")}
                        title="Open the project's memory"
                      >
                        {item.text}
                      </button>
                    ) : (
                      item.text
                    )}
                  </ItemTitle>
                  <ItemDescription className="line-clamp-none text-[13px]">
                    {reviewHeadline(item)}
                    {item.at > 0 && ` on ${factDate(item.at)}`}
                    {project && ` · ${baseName(project)}`}
                    {item.reason === "flagged" &&
                      // An unpinned playbook or subagent fact: memory never injects it,
                      // flagged or kept.
                      (item.injectable === false
                        ? " · not part of injected memory either way"
                        : item.trusted
                          ? " · pinned or edited, so it stays in memory"
                          : " · left out of memory until you choose")}
                  </ItemDescription>
                  {item.note && (
                    <ItemDescription className="line-clamp-none text-[13px] wrap-break-word">
                      Reason: <span className="text-foreground">{item.note}</span>
                    </ItemDescription>
                  )}
                  {item.other && (
                    <ItemDescription className="line-clamp-none text-[13px] wrap-break-word">
                      {item.reason === "disputed" ? "Disputed by" : "Replaced by"}:{" "}
                      <span className="text-foreground">{item.other.text}</span>
                    </ItemDescription>
                  )}
                </ItemContent>
                <ItemActions className={cn(ABOVE, "gap-1")}>
                  {(REVIEW_ACTIONS[item.reason] ?? REVIEW_ACTIONS.recorded).map(
                    ([action, label, hint], n) => (
                      <Tooltip key={action}>
                        <TooltipTrigger
                          render={
                            <Button
                              size="xs"
                              variant={n === 0 ? "outline" : "ghost"}
                              // Stays focusable while its write runs, so focus is still in
                              // the list when the row goes (focusAfterReview) or the write
                              // fails; `disabled:` does not match an aria-disabled button.
                              focusableWhenDisabled
                              disabled={busy(item)}
                              className={cn(
                                "aria-disabled:opacity-50",
                                n > 0 && "text-muted-foreground",
                              )}
                              onClick={() => act.mutate({ item, action })}
                            />
                          }
                        >
                          {label}
                          <span className="sr-only">: {hint}</span>
                        </TooltipTrigger>
                        <TooltipContent>{hint}</TooltipContent>
                      </Tooltip>
                    ),
                  )}
                </ItemActions>
              </Item>
            </ListRow>
          );
        })}
      </ItemList>
    </TooltipProvider>
  );
}

/** ArrowUp / ArrowDown move between the session rows (their title buttons) and Home / End
 *  jump to the first and last, across repos and from any control in a row (Resume, a
 *  commit); Tab still walks every control in order. */
function moveRowFocus(e: KeyboardEvent<HTMLElement>) {
  if (!["ArrowDown", "ArrowUp", "Home", "End"].includes(e.key)) return;
  const rows = [...e.currentTarget.querySelectorAll<HTMLElement>("[data-today-row]")];
  const active = document.activeElement;
  const at = rows.findIndex((row) => row.closest("[data-slot=item]")?.contains(active));
  if (at < 0) return;
  e.preventDefault();
  const to =
    e.key === "Home"
      ? 0
      : e.key === "End"
        ? rows.length - 1
        : Math.min(rows.length - 1, Math.max(0, at + (e.key === "ArrowDown" ? 1 : -1)));
  rows[to]?.focus();
}

function SessionGroups({ report }: { report: Today }) {
  const openProject = useUi((s) => s.openProject);
  // Fixed for the session, like ThreadView: a Linux box without a terminal hides Resume.
  const canLaunch = useCanLaunchCli();
  return (
    // biome-ignore lint/a11y/noStaticElementInteractions: only forwards arrow keys between the row buttons it contains
    <div className="space-y-6" onKeyDown={moveRowFocus}>
      {report.groups.map((group) => {
        // The backend groups sessions with no project under "": no page to open for them.
        const name = group.project ? baseName(group.project) : "No project";
        return (
          // Unnamed: thirty repos would be thirty landmarks. The heading gives the structure
          // and the list carries the repo's name.
          <section key={group.project}>
            <div className="flex items-center gap-2 px-1 pb-2">
              <h2
                className="min-w-0 truncate text-sm font-semibold"
                title={group.project || undefined}
              >
                {name}
              </h2>
              <span className="shrink-0 text-[13px] text-muted-foreground tabular-nums">
                {plural(group.threads.length, "session")}
              </span>
              {group.project && (
                <Button
                  size="xs"
                  variant="ghost"
                  className="ml-auto text-muted-foreground"
                  // Starts with the visible words, and tells one repo's button from the next.
                  aria-label={`Open project ${name}`}
                  onClick={() => openProject(group.project)}
                >
                  Open project
                  <ChevronRight data-icon="inline-end" />
                </Button>
              )}
            </div>
            <ItemList label={`Sessions in ${name}`}>
              {group.threads.map((t, i) => (
                <SessionRow key={t.id} thread={t} canLaunch={canLaunch} first={i === 0} />
              ))}
            </ItemList>
          </section>
        );
      })}
    </div>
  );
}

/** A session: its title opens the transcript from anywhere on the row, with Resume beside it
 *  and the commits the session produced under it. */
function SessionRow({
  thread,
  canLaunch,
  first,
}: {
  thread: TodayThread;
  canLaunch: boolean;
  first: boolean;
}) {
  const openThread = useUi((s) => s.openThread);
  // Native resume exists only for the agents whose CLI takes a session id.
  const canResume = canLaunch && (thread.source === "claude_code" || thread.source === "codex");
  const resume = useMutation({
    mutationFn: () => api.resumeThread(thread.id),
    onError: (e) => notify.error(`Couldn't resume the session: ${errorText(e)}`),
  });
  const title = thread.title || "Untitled session";
  const source = SOURCE_LABELS[thread.source] ?? thread.source;
  return (
    <ListRow first={first}>
      <Item size="sm" className={cn(ROW_STRETCHED, "gap-y-2")}>
        <ItemContent className="min-w-0 gap-0.5">
          <ItemTitle className="block w-full">
            <button
              type="button"
              data-today-row
              data-stretch
              onClick={() => openThread(thread.id)}
              // A cut-off name can be read on hover.
              title={title}
              className={cn(STRETCH, "block max-w-full truncate")}
            >
              {title}
            </button>
          </ItemTitle>
          <ItemDescription className={ROW_META}>
            {source} · {formatDuration(thread.durationSecs)} · {plural(thread.messages, "message")}{" "}
            · {formatTime(thread.lastActive)}
          </ItemDescription>
        </ItemContent>
        {canResume && (
          <ItemActions className={ABOVE}>
            <Button
              size="xs"
              variant="outline"
              // Stays focusable while the terminal opens: a disabled button drops focus to
              // the page body, and arrow keys then have no row to move from.
              focusableWhenDisabled
              disabled={resume.isPending}
              className="aria-disabled:opacity-50"
              onClick={() => resume.mutate()}
              title={`Resume this session in ${source}`}
              aria-label={`Resume "${title}" in ${source}`}
            >
              {resume.isPending ? (
                <>
                  <Spinner data-icon="inline-start" className="size-3" />
                  Resuming…
                </>
              ) : (
                <>
                  Resume
                  <ArrowUpRight data-icon="inline-end" />
                </>
              )}
            </Button>
          </ItemActions>
        )}
        {thread.commits.length > 0 && (
          // One line each: a sha badge is as wide as its text and only the subject gives
          // way, so the two cannot meet however long either is.
          <ItemFooter className={cn(ABOVE, "block min-w-0")}>
            <ul aria-label={`Commits from "${title}"`} className="space-y-1">
              {thread.commits.map((c) => (
                <li key={c.sha} className="flex min-w-0 items-center gap-2 text-[13px]">
                  <CommitSha
                    sha={c.sha}
                    shortSha={c.shortSha}
                    label={`Copy commit ${c.shortSha}${c.subject ? `: ${c.subject}` : ""}`}
                  />
                  <span
                    className="min-w-0 truncate text-muted-foreground"
                    title={c.subject ?? undefined}
                  >
                    {c.subject || "(no subject)"}
                  </span>
                </li>
              ))}
            </ul>
          </ItemFooter>
        )}
      </Item>
    </ListRow>
  );
}
