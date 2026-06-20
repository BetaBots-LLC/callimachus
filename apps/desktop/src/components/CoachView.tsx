import { useState } from "react";
import { useMutation, useQuery } from "@tanstack/react-query";
import { api, type CoachFact } from "../lib/api";
import { useUi } from "../store/ui";
import { shortPath } from "../lib/format";
import { Loading } from "./Loading";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Spinner } from "@/components/ui/spinner";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";

const SECS_PER_DAY = 86_400;
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
// faint track. Kept off the Tailwind palette on purpose — this ramp is the view's signature.
const CELL_BG = [
  "color-mix(in oklab, var(--muted-foreground) 14%, transparent)",
  "rgba(193, 110, 44, 0.3)",
  "rgba(193, 110, 44, 0.5)",
  "rgba(193, 110, 44, 0.72)",
  "rgba(193, 110, 44, 0.95)",
];

const DATE_FMT: Intl.DateTimeFormatOptions = { weekday: "short", month: "short", day: "numeric" };

function relativeDay(epochSeconds: number, todayMid: number): string {
  const days = Math.round(
    (todayMid - Math.floor(epochSeconds / SECS_PER_DAY) * SECS_PER_DAY) / SECS_PER_DAY,
  );
  if (days <= 0) return "today";
  if (days === 1) return "yesterday";
  return `${days}d ago`;
}

function FactList({
  facts,
  todayMid,
  onOpen,
}: {
  facts: CoachFact[];
  todayMid: number;
  onOpen: (f: CoachFact) => void;
}) {
  if (facts.length === 0) {
    return <p className="text-sm text-muted-foreground">Nothing captured this week.</p>;
  }
  return (
    <ul className="space-y-1.5">
      {facts.map((f) => (
        <li key={f.id}>
          <button
            type="button"
            onClick={() => onOpen(f)}
            className="group w-full cursor-pointer rounded-lg border px-3 py-2 text-left transition-colors hover:bg-muted/50"
          >
            <span className="block text-sm leading-snug text-foreground">{f.text}</span>
            <span className="mt-1 flex items-center gap-2 text-[0.7rem] text-muted-foreground">
              <span className="truncate">{f.title || `Thread #${f.threadId}`}</span>
              {f.project ? <span className="shrink-0">· {shortPath(f.project)}</span> : null}
              <span className="ml-auto shrink-0">{relativeDay(f.createdAt, todayMid)}</span>
            </span>
          </button>
        </li>
      ))}
    </ul>
  );
}

/** The "have I done this before?" guard: describe a task → prior sessions that solved
 *  something similar, each with its most-relevant decision/gotcha. */
function PriorWorkSearch({ onOpen }: { onOpen: (threadId: number) => void }) {
  const [q, setQ] = useState("");
  const find = useMutation({
    mutationFn: (query: string) => api.findPriorWork(query, { limit: 8 }),
  });
  return (
    <section className="mb-8">
      <h2 className="mb-2 text-xs font-medium uppercase tracking-wide text-muted-foreground">
        Have you done this before?
      </h2>
      <form
        onSubmit={(e) => {
          e.preventDefault();
          const v = q.trim();
          if (v) find.mutate(v);
        }}
        className="flex gap-2"
      >
        <Input
          value={q}
          onChange={(e) => setQ(e.currentTarget.value)}
          placeholder="Describe what you're about to work on…"
        />
        <Button type="submit" disabled={find.isPending || !q.trim()}>
          {find.isPending ? <Spinner /> : "Check"}
        </Button>
      </form>
      {find.data &&
        (find.data.length === 0 ? (
          <p className="mt-3 text-sm text-muted-foreground">
            No prior work found — needs distilled decisions / gotchas (enable Knowledge in
            Settings).
          </p>
        ) : (
          <ul className="mt-3 space-y-1.5">
            {find.data.map((h) => (
              <li key={h.threadId}>
                <button
                  type="button"
                  onClick={() => onOpen(h.threadId)}
                  className="w-full cursor-pointer rounded-lg border px-3 py-2 text-left transition-colors hover:bg-muted/50"
                >
                  <span className="flex items-center gap-2">
                    <span className="truncate text-sm font-medium text-foreground">
                      {h.title || `Thread #${h.threadId}`}
                    </span>
                    {h.projectPath ? (
                      <span className="shrink-0 text-[0.7rem] text-muted-foreground">
                        · {shortPath(h.projectPath)}
                      </span>
                    ) : null}
                    <span className="ml-auto shrink-0 text-[0.7rem] text-muted-foreground">
                      {Math.round(h.similarity * 100)}% match
                    </span>
                  </span>
                  <span className="mt-1 block text-xs text-muted-foreground">
                    <span className="uppercase">{h.kind}</span> · {h.snippet}
                  </span>
                </button>
              </li>
            ))}
          </ul>
        ))}
    </section>
  );
}

