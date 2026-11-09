import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { type KeyboardEvent, useRef, useState } from "react";
import { ChevronRight } from "lucide-react";
import { api, type DayCost, SOURCE_LABELS, type SourceKind, type ValueLedger } from "../lib/api";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Spinner } from "@/components/ui/spinner";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import { formatUsd, shortPath } from "../lib/format";
import { AGGREGATE_STALE_MS } from "../lib/queryClient";
import { errorText, notify } from "../lib/toast";
import { useUi } from "../store/ui";
import { cn } from "@/lib/utils";
import { Loading } from "./Loading";
import { CommitSpendList } from "./project/SpendSection";

export function StatsView() {
  const { data, isLoading, isError, error } = useQuery({
    queryKey: ["index_stats"],
    queryFn: api.indexStats,
    // ~1s of SQL; refreshed by index/embed events instead of on every visit.
    staleTime: AGGREGATE_STALE_MS,
  });

  if (isLoading) return <Loading label="Crunching the index…" className="h-full" />;
  if (isError || !data) return <Centered>Couldn't load stats. {String(error ?? "")}</Centered>;

  const coverage = data.embeddable > 0 ? data.embedded / data.embeddable : 0;
  const maxSrcMessages = Math.max(1, ...data.perSource.map((s) => s.messages));
  const totalRoleMessages = Math.max(
    1,
    data.perRole.reduce((n, r) => n + r.messages, 0),
  );

  return (
    <div className="mx-auto w-full max-w-4xl space-y-6 overflow-y-auto p-6">
      <ReportLink />
      {/* One row of label/value pairs, like the Settings rows, rather than hero tiles. It
          wraps to two columns below sm (the window has no minimum width); the 1px gaps over
          the border color draw the hairlines in either layout. */}
      <dl className="grid grid-cols-2 gap-px overflow-hidden rounded-lg border bg-border sm:grid-cols-[repeat(3,minmax(0,1fr))_minmax(0,1.6fr)]">
        <Stat label="Threads" value={data.threads.toLocaleString()} />
        <Stat label="Messages" value={data.messages.toLocaleString()} />
        <Stat
          label="Semantic coverage"
          value={`${Math.round(coverage * 100)}%`}
          detail={`${data.embedded.toLocaleString()} of ${data.embeddable.toLocaleString()}`}
          title={`${Math.round(coverage * 100)}%: ${data.embedded.toLocaleString()} of ${data.embeddable.toLocaleString()} messages embedded`}
        />
        <Stat label="Span" value={spanLabel(data.earliest, data.latest)} />
      </dl>

      <ActivityCard />

      <ValueCard />

      <Card>
        <CardHeader>
          <CardTitle>By source</CardTitle>
        </CardHeader>
        <CardContent className="space-y-2">
          {data.perSource.map((s) => (
            <div key={s.kind} className="space-y-1">
              <div className="flex items-baseline justify-between text-sm">
                <span>{SOURCE_LABELS[s.kind as SourceKind] ?? s.kind}</span>
                <span className="text-muted-foreground">
                  {s.threads.toLocaleString()} threads · {s.messages.toLocaleString()} msgs
                </span>
              </div>
              <div className="h-1.5 overflow-hidden rounded-full bg-muted">
                <div
                  className="h-full rounded-full bg-primary"
                  style={{ width: `${(s.messages / maxSrcMessages) * 100}%` }}
                />
              </div>
            </div>
          ))}
        </CardContent>
      </Card>

      <SpendCard />

      <div className="grid gap-6 md:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle>By role</CardTitle>
          </CardHeader>
          <CardContent className="space-y-2">
            {data.perRole.map((r) => (
              <div key={r.role} className="flex items-center justify-between text-sm">
                <span className="capitalize">{r.role}</span>
                <span className="text-muted-foreground">
                  {r.messages.toLocaleString()} ·{" "}
                  {Math.round((r.messages / totalRoleMessages) * 100)}%
                </span>
              </div>
            ))}
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>Top projects</CardTitle>
          </CardHeader>
          <CardContent className="space-y-2">
            {data.topProjects.length === 0 && (
              <p className="text-sm text-muted-foreground">No project paths recorded.</p>
            )}
            {data.topProjects.map((p) => (
              <div key={p.project} className="flex items-center justify-between gap-2 text-sm">
                <span className="truncate" title={p.project}>
                  {shortPath(p.project)}
                </span>
                <span className="shrink-0 text-muted-foreground">{p.threads.toLocaleString()}</span>
              </div>
            ))}
          </CardContent>
        </Card>
      </div>
    </div>
  );
}

