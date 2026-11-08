import { useQuery } from "@tanstack/react-query";
import { api, type CommitCost, type Spend } from "../../lib/api";
import { formatSince, formatUsd } from "../../lib/format";
import { AGGREGATE_STALE_MS } from "../../lib/queryClient";
import { errorText } from "../../lib/toast";
import { SectionNote, SectionSkeleton } from "./ProjectSection";
import { useOpenThread } from "./shared";

const DAY = 86_400;

/** Midnight UTC 30 days back. Rounded to the day so the spend cache (keyed on `since`)
 *  serves every visit that day instead of missing on a since that moves each second. */
function thirtyDaysAgo(): number {
  const today = Math.floor(Date.now() / 1000 / DAY) * DAY;
  return today - 30 * DAY;
}

/** Estimated spend in this repo from captured token usage: the last 30 days next to all
 *  time, the models behind it, the priciest sessions and, once commits are linked, what
 *  the sessions behind each commit cost. */
export function SpendSection({ project }: { project: string }) {
  const since = thirtyDaysAgo();
  const recent = useQuery({
    queryKey: ["spend", project, since],
    queryFn: () => api.spend(project, since),
    staleTime: AGGREGATE_STALE_MS,
  });
  const allTime = useQuery({
    queryKey: ["spend", project],
    queryFn: () => api.spend(project),
    staleTime: AGGREGATE_STALE_MS,
  });
  const breakdown = useQuery({
    queryKey: ["spend_breakdown", project],
    queryFn: () => api.spendBreakdown(project),
    staleTime: AGGREGATE_STALE_MS,
  });
  const openThread = useOpenThread();

  if (recent.isPending || allTime.isPending) return <SectionSkeleton rows={2} />;
  const failed = recent.error ?? allTime.error;
  if (failed || !recent.data || !allTime.data) {
    return <SectionNote tone="error">Couldn't load the spend: {errorText(failed)}</SectionNote>;
  }
  const all = allTime.data;
  if (all.trackedCalls === 0 && all.untrackedCalls === 0) {
    return (
      <SectionNote>
        No token usage recorded for this repo's sessions. Claude Code, Codex, Gemini CLI, Qwen Code,
        OpenCode and Copilot record it; other agents do not. Sessions indexed before this update get
        theirs once, in the background, soon after the app starts.
      </SectionNote>
    );
  }
  const priced = all.byModel.filter((m) => m.priced);

  return (
    <div className="space-y-3">
      <dl className="grid grid-cols-2 gap-px overflow-hidden rounded-xl border bg-border">
        <Figure label="Last 30 days" spend={recent.data} />
        <Figure label="All time" spend={all} />
      </dl>
      <div className="grid gap-3 @min-[40rem]/page:grid-cols-2">
        {priced.length > 0 && (
          <div>
            <h3 className="mb-1.5 px-1 text-[13px] font-medium text-muted-foreground">
              By model, all time
            </h3>
            <ul className="divide-y divide-border overflow-hidden rounded-xl border bg-card">
              {priced.slice(0, 5).map((m) => (
                <li key={m.model} className="flex items-baseline gap-2 px-3 py-2 text-sm">
                  <span className="min-w-0 flex-1 truncate font-mono text-xs">{m.model}</span>
                  <span className="shrink-0 tabular-nums">{formatUsd(m.cost)}</span>
                  <span className="w-20 shrink-0 text-right text-xs tabular-nums text-muted-foreground">
                    {m.calls.toLocaleString()} calls
                  </span>
                </li>
              ))}
            </ul>
          </div>
        )}
        {all.topThreads.length > 0 && (
          <div>
            <h3 className="mb-1.5 px-1 text-[13px] font-medium text-muted-foreground">
              Priciest sessions
            </h3>
            <ul className="divide-y divide-border overflow-hidden rounded-xl border bg-card">
              {all.topThreads.slice(0, 5).map((t) => (
                <li key={t.threadId}>
                  <button
                    type="button"
                    onClick={() => openThread(t.threadId)}
                    className="flex w-full cursor-pointer items-baseline gap-2 px-3 py-2 text-left text-sm outline-none transition-colors hover:bg-muted/40 focus-visible:bg-muted/40 focus-visible:ring-[3px] focus-visible:ring-inset focus-visible:ring-ring/50"
                  >
                    <span className="min-w-0 flex-1 truncate">
                      {t.title || `Session #${t.threadId}`}
                    </span>
                    <span className="shrink-0 tabular-nums">{formatUsd(t.cost)}</span>
                  </button>
                </li>
              ))}
            </ul>
          </div>
        )}
      </div>
      {breakdown.data && breakdown.data.commits.length > 0 && (
        <div>
          <h3 className="mb-1.5 px-1 text-[13px] font-medium text-muted-foreground">
            Priciest commits
          </h3>
          <CommitSpendList commits={breakdown.data.commits} />
        </div>
      )}
      <p className="px-1 text-xs text-muted-foreground">
        List-price equivalent of the recorded usage, not a billing record; a subscription bills
        differently.
        {all.untrackedCalls > 0
          ? ` ${all.untrackedCalls.toLocaleString()} calls on unpriced models are not counted.`
          : ""}
      </p>
    </div>
  );
}

function Figure({ label, spend }: { label: string; spend: Spend }) {
  return (
    <div className="bg-card px-4 py-3">
      <dt className="text-[13px] text-muted-foreground">{label}</dt>
      <dd className="mt-0.5 text-lg font-semibold tabular-nums">
        {formatUsd(spend.totalCost)}
        <span className="ml-1.5 text-[13px] font-normal text-muted-foreground">
          {spend.trackedCalls.toLocaleString()} calls
        </span>
      </dd>
    </div>
  );
}

/** Commits with what the sessions behind them cost; a row opens its priciest session. A
 *  session behind several commits counts toward each, which the row says. */
export function CommitSpendList({ commits }: { commits: CommitCost[] }) {
  const openThread = useOpenThread();
  return (
    <ul className="divide-y divide-border overflow-hidden rounded-xl border bg-card">
      {commits.map((c) => {
        const session = c.sessions[0];
        const sessions = c.sessions.length;
        const detail = [
          formatSince(c.committedAt),
          sessions === 1 ? "1 session" : `${sessions} sessions`,
          c.shared ? "shared with other commits" : null,
        ]
          .filter(Boolean)
          .join(" · ");
        return (
          <li key={c.sha}>
            <button
              type="button"
              disabled={!session}
              onClick={() => session && openThread(session.id)}
              title={session ? `Open ${session.title || `session #${session.id}`}` : undefined}
              className="flex w-full cursor-pointer items-baseline gap-2 px-3 py-2 text-left text-sm outline-none transition-colors hover:bg-muted/40 focus-visible:bg-muted/40 focus-visible:ring-[3px] focus-visible:ring-inset focus-visible:ring-ring/50 disabled:cursor-default"
            >
              <span className="shrink-0 font-mono text-xs text-muted-foreground">{c.shortSha}</span>
              <span className="min-w-0 flex-1">
                <span className="block truncate">{c.subject || c.shortSha}</span>
                <span className="block truncate text-xs text-muted-foreground">{detail}</span>
              </span>
              <span className="shrink-0 tabular-nums">{formatUsd(c.cost)}</span>
            </button>
          </li>
        );
      })}
    </ul>
  );
}