export function CoachView() {
  const selectThread = useUi((s) => s.selectThread);
  const setView = useUi((s) => s.setView);
  const { data, isLoading } = useQuery({
    queryKey: ["coach_overview"],
    queryFn: api.coachOverview,
  });

  if (isLoading) return <Loading label="Reading your history…" />;

  const todayMid = Math.floor(Date.now() / 1000 / SECS_PER_DAY) * SECS_PER_DAY;
  const startMid = todayMid - HEATMAP_DAYS * SECS_PER_DAY;
  // Pad the start back to the beginning of its week (Sunday) so columns align like a calendar.
  const startDow = new Date(startMid * 1000).getUTCDay();
  const gridStart = startMid - startDow * SECS_PER_DAY;
  // Columns = the index of today's column + 1 (today sits at day-offset / 7). No trailing
  // all-empty column.
  const weekCount = Math.floor((todayMid - gridStart) / SECS_PER_DAY / 7) + 1;

  const counts = new Map((data?.heatmap ?? []).map((d) => [d.day, d.messages]));
  const maxDay = (data?.heatmap ?? []).reduce((m, d) => Math.max(m, d.messages), 0);
  const totalMessages = (data?.heatmap ?? []).reduce((s, d) => s + d.messages, 0);
  const activeDays = (data?.heatmap ?? []).filter((d) => d.messages > 0).length;

  const openThread = (f: CoachFact) => {
    selectThread(f.threadId);
    setView("search");
  };

  return (
    <div className="mx-auto h-full w-full max-w-5xl overflow-y-auto px-8 py-6">
      <div className="mb-5">
        <h1 className="text-xl font-semibold tracking-tight">Coach</h1>
        <p className="text-sm text-muted-foreground">
          What your history is telling you — recent activity and the decisions worth remembering.
        </p>
      </div>

      <PriorWorkSearch
        onOpen={(id) => {
          selectThread(id);
          setView("search");
        }}
      />

      {/* Activity heatmap */}
      <section className="mb-8">
        <div className="mb-2 flex items-baseline justify-between">
          <h2 className="text-xs font-medium uppercase tracking-wide text-muted-foreground">
            Activity · last 52 weeks
          </h2>
          <span className="text-[0.7rem] text-muted-foreground">
            {totalMessages.toLocaleString()} messages · {activeDays} active days
          </span>
        </div>
        <TooltipProvider delay={80}>
          <div className="flex w-full gap-0.5">
            {Array.from({ length: weekCount }, (_, w) => (
              <div key={gridStart + w * 7 * SECS_PER_DAY} className="flex flex-1 flex-col gap-0.5">
                {Array.from({ length: 7 }, (_, d) => {
                  const day = gridStart + (w * 7 + d) * SECS_PER_DAY;
                  if (day < startMid || day > todayMid) {
                    return <div key={day} className="aspect-square" />;
                  }
                  const c = counts.get(day) ?? 0;
                  const label = `${c} message${c === 1 ? "" : "s"} · ${new Date(
                    day * 1000,
                  ).toLocaleDateString(undefined, DATE_FMT)}`;
                  return (
                    <Tooltip key={day}>
                      <TooltipTrigger
                        render={
                          <div
                            className="aspect-square rounded-xs ring-ring/40 hover:ring-1"
                            style={{ backgroundColor: CELL_BG[level(c, maxDay)] }}
                          />
                        }
                      />
                      <TooltipContent>{label}</TooltipContent>
                    </Tooltip>
                  );
                })}
              </div>
            ))}
          </div>
        </TooltipProvider>
        <div className="mt-2 flex items-center gap-1.5 text-[0.7rem] text-muted-foreground">
          <span>less</span>
          {CELL_BG.map((bg) => (
            <span key={bg} className="size-2.5 rounded-xs" style={{ backgroundColor: bg }} />
          ))}
          <span>more</span>
        </div>
      </section>

      {/* This week's captured knowledge */}
      <section>
        <h2 className="mb-3 text-xs font-medium uppercase tracking-wide text-muted-foreground">
          This week
        </h2>
        <div className="grid gap-6 sm:grid-cols-2">
          <div className="space-y-2">
            <h3 className="text-sm font-medium">Decisions</h3>
            <FactList facts={data?.decisions ?? []} todayMid={todayMid} onOpen={openThread} />
          </div>
          <div className="space-y-2">
            <h3 className="text-sm font-medium">Gotchas</h3>
            <FactList facts={data?.gotchas ?? []} todayMid={todayMid} onOpen={openThread} />
          </div>
        </div>
        {(data?.decisions.length ?? 0) === 0 && (data?.gotchas.length ?? 0) === 0 && (
          <p className="mt-3 text-xs text-muted-foreground">
            Decisions and gotchas appear here once your recent sessions have been distilled (enable
            the Knowledge layer in Settings).
          </p>
        )}
      </section>
    </div>
  );
}
