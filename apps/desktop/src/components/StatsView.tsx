import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { api, SOURCE_LABELS, type SourceKind, type ValueLedger } from "../lib/api";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Spinner } from "@/components/ui/spinner";
import { formatUsd, shortPath } from "../lib/format";
import { AGGREGATE_STALE_MS } from "../lib/queryClient";
import { errorText, notify } from "../lib/toast";
import { cn } from "@/lib/utils";
import { Loading } from "./Loading";

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

/** Retention "Keep for a year" sets: long enough that the sweep rarely reaches a session
 *  someone still looks for, and a round number the confirm can name. (Claude Code documents
 *  only a minimum, 1 day.) */
const KEEP_DAYS = 365;

const plural = (n: number, one: string, many = `${one}s`) =>
  `${n.toLocaleString()} ${n === 1 ? one : many}`;

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
  const weekTotal = week.hook + week.digest + week.recall + week.mcp;
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
        <dl className="grid grid-cols-2 gap-px overflow-hidden rounded-lg border bg-border sm:grid-cols-4">
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

/** Estimated AI spend by model + the priciest threads, from captured token usage. */
function SpendCard() {
  const { data } = useQuery({
    queryKey: ["spend"],
    queryFn: () => api.spend(),
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
            No token usage captured yet. Hit{" "}
            <span className="font-medium text-foreground">Reindex</span> to read it from your source
            files. Then this shows your estimated AI spend by model and your priciest threads.
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
        <p className="text-[0.7rem] text-muted-foreground">
          Estimate from list prices, not a billing record.
          {data.untrackedCalls > 0
            ? ` ${data.untrackedCalls.toLocaleString()} calls on unpriced models not counted.`
            : ""}
        </p>
      </CardContent>
    </Card>
  );
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