const HEATMAP_DAYS = 364; // 52 weeks, inclusive of today

/** GitHub-style intensity, 0..4, scaled to the busiest day so quiet histories still show. */
function level(count: number, max: number): number {
  if (count <= 0) return 0;
  const r = count / Math.max(max, 1);
  if (r > 0.66) return 4;
  if (r > 0.33) return 3;
  if (r > 0.12) return 2;
  return 1;
}

// Brand sunset (#C16E2C) at rising opacity reads as intensity on both themes; level 0 is a
// faint track. Kept off the Tailwind palette on purpose: this ramp is the heatmap's signature.
const CELL_BG = [
  "color-mix(in oklab, var(--muted-foreground) 14%, transparent)",
  "rgba(193, 110, 44, 0.3)",
  "rgba(193, 110, 44, 0.5)",
  "rgba(193, 110, 44, 0.72)",
  "rgba(193, 110, 44, 0.95)",
];

// Days are local calendar days, as search::coach_heatmap buckets them ("YYYY-MM-DD" in the
// machine's zone), so the cell label, the Search chip and the range a click opens all name
// the same day. (UTC buckets put evening work west of Greenwich on the next day's cell.)
const DATE_FMT: Intl.DateTimeFormatOptions = { weekday: "short", month: "short", day: "numeric" };

/** The local day `offset` days from `today` (0 = today). Calendar arithmetic, not 86,400 s
 *  steps, so a DST change never skips or repeats a day. */
function dayAt(today: Date, offset: number): Date {
  return new Date(today.getFullYear(), today.getMonth(), today.getDate() + offset);
}

