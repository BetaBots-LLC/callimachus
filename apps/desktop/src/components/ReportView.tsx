import { useEffect, useRef, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import { Copy, ImageDown } from "lucide-react";
import {
  api,
  type OnboardingSteps,
  type Report,
  type ReportMemory,
  type ReportSpend,
  type ReportTotals,
} from "../lib/api";
import { copyText } from "../lib/clipboard";
import { baseName, formatUsd } from "../lib/format";
import { AGGREGATE_STALE_MS } from "../lib/queryClient";
import { errorText, notify } from "../lib/toast";
import { useTheme } from "../store/theme";
import { useUi } from "../store/ui";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Spinner } from "@/components/ui/spinner";
import { cn } from "@/lib/utils";
import { Loading } from "./Loading";
import { CommitSpendList } from "./project/SpendSection";
import { CARD_HEIGHT, CARD_WIDTH, drawReportCard, headline, reportCardPng } from "./report/card";

/**
 * "Your agents, so far": one screen over the whole index (`report.rs`), built from what is
 * already indexed and nothing else. Two share actions: the same markdown `cal report`
 * prints, and a 1200 x 630 PNG card drawn here and saved to Downloads.
 */
export const REPORT_KEY = ["report"] as const;
const ONBOARDING_KEY = ["onboarding_state"] as const;

const DATE: Intl.DateTimeFormatOptions = { year: "numeric", month: "short", day: "numeric" };
const date = (secs: number) => new Date(secs * 1000).toLocaleDateString(undefined, DATE);
const n = (v: number) => v.toLocaleString();
const pct = (share: number) => (share > 0 && share < 0.01 ? "<1%" : `${Math.round(share * 100)}%`);
const plural = (v: number, one: string, many = `${one}s`) => `${n(v)} ${v === 1 ? one : many}`;
const dayLabel = (day: string) => {
  const [y, m, d] = day.split("-").map(Number);
  return new Date(y, m - 1, d).toLocaleDateString(undefined, DATE);
};

export function ReportView() {
  const qc = useQueryClient();
  const theme = useTheme((s) => s.resolved);
  const report = useQuery({
    queryKey: REPORT_KEY,
    queryFn: () => api.report(),
    staleTime: AGGREGATE_STALE_MS,
  });

  // Opening the view finishes the checklist's "See your report" step, once.
  const marked = useRef(false);
  useEffect(() => {
    if (!report.isSuccess || marked.current) return;
    marked.current = true;
    const steps = qc.getQueryData<OnboardingSteps>(ONBOARDING_KEY);
    if (steps?.report === "done") return;
    api
      .setOnboardingStep("report", "done")
      .then((s) => qc.setQueryData(ONBOARDING_KEY, s))
      .catch(() => {}); // an older index without the step: the view still works
  }, [report.isSuccess, qc]);

  const save = useMutation({
    mutationFn: async (r: Report) => api.saveReportCard(await reportCardPng(r, theme)),
    onSuccess: (path) =>
      notify.success("Card saved", {
        description: baseName(path),
        action: {
          label: "Show in folder",
          // The card may have been moved or deleted since: say so rather than do nothing.
          onClick: () =>
            void revealItemInDir(path).catch((e) =>
              notify.error("Couldn't show the card", { description: errorText(e) }),
            ),
        },
      }),
    onError: (e) => notify.error("Couldn't save the card", { description: errorText(e) }),
  });

  if (report.isPending) return <Loading label="Reading your history…" className="h-full" />;
  if (report.isError)
    return (
      <div role="alert" className="mx-auto w-full max-w-3xl space-y-3 p-6 text-sm">
        <p className="text-destructive">Couldn't build the report: {errorText(report.error)}</p>
        <Button size="sm" variant="outline" onClick={() => void report.refetch()}>
          Try again
        </Button>
      </div>
    );

  const { report: r, markdown } = report.data;
  const copy = () =>
    copyText(markdown).then(
      () => notify.success("Report copied as markdown"),
      (e) => notify.error("Couldn't copy the report", { description: errorText(e) }),
    );

  return (
    <div className="h-full overflow-y-auto">
      <div className="mx-auto w-full max-w-4xl space-y-6 p-6">
        <header className="flex flex-wrap items-end gap-x-4 gap-y-3">
          <div className="min-w-0 flex-1 space-y-1">
            <h1 className="text-xl font-semibold tracking-tight text-balance">
              Your agents, so far
            </h1>
            {r.totals && (
              <p className="text-sm text-muted-foreground">
                {date(r.totals.firstSession)} to {date(r.totals.lastSession)}, from this machine's
                index. Nothing here leaves it unless you share it.
              </p>
            )}
          </div>
          {r.totals && (
            <div className="flex shrink-0 gap-2">
              <Button size="sm" variant="outline" onClick={copy}>
                <Copy data-icon="inline-start" /> Copy as markdown
              </Button>
              <Button size="sm" disabled={save.isPending} onClick={() => save.mutate(r)}>
                {save.isPending ? (
                  <Spinner data-icon="inline-start" />
                ) : (
                  <ImageDown data-icon="inline-start" />
                )}
                Save card as PNG
              </Button>
            </div>
          )}
        </header>

        {!r.totals ? (
          <p className="text-sm text-muted-foreground">
            No agent sessions indexed yet. Once the first index finishes, this shows your sessions,
            time, spend, recurring errors and what your agents remember.
          </p>
        ) : (
          <>
            <Headline report={r} totals={r.totals} />
            <ToolsCard totals={r.totals} />
            {r.spend && <SpendCard spend={r.spend} />}
            {r.recurringErrors && <ErrorsCard report={r} />}
            {r.solvedAgain && <SolvedAgainCard report={r} />}
            {r.memory && <MemoryCard memory={r.memory} />}
            {r.files && <FilesCard report={r} />}
            {r.ledger && <LedgerCard report={r} />}
            <SharePreview report={r} />
          </>
        )}
      </div>
    </div>
  );
}

