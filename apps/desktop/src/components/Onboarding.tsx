import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, type IndexProgress, SOURCE_LABELS, type SourceKind } from "../lib/api";
import { useIndexError } from "../lib/queries";
import { Button } from "@/components/ui/button";
import { Progress, ProgressLabel } from "@/components/ui/progress";

/**
 * First-run experience. Shown on the Search landing while the index is empty, so a fresh
 * install isn't a blank screen: one click indexes the user's local agent history. Once
 * threads exist this unmounts and the normal app takes over. Semantic search and memory
 * (LLM distillation) are opt-in follow-ups (the search header and the Turn on memory card
 * guide there).
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
  // db_stats is the shared header cache (no extra fetch). `index_ran` is set by the
  // index:done listener in main.tsx, so it's true once a reindex has finished this session.
  const stats = useQuery({ queryKey: ["db_stats"], queryFn: api.dbStats });
  const ran = useQuery<boolean>({
    queryKey: ["index_ran"],
    queryFn: () => qc.getQueryData<boolean>(["index_ran"]) ?? false,
    staleTime: Number.POSITIVE_INFINITY,
  });

  // Set by the index:error listener in main.tsx. A failed scan also ends with an empty
  // index:done, so this is checked first: "no threads found" would be the wrong story.
  const indexError = useIndexError();

  const busy = !!indexing.data || reindex.isPending;
  const failed = !!indexError && !busy;
  // A reindex finished but the store is still empty → say so, instead of silently
  // re-offering the same "Index my history" button (which read as "nothing happened").
  const emptyAfterRun = !failed && !!ran.data && !busy && (stats.data?.threads ?? 0) === 0;
  const ip = progress.data;
  const source = ip?.current ? (SOURCE_LABELS[ip.current as SourceKind] ?? ip.current) : "…";
  // Determinate % when we have an estimated total (re-index); indeterminate on first run.
  const pct = ip && ip.total > 0 ? Math.min(99, Math.round((ip.done / ip.total) * 100)) : null;

  return (
    <div className="mx-auto flex h-full w-full max-w-lg flex-col items-center justify-center gap-6 p-8 text-center">
      <div className="space-y-2">
        <h1 className="text-2xl font-semibold tracking-tight">
          {failed
            ? "Couldn't index your history"
            : emptyAfterRun
              ? "No threads found"
              : "Welcome to Callimachus"}
        </h1>
        <p className="text-sm text-muted-foreground">
          {failed
            ? "The scan stopped before it could index anything, so your history may well be there. Your agents' files were only read, never changed. Try again, and if it keeps failing, the reason below says what to fix."
            : emptyAfterRun
              ? "Callimachus scanned this machine for Claude Code, Codex, Cursor, and 9 more agents but found no history to index. If you just used one, reindex to pick it up."
              : "One local, searchable index of every AI coding-agent conversation you've had. Let's build it from your history on this machine."}
        </p>
        {failed && (
          <p
            role="alert"
            className="break-words rounded-md bg-destructive/10 px-3 py-2 text-left font-mono text-xs text-destructive"
          >
            {indexError}
          </p>
        )}
      </div>

      {busy ? (
        <div className="w-full max-w-xs space-y-2">
          <Progress value={pct} className="gap-1.5">
            <ProgressLabel className="text-xs font-normal text-muted-foreground">
              Indexing {source}
            </ProgressLabel>
            <span className="ml-auto text-xs text-muted-foreground tabular-nums">
              {(ip?.done ?? 0).toLocaleString()} scanned
            </span>
          </Progress>
          <p className="text-xs text-muted-foreground">
            Scanning Claude Code, Codex, Cursor, and 9 more. Read-only.
          </p>
        </div>
      ) : (
        <Button size="lg" onClick={() => reindex.mutate()}>
          {failed ? "Try again" : emptyAfterRun ? "Reindex" : "Index my history"}
        </Button>
      )}

      <p className="text-xs text-muted-foreground">
        Your history stays on your machine. After indexing, add semantic search from the header and
        turn on memory in Settings &gt; Knowledge.
      </p>
    </div>
  );
}