/** A local date as the backend's day key, "YYYY-MM-DD". */
function dayKey(d: Date): string {
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

// Arrow keys move like a calendar: a column is a week, so left and right step a week.
const STEP: Record<string, number> = {
  ArrowUp: -1,
  ArrowDown: 1,
  ArrowLeft: -7,
  ArrowRight: 7,
};

/**
 * Daily activity over the last 52 weeks (moved here from the old Coach tab). A day with
 * activity opens Search scoped to it, listing that day's threads. The grid is one Tab stop:
 * the arrow keys move between days (Home / End: first day, today) and Enter opens one,
 * rather than 364 stops in the page's Tab order.
 */
function ActivityCard() {
  const openSearch = useUi((s) => s.openSearch);
  const { data, isError, error, refetch, isFetching } = useQuery({
    queryKey: ["coach_overview"],
    queryFn: api.coachOverview,
    staleTime: AGGREGATE_STALE_MS, // refreshed by index events (main.tsx)
  });
  // Days are offsets from today (0), negative into the past.
  const today = new Date();
  const [focusDay, setFocusDay] = useState(0);
  const grid = useRef<HTMLDivElement>(null);
  // A cold cache is a GROUP BY over every message (seconds on a large index), and a failure
  // left the card silently missing: the card keeps its place and says which it is.
  if (!data) {
    return (
      <Card>
        <CardHeader>
          <CardTitle>Activity</CardTitle>
          <CardDescription>Pick a day to see its threads in Search.</CardDescription>
        </CardHeader>
        <CardContent>
          {isError ? (
            <div role="alert" className="flex flex-wrap items-center gap-2 text-sm">
              <span className="text-destructive">
                Couldn't load the activity: {errorText(error)}
              </span>
              <Button
                size="xs"
                variant="outline"
                disabled={isFetching}
                onClick={() => void refetch()}
              >
                Try again
              </Button>
            </div>
          ) : (
            // The grid's own shape (53 week columns, 7 days), so nothing jumps when it lands.
            <div
              role="status"
              aria-label="Loading the activity"
              className="aspect-[53/7] w-full animate-pulse rounded-md bg-muted"
            />
          )}
        </CardContent>
      </Card>
    );
  }

  const startDay = -HEATMAP_DAYS;
  // Pad the start back to the beginning of its week (Sunday) so columns align like a calendar.
  const gridStart = startDay - dayAt(today, startDay).getDay();
  // Columns = the index of today's column + 1 (today sits at day-offset / 7). No trailing
  // all-empty column.
  const weekCount = Math.floor((0 - gridStart) / 7) + 1;

  const byDay = new Map(data.heatmap.map((d) => [d.day, d]));
  const maxDay = data.heatmap.reduce((m, d) => Math.max(m, d.messages), 0);
  const totalMessages = data.heatmap.reduce((n, d) => n + d.messages, 0);
  const activeDays = data.heatmap.filter((d) => d.messages > 0).length;
  const current = Math.min(0, Math.max(startDay, focusDay));

  const open = (offset: number) => {
    const day = byDay.get(dayKey(dayAt(today, offset)));
    if (!day || day.messages <= 0) return;
    // The backend's own bounds for the local day (inclusive, also on a 23 or 25 hour DST
    // day), and with both bounds recent_threads lists the threads with a message inside, so
    // the list is the threads behind the cell's count. The cell counts every repo and
    // source, top-level sessions only, so the filters that would hide some of them reset.
    openSearch({
      query: "",
      project: null,
      sources: [],
      starred: null,
      tags: [],
      includeSubagents: false,
      after: day.start,
      before: day.end,
    });
  };
  const moveTo = (offset: number) => {
    const next = Math.min(0, Math.max(startDay, offset));
    setFocusDay(next);
    grid.current?.querySelector<HTMLElement>(`[data-day="${next}"]`)?.focus();
  };
  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    const step = STEP[e.key];
    if (step) moveTo(current + step);
    else if (e.key === "Home") moveTo(startDay);
    else if (e.key === "End") moveTo(0);
    else return;
    e.preventDefault();
  };

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex flex-wrap items-baseline justify-between gap-x-3 gap-y-1">
          <span>Activity</span>
          <span className="text-[13px] font-normal text-muted-foreground tabular-nums">
            {totalMessages.toLocaleString()} messages · {activeDays} active days in the last 52
            weeks
          </span>
        </CardTitle>
        <CardDescription>Pick a day to see its threads in Search.</CardDescription>
      </CardHeader>
      <CardContent>
        <TooltipProvider delay={80}>
          {/* biome-ignore lint/a11y/useSemanticElements: a fieldset would draw a form group around a chart; this only names the roving set of day buttons */}
          <div
            ref={grid}
            role="group"
            aria-label="Messages per day, last 52 weeks"
            className="flex w-full gap-0.5"
            onKeyDown={onKeyDown}
          >
            {Array.from({ length: weekCount }, (_, w) => gridStart + w * 7).map((weekStart) => (
              <div key={weekStart} className="flex flex-1 flex-col gap-0.5">
                {Array.from({ length: 7 }, (_, d) => {
                  const day = weekStart + d;
                  if (day < startDay || day > 0) {
                    return <div key={day} className="aspect-square" />;
                  }
                  const date = dayAt(today, day);
                  const c = byDay.get(dayKey(date))?.messages ?? 0;
                  const label = `${date.toLocaleDateString(undefined, DATE_FMT)}: ${c.toLocaleString()} message${c === 1 ? "" : "s"}`;
                  return (
                    <Tooltip key={day}>
                      <TooltipTrigger
                        render={
                          <button
                            type="button"
                            data-day={day}
                            tabIndex={day === current ? 0 : -1}
                            aria-label={label}
                            aria-disabled={c === 0 || undefined}
                            onClick={() => open(day)}
                            onFocus={() => setFocusDay(day)}
                            className={cn(
                              "aspect-square rounded-xs outline-none",
                              "focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-1 focus-visible:ring-offset-card",
                              c > 0
                                ? "cursor-pointer hover:ring-1 hover:ring-ring/60"
                                : "cursor-default",
                            )}
                            style={{ backgroundColor: CELL_BG[level(c, maxDay)] }}
                          />
                        }
                      />
                      {/* Visual only: the button's aria-label already says this, so a
                            screen reader announces each day once. */}
                      <TooltipContent aria-hidden>{label}</TooltipContent>
                    </Tooltip>
                  );
                })}
              </div>
            ))}
          </div>
        </TooltipProvider>
        <div
          className="mt-2 flex items-center gap-1.5 text-[0.7rem] text-muted-foreground"
          aria-hidden="true"
        >
          <span>Less</span>
          {CELL_BG.map((bg) => (
            <span key={bg} className="size-2.5 rounded-xs" style={{ backgroundColor: bg }} />
          ))}
          <span>More</span>
        </div>
      </CardContent>
    </Card>
  );
}

