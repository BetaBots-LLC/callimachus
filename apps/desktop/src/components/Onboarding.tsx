import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, type IndexProgress, SOURCE_LABELS, type SourceKind } from "../lib/api";
import { Button } from "@/components/ui/button";
import { Progress, ProgressLabel, ProgressValue } from "@/components/ui/progress";

/**
 * First-run experience. Shown on the Search landing while the index is empty, so a fresh
 * install isn't a blank screen — one click indexes the user's local agent history. Once
 * threads exist this unmounts and the normal app takes over. Semantic search + the
 * Knowledge layer are opt-in follow-ups (the search header + the Knowledge tab guide there).
 */
export function Onboarding() {
  const qc = useQueryClient();
  const indexing = useQuery({
    queryKey: ["index_status"],
    queryFn: api.indexingStatus,
    refetchInterval: (q) => (q.state.data ? 1000 : false),
  });
  const progress = useQuery<IndexProgress | null>({
    queryKey: ["index_progress"],
    queryFn: () => qc.getQueryData<IndexProgress>(["index_progress"]) ?? null,
    staleTime: Number.POSITIVE_INFINITY,
  });
  const reindex = useMutation({
    mutationFn: api.indexAll,
    onSuccess: () => qc.invalidateQueries({ queryKey: ["index_status"] }),
  });

  const busy = !!indexing.data || reindex.isPending;
  const ip = progress.data;
  const pct = ip && ip.total > 0 ? Math.round((ip.done / ip.total) * 100) : 0;

  return (
    <div className="mx-auto flex h-full w-full max-w-lg flex-col items-center justify-center gap-6 p-8 text-center">
      <div className="space-y-2">
        <h1 className="text-2xl font-semibold tracking-tight">Welcome to Callimachus</h1>
        <p className="text-sm text-muted-foreground">
          One local, searchable index of every AI coding-agent conversation you've had. Let's
          build it from your history on this machine.
        </p>
      </div>

      {busy ? (
        <div className="w-full max-w-xs space-y-2">
          <Progress value={pct} className="gap-1.5">
            <ProgressLabel className="text-xs font-normal text-muted-foreground">
              Indexing {ip?.current ? (SOURCE_LABELS[ip.current as SourceKind] ?? ip.current) : "…"}
            </ProgressLabel>
            <ProgressValue className="text-xs" />
          </Progress>
          <p className="text-xs text-muted-foreground">
            Scanning Claude Code, Codex, Cursor, and 8 more — read-only.
          </p>
        </div>
      ) : (
        <Button size="lg" onClick={() => reindex.mutate()}>
          Index my history
        </Button>
      )}

      <p className="text-xs text-muted-foreground">
        Nothing leaves your machine. After indexing, add semantic search from the header and the
        Knowledge layer in Settings.
      </p>
    </div>
  );
}