function Headline({ report, totals }: { report: Report; totals: ReportTotals }) {
  const cells: { label: string; value: string; title?: string }[] = [
    { label: "Sessions", value: n(totals.sessions) },
    { label: "Messages", value: n(totals.messages) },
    { label: "Active days", value: n(totals.activeDays) },
  ];
  if (totals.hoursSessions > 0)
    cells.push({
      label: "Hours (estimate)",
      value: n(Math.round(totals.estimatedHours)),
      title: "Each session's span, capped at 8 hours",
    });
  // Most Cursor sessions carry no usable times, so the hours can cover only some agents.
  const partial = totals.hoursSessions > 0 && totals.hoursSessions < totals.sessions;
  const timedAgents = totals.byTool
    .filter((t) => totals.hoursSources.includes(t.source))
    .map((t) => t.label);
  if (report.spend)
    cells.push({
      label: "Spend (estimate)",
      value: formatUsd(report.spend.total.totalCost),
      title: report.spend.basis,
    });
  return (
    <div className="space-y-2">
      <dl
        className={cn(
          "grid grid-cols-2 gap-px overflow-hidden rounded-lg border bg-border",
          cells.length === 5
            ? "sm:grid-cols-5"
            : cells.length === 4
              ? "sm:grid-cols-4"
              : "sm:grid-cols-3",
        )}
      >
        {cells.map((c) => (
          <div key={c.label} className="min-w-0 bg-background px-4 py-3" title={c.title ?? c.value}>
            <dt className="truncate text-[13px] text-muted-foreground">{c.label}</dt>
            <dd className="mt-0.5 truncate text-lg font-semibold tabular-nums">{c.value}</dd>
          </div>
        ))}
      </dl>
      {partial && (
        <p className="text-xs text-muted-foreground">
          Hours cover the {plural(totals.hoursSessions, "session")} with usable timestamps (
          {listJoin(timedAgents)}); the others have none to measure.
        </p>
      )}
    </div>
  );
}

/** `a`, `a and b`, `a, b and c`, as the markdown writes it. */
function listJoin(items: string[]): string {
  if (items.length < 2) return items.join("");
  return `${items.slice(0, -1).join(", ")} and ${items[items.length - 1]}`;
}