/** Retention "Keep for a year" sets: long enough that the sweep rarely reaches a session
 *  someone still looks for, and a round number the confirm can name. (Claude Code documents
 *  only a minimum, 1 day.) */
const KEEP_DAYS = 365;

const plural = (n: number, one: string, many = `${one}s`) =>
  `${n.toLocaleString()} ${n === 1 ? one : many}`;

/** The way into the Report view ("Your agents, so far") after Today's card is gone. */
function ReportLink() {
  const setView = useUi((s) => s.setView);
  return (
    <button
      type="button"
      onClick={() => setView("report")}
      className="flex w-full cursor-pointer items-center gap-3 rounded-lg border px-4 py-3 text-left outline-none transition-colors hover:bg-muted/40 focus-visible:ring-3 focus-visible:ring-ring/50"
    >
      <span className="min-w-0 flex-1">
        <span className="block text-sm font-medium">Your agents, so far</span>
        <span className="block truncate text-[13px] text-muted-foreground">
          Time, spend, recurring errors and memory across every session, ready to share.
        </span>
      </span>
      <ChevronRight className="size-4 shrink-0 text-muted-foreground" aria-hidden="true" />
    </button>
  );
}

/** What the hooks and the MCP server did this week (the local ledger), and the Claude Code
 *  sessions the index still holds after Claude Code deleted their transcripts. */
function ValueCard() {
  const { data } = useQuery({
    queryKey: ["value_ledger"],
    queryFn: api.valueLedger,
    // One stat per Claude Code session; the counts move by the minute at most.
    staleTime: 60_000,
  });
  if (!data) return null;

  const { week, allTime } = data;
  const weekTotal = week.hook + week.digest + week.recall + week.playbook + week.mcp;
  return (
    <Card>
      <CardHeader>
        <CardTitle>What Callimachus did for your agents this week</CardTitle>
        <CardDescription>
          {allTime === 0
            ? "Nothing yet. Counts start when an agent opens a session with the hooks or the MCP server connected (Settings > Agents)."
            : `Helped ${plural(weekTotal, "time")} in the last 7 days, ${allTime.toLocaleString()} in all. Counted on this machine only, never sent anywhere.`}
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-5">
        <dl className="grid grid-cols-2 gap-px overflow-hidden rounded-lg border bg-border sm:grid-cols-5">
          <Stat
            label="Memory injected"
            value={week.hook.toLocaleString()}
            title="Sessions that started with this repo's decisions and gotchas"
          />
          <Stat
            label="Session digests"
            value={week.digest.toLocaleString()}
            title="Sessions in a repo with no memory yet that started with its recent sessions"
          />
          <Stat
            label="Recall hits"
            value={week.recall.toLocaleString()}
            title="Prompts where related past work was surfaced before the agent redid it"
          />
          <Stat
            label="Error playbooks"
            value={week.playbook.toLocaleString()}
            title="Failed shell commands where the agent was told how the same error was fixed before"
          />
          <Stat
            label="MCP tool calls"
            value={week.mcp.toLocaleString()}
            title="Searches, memory lookups and thread reads answered for an agent"
          />
        </dl>
        <RetentionGuard preserved={data.preserved} />
      </CardContent>
    </Card>
  );
}

