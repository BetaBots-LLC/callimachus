import { useQuery } from "@tanstack/react-query";
import { api, type IndexProgress, SOURCE_LABELS, type SourceKind } from "../lib/api";
import { useEmbedStatus, useJobsStatus, usePushed, userReindexing } from "../lib/queries";
import { Progress, ProgressLabel, ProgressValue } from "@/components/ui/progress";

/**
 * The search bar's long-job progress bar (reindex, semantic build, distill). It is the only
 * component that subscribes to the per-tick progress data: a full reindex emits about
 * 2,100 index:progress events and a semantic build one embed:progress per batch, and when
 * SearchBar read them itself, each one re-rendered the input, the source chips and the
 * menus (300 events gave 300 commits of the whole bar in the dev build).
 */
export function JobProgress() {
  const jobs = useJobsStatus();
  const indexing = userReindexing(jobs.data);
  const embed = useEmbedStatus().data;
  // Per-source reindex progress, pushed by the index:progress event (main.tsx).
  const ip = usePushed<IndexProgress>(["index_progress"]);
  // Background auto-distill: status flag + per-thread progress (distill:* events).
  const distilling = useQuery({
    queryKey: ["distill_status"],
    queryFn: api.distillingStatus,
    refetchInterval: (q) => (q.state.data ? 1500 : false),
  });
  const dp = usePushed<{ done: number; total: number }>(["distill_progress"]);

  if (indexing) {
    // Reindex total is estimated from the existing thread count (accurate on a re-index,
    // indeterminate on a first run). The live "N scanned" count keeps a long source
    // (e.g. Claude Code) from ever looking stuck.
    return (
      <Progress
        value={ip && ip.total > 0 ? Math.min(99, Math.round((ip.done / ip.total) * 100)) : null}
        className="gap-1.5"
      >
        <ProgressLabel className="text-xs font-normal text-muted-foreground">
          {ip?.current
            ? `Indexing ${SOURCE_LABELS[ip.current as SourceKind] ?? ip.current}…`
            : "Indexing…"}
        </ProgressLabel>
        <span className="ml-auto text-xs text-muted-foreground tabular-nums">
          {(ip?.done ?? 0).toLocaleString()} scanned
        </span>
      </Progress>
    );
  }

  // One bar, whichever job is active. Embed wins over distill (distill is background and
  // yields to the others).
  let pct: number;
  let label: string;
  if (embed?.running) {
    pct = embed.total > 0 ? Math.round((embed.done / embed.total) * 100) : 0;
    label = `Building semantic index · ${embed.done.toLocaleString()} / ${embed.total.toLocaleString()} messages`;
  } else if (distilling.data) {
    pct = dp && dp.total > 0 ? Math.round((dp.done / dp.total) * 100) : 0;
    label = dp
      ? `Distilling knowledge · ${dp.done} / ${dp.total} threads`
      : "Distilling knowledge…";
  } else {
    return null;
  }
  return (
    <Progress value={pct} className="gap-1.5">
      <ProgressLabel className="text-xs font-normal text-muted-foreground">{label}</ProgressLabel>
      <ProgressValue className="text-xs" />
    </Progress>
  );
}

/** A quiet note while the launch catch-up scan looks for sessions written while the app
 *  was closed. No bar and no spinner: it is routine, not something the user waits on. */
export function CatchUpNote() {
  const catchUp = useJobsStatus().data?.catchUp === true;
  return catchUp ? <span role="status">Checking for new sessions…</span> : null;
}