function Bar({ share }: { share: number }) {
  return (
    <div className="h-1.5 overflow-hidden rounded-full bg-muted">
      <div
        className="h-full rounded-full bg-primary"
        style={{ width: `${Math.max(share > 0 ? 1 : 0, share * 100)}%` }}
      />
    </div>
  );
}

function ToolsCard({ totals }: { totals: ReportTotals }) {
  return (
    <Card>
      <CardHeader>
        <CardTitle>Sessions by agent</CardTitle>
        <CardDescription>
          Top-level sessions; subagents count toward their session's messages.
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-2">
        {totals.byTool.map((t) => (
          <div key={t.source} className="space-y-1">
            <div className="flex items-baseline justify-between gap-2 text-sm">
              <span className="truncate">{t.label}</span>
              <span className="shrink-0 tabular-nums text-muted-foreground">
                {plural(t.sessions, "session")} · {pct(t.share)}
              </span>
            </div>
            <Bar share={t.share} />
          </div>
        ))}
      </CardContent>
    </Card>
  );
}

function SpendCard({ spend }: { spend: ReportSpend }) {
  const recentOf = (source: string) =>
    spend.last30Days.byTool.find((t) => t.source === source)?.cost ?? 0;
  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-baseline justify-between gap-2">
          <span>Spend (estimate)</span>
          <span className="text-base font-normal tabular-nums text-muted-foreground">
            {formatUsd(spend.last30Days.totalCost)} in the last 30 days
          </span>
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-4">
        {spend.total.byTool.length > 0 && (
          <div className="space-y-1.5">
            {spend.total.byTool.map((t) => (
              <div key={t.source} className="flex items-baseline justify-between gap-2 text-sm">
                <span className="truncate">{t.label}</span>
                <span className="shrink-0 tabular-nums text-muted-foreground">
                  {formatUsd(t.cost)} · {formatUsd(recentOf(t.source))} last 30 days
                </span>
              </div>
            ))}
          </div>
        )}
        {spend.total.byModel.length > 0 && (
          <div>
            <div className="mb-1 text-xs uppercase tracking-wide text-muted-foreground">
              By model
            </div>
            <div className="space-y-1">
              {spend.total.byModel.map((m) => (
                <div key={m.model} className="flex items-baseline justify-between gap-2 text-sm">
                  <span className="truncate">{m.model}</span>
                  <span className="shrink-0 tabular-nums text-muted-foreground">
                    {formatUsd(m.cost)} · {plural(m.calls, "call")}
                  </span>
                </div>
              ))}
            </div>
          </div>
        )}
        {spend.priciestCommits.length > 0 && (
          <div>
            <div className="mb-1 text-xs uppercase tracking-wide text-muted-foreground">
              Priciest commits
            </div>
            <CommitSpendList commits={spend.priciestCommits} />
          </div>
        )}
        <p className="text-[0.7rem] text-muted-foreground">
          {spend.basis}
          {spend.total.untrackedCalls > 0
            ? ` ${plural(spend.total.untrackedCalls, "call")} on unpriced models not counted.`
            : ""}
        </p>
      </CardContent>
    </Card>
  );
}

/** A list row that opens a session in the reader. */
function RowButton({
  onClick,
  title,
  children,
}: {
  onClick?: () => void;
  title?: string;
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      disabled={!onClick}
      onClick={onClick}
      title={title}
      className="flex w-full cursor-pointer flex-col gap-0.5 px-3 py-2 text-left text-sm outline-none transition-colors hover:bg-muted/40 focus-visible:bg-muted/40 focus-visible:ring-[3px] focus-visible:ring-inset focus-visible:ring-ring/50 disabled:cursor-default"
    >
      {children}
    </button>
  );
}