/** "N sessions preserved that Claude Code deleted", the retention behind it, and a
 *  reversible "keep for a year" that edits Claude Code's settings.json after a confirm. */
function RetentionGuard({ preserved }: { preserved: ValueLedger["preserved"] }) {
  const queryClient = useQueryClient();
  const [confirming, setConfirming] = useState<"keep" | "restore" | null>(null);
  const restoreTo = preserved.previousCleanupDays;
  const restoreLabel =
    restoreTo === null ? "Claude Code's 30-day default" : plural(restoreTo, "day");

  const change = useMutation({
    mutationFn: (days: number | null) => api.setClaudeCleanupDays(days),
    onSuccess: (_, days) => {
      setConfirming(null);
      queryClient.invalidateQueries({ queryKey: ["value_ledger"] });
      notify.success(
        // Names the edit, not the outcome: managed settings (an organization's policy) outrank
        // the user's settings.json, and this card reads only the user file.
        days === KEEP_DAYS
          ? `Set cleanupPeriodDays to ${KEEP_DAYS} in your Claude Code settings.json`
          : days === null
            ? "Removed cleanupPeriodDays from your Claude Code settings.json (30-day default)"
            : `Set cleanupPeriodDays back to ${days} in your Claude Code settings.json`,
      );
    },
    onError: (e) => {
      setConfirming(null);
      notify.error("Couldn't change Claude Code's retention", {
        description: `${errorText(e)}. Fix settings.json, or set cleanupPeriodDays there yourself.`,
      });
    },
  });

  const retention =
    preserved.cleanupPeriodDays === null
      ? `Claude Code deletes transcripts after ${plural(preserved.effectiveDays, "day")} by default; your settings.json does not change that.`
      : `Your settings.json has Claude Code delete transcripts after ${plural(preserved.effectiveDays, "day")} (cleanupPeriodDays).`;

  let action: React.ReactNode = null;
  if (confirming) {
    const keep = confirming === "keep";
    action = (
      <div className="flex flex-wrap items-center gap-2">
        <span className="text-[13px] text-muted-foreground">
          {keep
            ? `Set cleanupPeriodDays to ${KEEP_DAYS} in Claude Code's settings.json?`
            : restoreTo === null
              ? "Remove cleanupPeriodDays from Claude Code's settings.json?"
              : `Set cleanupPeriodDays back to ${restoreTo}?`}
        </span>
        {!change.isPending && (
          <Button size="xs" variant="ghost" type="button" onClick={() => setConfirming(null)}>
            Cancel
          </Button>
        )}
        <Button
          size="xs"
          type="button"
          disabled={change.isPending}
          onClick={() => change.mutate(keep ? KEEP_DAYS : restoreTo)}
        >
          {change.isPending && <Spinner data-icon="inline-start" />}
          {keep ? "Keep for a year" : "Restore"}
        </Button>
      </div>
    );
  } else if (preserved.canRestore && preserved.cleanupPeriodDays === KEEP_DAYS) {
    action = (
      <Button size="sm" variant="ghost" type="button" onClick={() => setConfirming("restore")}>
        Restore {restoreLabel}
      </Button>
    );
  } else if (preserved.effectiveDays < KEEP_DAYS) {
    action = (
      <Button size="sm" variant="outline" type="button" onClick={() => setConfirming("keep")}>
        Keep transcripts for a year
      </Button>
    );
  }

  return (
    <div className="flex flex-wrap items-center justify-between gap-3 border-t pt-4">
      <div className="min-w-0 space-y-0.5">
        <p className="text-sm font-medium">
          {preserved.threads > 0
            ? `${plural(preserved.threads, "session")} preserved that Claude Code deleted`
            : "No indexed Claude Code session has been deleted yet"}
        </p>
        <p className="text-[13px] text-muted-foreground">
          {retention} Managed settings from your organization, if any, take precedence. Callimachus
          keeps its indexed copy either way.
        </p>
      </div>
      {action}
    </div>
  );
}

