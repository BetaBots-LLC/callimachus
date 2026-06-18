import { useQuery } from "@tanstack/react-query";
import { api, SOURCE_LABELS, type SourceKind } from "../lib/api";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { formatTime, shortPath } from "../lib/format";

export function StatsView() {
  const { data, isLoading, isError, error } = useQuery({
    queryKey: ["index_stats"],
    queryFn: api.indexStats,
  });

  if (isLoading) return <Centered>Crunching the index…</Centered>;
  if (isError || !data) return <Centered>Couldn't load stats. {String(error ?? "")}</Centered>;

  const coverage = data.embeddable > 0 ? data.embedded / data.embeddable : 0;
  const maxSrcMessages = Math.max(1, ...data.perSource.map((s) => s.messages));
  const totalRoleMessages = Math.max(
    1,
    data.perRole.reduce((n, r) => n + r.messages, 0),
  );

  return (
    <div className="mx-auto w-full max-w-4xl space-y-6 overflow-y-auto p-6">
      <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
        <Stat label="Threads" value={data.threads.toLocaleString()} />
        <Stat label="Messages" value={data.messages.toLocaleString()} />
        <Stat
          label="Semantic coverage"
          value={`${Math.round(coverage * 100)}%`}
          sub={`${data.embedded.toLocaleString()} / ${data.embeddable.toLocaleString()} embedded`}
        />
        <Stat
          label="Span"
          value={data.earliest ? formatTime(data.earliest) : "—"}
          sub={data.latest ? `→ ${formatTime(data.latest)}` : undefined}
        />
      </div>

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

function Stat({ label, value, sub }: { label: string; value: string; sub?: string }) {
  return (
    <Card>
      <CardContent className="p-4">
        <div className="text-xs uppercase tracking-wide text-muted-foreground">{label}</div>
        <div className="mt-1 text-2xl font-semibold tabular-nums">{value}</div>
        {sub && <div className="mt-0.5 text-xs text-muted-foreground">{sub}</div>}
      </CardContent>
    </Card>
  );
}

function Centered({ children }: { children: React.ReactNode }) {
  return (
    <div className="flex h-full items-center justify-center p-6 text-center text-muted-foreground">
      {children}
    </div>
  );
}