function ErrorsCard({ report }: { report: Report }) {
  const openThread = useUi((s) => s.openThread);
  const errors = report.recurringErrors ?? [];
  return (
    <Card>
      <CardHeader>
        <CardTitle>Recurring errors</CardTitle>
        <CardDescription>
          The errors your agents hit most in the last 180 days, across every project, and the commit
          that fixed each once one is linked. Each line is from tool output, with paths shortened to
          file names.
        </CardDescription>
      </CardHeader>
      <CardContent>
        <ul className="divide-y divide-border overflow-hidden rounded-xl border bg-card">
          {errors.map((e) => (
            <li key={e.example}>
              <RowButton
                onClick={e.fix ? () => openThread(e.fix?.threadId as number) : undefined}
                title={e.fix ? "Open the session that fixed it" : undefined}
              >
                <code className="block truncate font-mono text-xs">{e.example}</code>
                <span className="block truncate text-xs text-muted-foreground">
                  {plural(e.count, "time")} in {plural(e.sessions, "session")}, {date(e.firstSeen)}{" "}
                  to {date(e.lastSeen)}
                  {e.project ? ` · latest in ${baseName(e.project)}` : ""}
                </span>
                <span
                  className={cn(
                    "block truncate text-xs",
                    e.fix ? "text-foreground" : "text-muted-foreground",
                  )}
                >
                  {e.fix
                    ? `Fixed in ${e.fix.shortSha}${e.fix.subject ? `: ${e.fix.subject}` : ""}`
                    : "No linked fix yet"}
                </span>
              </RowButton>
            </li>
          ))}
        </ul>
      </CardContent>
    </Card>
  );
}

function SolvedAgainCard({ report }: { report: Report }) {
  const openThread = useUi((s) => s.openThread);
  return (
    <Card>
      <CardHeader>
        <CardTitle>Solved more than once</CardTitle>
        <CardDescription>
          Sessions that opened with the same prompt on different days. Ask your agent to check its
          history first (the recall skill or the MCP server) and the next one starts from the last
          answer.
        </CardDescription>
      </CardHeader>
      <CardContent>
        <ul className="divide-y divide-border overflow-hidden rounded-xl border bg-card">
          {(report.solvedAgain ?? []).map((g) => (
            <li key={g.threadIds[0]}>
              <RowButton
                onClick={() => openThread(g.threadIds[0])}
                title="Open the newest of these sessions"
              >
                <span className="line-clamp-2">{g.prompt}</span>
                <span className="block truncate text-xs text-muted-foreground">
                  {plural(g.sessions, "session")}
                  {g.project ? ` in ${baseName(g.project)}` : ""}, {date(g.firstSeen)} to{" "}
                  {date(g.lastSeen)}
                </span>
              </RowButton>
            </li>
          ))}
        </ul>
      </CardContent>
    </Card>
  );
}

function MemoryCard({ memory: m }: { memory: ReportMemory }) {
  const openProject = useUi((s) => s.openProject);
  return (
    <Card>
      <CardHeader>
        <CardTitle>What your agents remember</CardTitle>
        {m.decisions + m.gotchas + m.openTodos > 0 && (
          <CardDescription>
            {plural(m.decisions, "decision")}, {plural(m.gotchas, "gotcha")} and{" "}
            {plural(m.openTodos, "open TODO")} across {plural(m.projectsWithMemory, "project")}.
          </CardDescription>
        )}
      </CardHeader>
      <CardContent className="space-y-4">
        {m.topDecisionProjects.length > 0 && (
          <div>
            <div className="mb-1 text-xs uppercase tracking-wide text-muted-foreground">
              Most decisions
            </div>
            <div className="flex flex-wrap gap-2">
              {m.topDecisionProjects.map((p) => (
                <Button
                  key={p.project}
                  size="sm"
                  variant="outline"
                  title={p.project}
                  onClick={() => openProject(p.project)}
                >
                  {baseName(p.project)}
                  <span className="tabular-nums text-muted-foreground">{n(p.decisions)}</span>
                </Button>
              ))}
            </div>
          </div>
        )}
        {m.injected && (
          <div className="space-y-1">
            <div className="flex items-baseline justify-between gap-2 text-sm">
              <span>Sessions that started with memory</span>
              <span className="shrink-0 tabular-nums text-muted-foreground">
                {pct(m.injected.share)}
              </span>
            </div>
            <Bar share={m.injected.share} />
            <p className="text-xs text-muted-foreground">
              {plural(m.injected.injections, "injection")} over{" "}
              {plural(m.injected.sessions, "session")} since {dayLabel(m.injected.sinceDay)}, on
              every agent.
            </p>
          </div>
        )}
      </CardContent>
    </Card>
  );
}