/** Estimated AI spend by model + the priciest threads, from captured token usage, with the
 *  last 30 days and the priciest commits once the breakdown loads. */
function SpendCard() {
  const { data } = useQuery({
    queryKey: ["spend"],
    queryFn: () => api.spend(),
    staleTime: AGGREGATE_STALE_MS,
  });
  const breakdown = useQuery({
    queryKey: ["spend_breakdown"],
    queryFn: () => api.spendBreakdown(),
    staleTime: AGGREGATE_STALE_MS,
  });
  if (!data) return null;

  if (data.trackedCalls === 0 && data.untrackedCalls === 0) {
    return (
      <Card>
        <CardHeader>
          <CardTitle>Spend</CardTitle>
        </CardHeader>
        <CardContent>
          <p className="text-sm text-muted-foreground">
            No token usage recorded yet. Claude Code, Codex, Gemini CLI, Qwen Code, OpenCode and
            Copilot record it; other agents do not. Sessions indexed before this update get theirs
            once, in the background, soon after the app starts. Then this shows your estimated AI
            spend by model and your priciest threads.
          </p>
        </CardContent>
      </Card>
    );
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-baseline justify-between">
          <span>Spend (estimate)</span>
          <span className="text-2xl font-semibold tabular-nums">{formatUsd(data.totalCost)}</span>
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-4">
        <div className="space-y-1.5">
          {data.byModel
            .filter((m) => m.priced)
            .map((m) => (
              <div key={m.model} className="flex items-baseline justify-between gap-2 text-sm">
                <span className="truncate">{m.model}</span>
                <span className="shrink-0 tabular-nums text-muted-foreground">
                  {formatUsd(m.cost)} · {m.calls.toLocaleString()} calls
                </span>
              </div>
            ))}
        </div>
        {breakdown.data?.days.some((d) => d.cost > 0) && <SpendByDay days={breakdown.data.days} />}
        {data.topThreads.length > 0 && (
          <div>
            <div className="mb-1 text-xs uppercase tracking-wide text-muted-foreground">
              Most expensive threads
            </div>
            <div className="space-y-1">
              {data.topThreads.slice(0, 5).map((t) => (
                <div key={t.threadId} className="flex items-baseline justify-between gap-2 text-sm">
                  <span className="truncate" title={t.title ?? undefined}>
                    {t.title || `Thread #${t.threadId}`}
                  </span>
                  <span className="shrink-0 tabular-nums text-muted-foreground">
                    {formatUsd(t.cost)}
                  </span>
                </div>
              ))}
            </div>
          </div>
        )}
        {breakdown.data && breakdown.data.commits.length > 0 && (
          <div>
            <div className="mb-1 text-xs uppercase tracking-wide text-muted-foreground">
              Priciest commits
            </div>
            <CommitSpendList commits={breakdown.data.commits} />
          </div>
        )}
        <p className="text-[0.7rem] text-muted-foreground">
          List-price equivalent of the recorded usage, not a billing record; a subscription bills
          differently.
          {data.untrackedCalls > 0
            ? ` ${data.untrackedCalls.toLocaleString()} calls on unpriced models not counted.`
            : ""}
        </p>
      </CardContent>
    </Card>
  );
}