function FilesCard({ report }: { report: Report }) {
  return (
    <Card>
      <CardHeader>
        <CardTitle>Files agents changed most</CardTitle>
      </CardHeader>
      <CardContent>
        <ul className="divide-y divide-border overflow-hidden rounded-xl border bg-card">
          {(report.files ?? []).map((f) => (
            <li
              key={`${f.project ?? ""}:${f.path}`}
              className="flex items-baseline gap-2 px-3 py-2 text-sm"
            >
              <span className="min-w-0 flex-1 truncate font-mono text-xs" title={f.path}>
                {f.path}
              </span>
              {f.project && (
                <span className="shrink-0 text-xs text-muted-foreground" title={f.project}>
                  {baseName(f.project)}
                </span>
              )}
              <span className="shrink-0 tabular-nums text-xs text-muted-foreground">
                {plural(f.edits, "edit")} · {plural(f.sessions, "session")}
              </span>
            </li>
          ))}
        </ul>
      </CardContent>
    </Card>
  );
}

function LedgerCard({ report }: { report: Report }) {
  const l = report.ledger;
  if (!l) return null;
  const cells = [
    { label: "Memory injected", value: l.counts.hook },
    { label: "Session digests", value: l.counts.digest },
    { label: "Recall hits", value: l.counts.recall },
    { label: "Error playbooks", value: l.counts.playbook },
    { label: "MCP tool calls", value: l.counts.mcp },
  ];
  return (
    <Card>
      <CardHeader>
        <CardTitle>What Callimachus did for your agents</CardTitle>
        <CardDescription>
          {l.sinceDay ? `Since ${dayLabel(l.sinceDay)}` : "Recently"}; {n(l.allTime)} in all.
          Counted on this machine only.
        </CardDescription>
      </CardHeader>
      <CardContent>
        <dl className="grid grid-cols-2 gap-px overflow-hidden rounded-lg border bg-border sm:grid-cols-5">
          {cells.map((c) => (
            <div key={c.label} className="min-w-0 bg-background px-4 py-3">
              <dt className="truncate text-[13px] text-muted-foreground">{c.label}</dt>
              <dd className="mt-0.5 truncate text-base font-semibold tabular-nums">{n(c.value)}</dd>
            </div>
          ))}
        </dl>
      </CardContent>
    </Card>
  );
}

/** The card as it will be saved, drawn at 2x for a sharp preview. */
function SharePreview({ report }: { report: Report }) {
  const theme = useTheme((s) => s.resolved);
  const [src, setSrc] = useState<string | null>(null);
  useEffect(() => {
    let live = true;
    drawReportCard(report, theme, 2)
      .then((canvas) => {
        if (live) setSrc(canvas.toDataURL("image/png"));
      })
      .catch(() => {});
    return () => {
      live = false;
    };
  }, [report, theme]);
  const figures = headline(report)
    .map((h) => `${h.value} ${h.label}`)
    .join(", ");
  return (
    <Card>
      <CardHeader>
        <CardTitle>Share card</CardTitle>
        <CardDescription>
          What "Save card as PNG" writes: headline numbers, your agents, the most frequent error and
          its fix. No prompt text: the error line comes from tool output, with paths shortened to
          file names.
        </CardDescription>
      </CardHeader>
      <CardContent>
        <div
          className="overflow-hidden rounded-lg border bg-muted"
          style={{ aspectRatio: `${CARD_WIDTH} / ${CARD_HEIGHT}` }}
        >
          {src && (
            <img
              src={src}
              alt={`Report card: ${figures}`}
              className="block h-full w-full"
              draggable={false}
            />
          )}
        </div>
      </CardContent>
    </Card>
  );
}