/** The last 30 local days as thin columns on a shared baseline, one tooltip per day. One
 *  series, so no legend: the heading names it, and the peak day is stated in text. */
function SpendByDay({ days }: { days: DayCost[] }) {
  const max = Math.max(...days.map((d) => d.cost));
  const total = days.reduce((n, d) => n + d.cost, 0);
  const peak = days.reduce((a, b) => (b.cost > a.cost ? b : a), days[0]);
  const label = (d: DayCost) => `${dayLabel(d.day)}: ${formatUsd(d.cost)}`;
  return (
    <div>
      <div className="mb-1.5 flex items-baseline justify-between gap-2">
        <span className="text-xs uppercase tracking-wide text-muted-foreground">Last 30 days</span>
        <span className="text-xs tabular-nums text-muted-foreground">
          {formatUsd(total)} · peak {formatUsd(peak.cost)} on {dayLabel(peak.day)}
        </span>
      </div>
      <TooltipProvider delay={100}>
        <ol className="flex h-16 items-end gap-0.5" aria-label="Estimated spend per day">
          {days.map((d) => (
            <Tooltip key={d.day}>
              <TooltipTrigger
                render={
                  <li className="flex h-full min-w-0 flex-1 items-end">
                    <span className="sr-only">{label(d)}</span>
                    <span
                      aria-hidden
                      className={cn(
                        "w-full rounded-t-[3px]",
                        d.cost > 0 ? "bg-primary hover:bg-primary/80" : "bg-muted",
                      )}
                      style={{
                        height: d.cost > 0 ? `${Math.max(4, (d.cost / max) * 100)}%` : "2px",
                      }}
                    />
                  </li>
                }
              />
              <TooltipContent aria-hidden>{label(d)}</TooltipContent>
            </Tooltip>
          ))}
        </ol>
      </TooltipProvider>
    </div>
  );
}

/** "Nov 7" for a local `YYYY-MM-DD`. */
function dayLabel(day: string): string {
  const [y, m, d] = day.split("-").map(Number);
  return new Date(y, m - 1, d).toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

/** A label/value cell. `title` (the value by default) keeps a truncated value readable;
 *  `detail` sits inline after the value as a muted suffix, so it never needs a hover. */
function Stat({
  label,
  value,
  detail,
  title = value,
  className,
}: {
  label: string;
  value: string;
  detail?: string;
  title?: string;
  className?: string;
}) {
  return (
    <div className={cn("min-w-0 bg-background px-4 py-3", className)} title={title}>
      <dt className="truncate text-[13px] text-muted-foreground">{label}</dt>
      <dd className="mt-0.5 truncate text-base font-semibold tabular-nums">
        {value}
        {detail && (
          <span className="ml-1.5 text-[13px] font-normal text-muted-foreground">{detail}</span>
        )}
      </dd>
    </div>
  );
}

const SPAN_DATE: Intl.DateTimeFormatOptions = { year: "numeric", month: "short", day: "numeric" };

/** "Oct 30, 2025 to today": whole dates, since a clock time says nothing about a span. */
function spanLabel(earliest: number | null, latest: number | null): string {
  if (!earliest) return "No history yet";
  const day = (s: number) => new Date(s * 1000).toDateString();
  const start = new Date(earliest * 1000).toLocaleDateString(undefined, SPAN_DATE);
  if (!latest) return start;
  const end =
    day(latest) === new Date().toDateString()
      ? "today"
      : new Date(latest * 1000).toLocaleDateString(undefined, SPAN_DATE);
  return start === end ? start : `${start} to ${end}`;
}

function Centered({ children }: { children: React.ReactNode }) {
  return (
    <div className="flex h-full items-center justify-center p-6 text-center text-muted-foreground">
      {children}
    </div>
  );
}
