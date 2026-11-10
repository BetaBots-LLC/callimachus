import { useEffect, useId, useRef, useState } from "react";
import {
  keepPreviousData,
  type MutationState,
  useIsMutating,
  useMutation,
  useMutationState,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";
import { ChevronLeft, ChevronRight, TriangleAlert } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Progress } from "@/components/ui/progress";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Spinner } from "@/components/ui/spinner";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { useAppForm } from "@/lib/form";
import {
  api,
  type CleanupRow,
  type CompactSummary,
  type IndexSnapshot,
  type ParseDrift,
  SOURCE_LABELS,
  type SnapshotStatus,
} from "../../lib/api";
import { formatTime } from "../../lib/format";
import {
  anyJobRunning,
  confirmUserStarted,
  EMBED_ERROR_KEY,
  fetchJobsStatus,
  markUserStarted,
  REWRITE_KEY,
  unmarkUserStarted,
  useEmbedError,
  useEmbedStatus,
  useIndexError,
  useIndexRewriting,
  useJobsStatus,
  usePushed,
  useReindex,
  useReindexStarting,
  userReindexing,
} from "../../lib/queries";
import { invalidateIndexDerived } from "../../lib/queryClient";
import { errorText, notify } from "../../lib/toast";
import { InlineError, Note, SectionHeader, SettingRow, SettingsGroup } from "./shared";

/** A cleanup row plus its form-managed selection flag. */
type Row = CleanupRow & { selected: boolean };

// Shrink index and Reclaim space each rewrite the whole file on their own connection.
// Keyed mutations (under REWRITE_KEY), so "one is running" comes from the mutation cache
// and survives leaving Settings (the view unmounts on every tab switch).
const VACUUM_KEY = [...REWRITE_KEY, "vacuum"] as const;
const COMPACT_KEY = [...REWRITE_KEY, "compact"] as const;
/** Running totals from `compact:progress`, kept in the cache for the same reason. */
const COMPACT_PROGRESS_KEY = ["compact_progress"] as const;

const PAGE_SIZE = 12;
const FETCH_LIMIT = 500; // oldest N threads; paginate client-side over these

const AGES: { value: string; label: string; days: number | null }[] = [
  { value: "all", label: "Any age", days: null },
  { value: "30", label: "Older than 30 days", days: 30 },
  { value: "90", label: "Older than 90 days", days: 90 },
  { value: "182", label: "Older than 6 months", days: 182 },
  { value: "365", label: "Older than 1 year", days: 365 },
];

function fmtBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(0)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}

/** Bytes as MB, or GB once that reads better (snapshots of a large index). */
function fmtSize(n: number): string {
  return n >= 1024 ** 3 ? `${(n / 1024 ** 3).toFixed(1)} GB` : fmtBytes(n);
}

const SNAPSHOT_KIND: Record<IndexSnapshot["reason"], string> = {
  weekly: "Weekly",
  "pre-migration": "Before an upgrade",
  manual: "cal backup",
};

/** "Weekly, Nov 8 · Before an upgrade, Nov 5 · Weekly of an earlier index, Oct 19", newest
 *  first. */
function snapshotList(s: SnapshotStatus): string {
  return s.snapshots
    .map(
      (x) =>
        `${SNAPSHOT_KIND[x.reason]}${x.otherIndex ? " of an earlier index" : ""}, ${formatTime(x.takenAt)}`,
    )
    .join(" · ");
}

/** "Saved 30 MB.", saying when the file itself has not shrunk (yet): Shrink index only
 *  rewrites the file once Reclaim space's 50 MB / 1% rule is met. */
function savedNote(s: CompactSummary): string {
  const saved = `Saved ${fmtBytes(s.bytesSaved)}`;
  if (s.vacuum && !s.vacuum.ran) {
    return `${saved} inside the index; the file shrinks once 50 MB is free.`;
  }
  if (s.vacuum?.pending) return `${saved}; the file shrinks shortly.`;
  return `${saved}.`;
}

/** The index's snapshots: what is kept and what it costs on disk. Restoring one is the
 *  startup screen's job (a damaged index), so this row only reports. */
function SnapshotsRow() {
  const status = useQuery({
    queryKey: ["index_snapshots"],
    queryFn: api.indexSnapshots,
    staleTime: 60_000,
  });
  const s = status.data;
  const earlier = s?.snapshots.some((x) => x.otherIndex) ?? false;
  return (
    <SettingRow
      title="Index snapshots"
      description={
        s && !s.automatic
          ? "Copies of the index that cal backup saves next to it. If the index is ever damaged, Callimachus offers to restore the newest one when it starts."
          : "A copy of the index before each upgrade, and once a week after an integrity check. If the index is ever damaged, Callimachus offers to restore the newest one when it starts."
      }
    >
      {s && s.snapshots.length > 0 && (
        <Note>
          {snapshotList(s)}.{" "}
          {s.bytesOnDisk != null
            ? `They use ${fmtSize(s.bytesOnDisk)} of disk, which grows as the index changes; Shrink index and Reclaim space remove ${earlier ? "this index's" : "the"} automatic ones.`
            : `They take ${fmtSize(s.snapshots.reduce((n, x) => n + x.bytes, 0))} of disk.`}{" "}
          {earlier &&
            "Those of an earlier index hold its history and are never removed automatically. "}
          In <code className="font-mono">{s.folder}</code>.
        </Note>
      )}
      {s && s.snapshots.length === 0 && s.automatic && (
        <Note>None yet: the first is taken while the app is idle, after an integrity check.</Note>
      )}
      {s && !s.automatic && (
        <Note>
          The index can't be copied for free here (that needs a Mac with the index on an APFS disk),
          so no snapshots are taken automatically.{" "}
          <code className="font-mono">cal backup --to &lt;folder&gt;</code> saves a full copy.
        </Note>
      )}
      {s?.lastCheck && !s.lastCheck.ok && (
        <InlineError>
          {/* `at` 0: a failed result carried across Shrink index / Reclaim space, no date. */}
          {s.lastCheck.at > 0
            ? `The last integrity check (${formatTime(s.lastCheck.at)}) found problems`
            : "An earlier integrity check found problems"}{" "}
          in the index file, so no new snapshot was taken and the existing ones were kept.{" "}
          <code className="font-mono">cal doctor --quick-check</code> shows what it found;
          callimachus.log next to the index has it too.
        </InlineError>
      )}
    </SettingRow>
  );
}

/** The history index and the semantic index behind search, and the disk space the history
 *  takes: shrink, reclaim, delete. */
export function IndexSection() {
  return (
    <>
      <SectionHeader
        title="Index & storage"
        description="The index behind search, and the space your history takes on disk."
      />
      <div className="space-y-8">
        <ParseDriftWarnings />
        <SettingsGroup aria-label="Search index">
          <ReindexRow />
          <SemanticIndexRow />
        </SettingsGroup>
        <StorageGroup />
      </div>
    </>
  );
}

/** The parse canary (`indexer::parse_health`): an agent whose last indexing pass read like a
 *  changed transcript format. Its unreadable messages are not indexed, and once the agent's
 *  own cleanup deletes the originals they are gone, so this says so until it is dismissed (for
 *  30 days of the same change) or a full pass reads the source clean again. */
function ParseDriftWarnings() {
  const queryClient = useQueryClient();
  const drifts = useQuery({ queryKey: ["parse_drifts"], queryFn: api.parseDrifts });
  const dismiss = useMutation({
    mutationFn: api.dismissParseDrift,
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["parse_drifts"] }),
    onError: (e) => notify.error("Couldn't dismiss the warning", { description: errorText(e) }),
  });
  if (!drifts.data?.length) return null;
  return (
    <SettingsGroup aria-label="Transcript format warnings">
      {drifts.data.map((d) => (
        <SettingRow
          key={d.source}
          title={
            <span className="inline-flex items-center gap-1.5 text-amber-700 dark:text-amber-400">
              <TriangleAlert aria-hidden="true" className="size-3.5 shrink-0" />
              {SOURCE_LABELS[d.source] ?? d.source} may have changed its transcript format
            </span>
          }
          description={driftText(d)}
          control={
            <Button
              size="sm"
              variant="outline"
              disabled={dismiss.isPending}
              onClick={() => dismiss.mutate(d.source)}
            >
              Dismiss
            </Button>
          }
        />
      ))}
    </SettingsGroup>
  );
}

/** What the drifting pass saw, and what to do about it. */
function driftText(d: ParseDrift): string {
  const parts: string[] = [];
  if (d.byShare) {
    const pct = (n: number) => `${Math.round(n * 100)}%`;
    parts.push(
      `In the indexing pass at ${formatTime(d.at)}, ${pct(d.share)} of the records it read were of a kind Callimachus does not know or could not parse (usually ${pct(d.baseline)})${d.kinds.length ? `: ${d.kinds.join(", ")}` : ""}.`,
    );
  }
  if (d.byZeroFiles) {
    parts.push(
      `${d.zeroFiles} of the ${d.files} files it read held no message Callimachus could index.`,
    );
  }
  parts.push(
    "Those messages are not in the index, and the agent may delete the originals later. Update Callimachus, or report it with the diagnostics from Settings > General.",
  );
  return parts.join(" ");
}

/** A full reindex, the same start as the palette's "Reindex history" (useReindex), so the
 *  repair the Spend card and a stale list point at is reachable without the palette. */
function ReindexRow() {
  const reindex = useReindex();
  // `cal watch` and the app never watch at once: while it holds the index, the app's watcher
  // waits and looks again every minute.
  const live = useQuery({
    queryKey: ["live_indexing"],
    queryFn: api.liveIndexing,
    refetchInterval: 60_000,
  });
  const starting = useReindexStarting();
  const running = userReindexing(useJobsStatus().data);
  const indexError = useIndexError();
  // Reindex returns without doing anything while Shrink index / Reclaim space runs.
  const rewriting = useIndexRewriting();
  const busy = starting || running;
  return (
    <SettingRow
      title="History index"
      description="Reads every agent's history again from its source files: new sessions, token usage and anything the watcher missed. Search keeps working while it runs."
      control={
        <Button
          size="sm"
          variant="outline"
          onClick={() => reindex.mutate()}
          disabled={busy || rewriting}
        >
          {busy && <Spinner data-icon="inline-start" />}
          {busy ? "Reindexing…" : "Reindex everything"}
        </Button>
      }
    >
      {live.data?.cliWatch && (
        <Note>
          Live indexing is off: <code>cal watch</code>
          {live.data.cliWatchPid != null && ` (process ${live.data.cliWatchPid})`} is keeping this
          index up to date. Stop it and the app takes over within a minute.
        </Note>
      )}
      {indexError && !busy && (
        <InlineError>The last reindex stopped: {indexError}. Reindex again to retry.</InlineError>
      )}
      {reindex.isError && (
        <InlineError>Couldn't start the reindex: {errorText(reindex.error)}</InlineError>
      )}
    </SettingRow>
  );
}

function SemanticIndexRow() {
  const queryClient = useQueryClient();
  // Event-driven; never polls while idle (see lib/queries.ts).
  const embed = useEmbedStatus();
  const embedError = useEmbedError();
  // Build does nothing while Shrink index / Reclaim space runs.
  const rewriting = useIndexRewriting();
  // A reindex holds the index too; a build started now would return without running.
  const indexing = !!useJobsStatus().data?.index;
  const buildIndex = useMutation({
    mutationFn: api.buildEmbeddings,
    // A retry starts clean; a new failure arrives as a fresh embed:error.
    onMutate: () => {
      queryClient.setQueryData(EMBED_ERROR_KEY, null);
      markUserStarted("embed"); // main.tsx toasts when it finishes
    },
    // Pick up running:true now; progress then streams in via embed:progress, shown in
    // place by this row and the search bar, so a successful start gets no toast.
    onSuccess: async () => {
      queryClient.invalidateQueries({ queryKey: ["embed_status"] });
      if (!(await confirmUserStarted(queryClient, "embed"))) {
        notify.warning("Couldn't start the build", {
          description: "Another job is using the index. Try again when it finishes.",
        });
      }
    },
    onError: (e) => {
      unmarkUserStarted("embed");
      notify.error("Couldn't start the build", { description: errorText(e) });
    },
  });

  const running = !!embed.data?.running;
  const nothingEmbedded = embed.data?.done === 0;
  const pct =
    embed.data && embed.data.total > 0 ? Math.round((embed.data.done / embed.data.total) * 100) : 0;

  return (
    <SettingRow
      title="Semantic index"
      description={
        embed.data
          ? `${embed.data.done.toLocaleString()} of ${embed.data.total.toLocaleString()} messages embedded`
          : "Loading…"
      }
      control={
        <Button
          size="sm"
          variant={nothingEmbedded ? "default" : "outline"}
          onClick={() => buildIndex.mutate()}
          disabled={running || rewriting || indexing}
        >
          {running && <Spinner data-icon="inline-start" />}
          {running ? "Building…" : nothingEmbedded ? "Build index" : "Update index"}
        </Button>
      }
    >
      {running && (
        <Progress value={pct} aria-label="Semantic index build" className="gap-1.5">
          <span className="sr-only">{`${pct}% embedded`}</span>
        </Progress>
      )}
      {embedError && !running && (
        <InlineError>
          The last build stopped: {embedError}. Nothing was marked as embedded, so building again
          picks up where it left off.
        </InlineError>
      )}
      {buildIndex.isError && (
        <InlineError>Couldn't start the build: {errorText(buildIndex.error)}</InlineError>
      )}
    </SettingRow>
  );
}

function StorageGroup() {
  const queryClient = useQueryClient();
  const ageId = useId();
  const [age, setAge] = useState("all");
  const [page, setPage] = useState(0);
  const [err, setErr] = useState<string | null>(null);
  const [confirming, setConfirming] = useState(false);

  // Key on the age *filter*, and compute the cutoff timestamp inside the fetch. Keying
  // on the timestamp itself (it changes every second) made each re-render a new query,
  // which re-seeded the form and wiped the selection before "Confirm delete".
  const rows = useQuery({
    queryKey: ["cleanup", age],
    queryFn: () => {
      const days = AGES.find((a) => a.value === age)?.days ?? null;
      const before = days ? Math.floor(Date.now() / 1000) - days * 86400 : undefined;
      return api.cleanupCandidates({ before, limit: FETCH_LIMIT });
    },
    staleTime: 30_000,
    placeholderData: keepPreviousData,
  });

  // Form holds the full list + a per-row `selected` flag (TanStack array field).
  const form = useAppForm({ defaultValues: { rows: [] as Row[] } });

  // Re-seed the form whenever the query data changes. A refetch of the same filter (after
  // a delete, or a background index update) keeps the selection for rows that are still
  // listed; a new filter starts fresh from page one.
  const seededAge = useRef<string | null>(null);
  // biome-ignore lint/correctness/useExhaustiveDependencies: `form` is stable; reading its current values here is the point (keep selections), not a trigger
  useEffect(() => {
    if (!rows.data || rows.isPlaceholderData) return;
    const sameFilter = seededAge.current === age;
    const keep = sameFilter
      ? new Set(form.state.values.rows.filter((r) => r.selected).map((r) => r.id))
      : new Set<number>();
    form.reset({ rows: rows.data.map((r) => ({ ...r, selected: keep.has(r.id) })) });
    seededAge.current = age;
    if (!sameFilter) {
      setConfirming(false);
      setPage(0);
    }
  }, [rows.data, rows.isPlaceholderData, age]);

  const total = rows.data?.length ?? 0;
  const pageCount = Math.max(1, Math.ceil(total / PAGE_SIZE));
  const safePage = Math.min(page, pageCount - 1);
  const start = safePage * PAGE_SIZE;

  function invalidateAll() {
    for (const key of ["db_stats", "results", "recent"]) {
      queryClient.invalidateQueries({ queryKey: [key] });
    }
    // delete_threads emits no index:done, so Stats / Today / Projects (5-minute stale
    // window) would keep listing the deleted threads without this.
    invalidateIndexDerived();
  }

  const del = useMutation({
    mutationFn: (ids: number[]) => api.deleteThreads(ids),
    onMutate: () => setErr(null),
    onSuccess: (removed) => {
      setConfirming(false);
      invalidateAll();
      void rows.refetch();
      if (removed === 0) {
        setErr("No threads were deleted.");
        notify.warning("No threads were deleted.");
      } else {
        notify.success(`Deleted ${removed} thread${removed === 1 ? "" : "s"}`);
      }
    },
    onError: (e) => {
      setErr(String(e));
      notify.error("Couldn't delete the threads", { description: errorText(e) });
    },
  });
  // The backend may have refused because a job started meanwhile, or the rewrite's own
  // claim just ended: either way, refresh the shared job flags the other buttons read.
  const refreshJobs = () => void fetchJobsStatus(queryClient).catch(() => {});
  const vacuum = useMutation({
    mutationKey: VACUUM_KEY,
    mutationFn: api.vacuumDb,
    onMutate: () => setErr(null),
    onSuccess: (report) => {
      invalidateAll();
      if (!report.ran) {
        notify.info("Nothing to reclaim", {
          description: `Only ${fmtBytes(report.reclaimableBytes)} is free inside the index.`,
        });
      } else if (report.pending) {
        // A reader held the closing checkpoint back: the file shrinks at the next one.
        notify.success("Index rewritten", {
          description: `${fmtBytes(report.reclaimableBytes)} will be returned to disk shortly.`,
        });
      } else {
        notify.success(`Reclaimed ${fmtBytes(report.freedBytes)}`);
      }
    },
    onError: (e) => {
      setErr(String(e));
      notify.error("Couldn't reclaim space", { description: errorText(e) });
    },
    onSettled: refreshJobs,
  });
  const compact = useMutation({
    mutationKey: COMPACT_KEY,
    mutationFn: () => api.compactIndex((p) => queryClient.setQueryData(COMPACT_PROGRESS_KEY, p)),
    onMutate: () => {
      setErr(null);
      queryClient.setQueryData(COMPACT_PROGRESS_KEY, null);
    },
    // Row sizes change, so the cleanup list (an index-derived aggregate) refreshes too.
    onSuccess: (summary) => {
      invalidateAll();
      notify.success(
        summary.rows > 0
          ? `Trimmed ${summary.rows.toLocaleString()} tool outputs`
          : "Nothing to trim",
        summary.bytesSaved > 0 ? { description: savedNote(summary) } : undefined,
      );
    },
    onError: (e) => {
      setErr(String(e));
      notify.error("Couldn't shrink the index", { description: errorText(e) });
    },
    onSettled: refreshJobs,
  });
  const compactProgress = usePushed<CompactSummary>(COMPACT_PROGRESS_KEY);
  // The latest Shrink index run, from the cache: still "Shrinking…" after a remount.
  const compactRuns = useMutationState({
    filters: { mutationKey: COMPACT_KEY },
    select: (m) => m.state as MutationState<CompactSummary, unknown, void>,
  });
  const lastCompact = compactRuns.length > 0 ? compactRuns[compactRuns.length - 1] : null;
  const shrinking = lastCompact?.status === "pending";
  const reclaiming = useIsMutating({ mutationKey: VACUUM_KEY }) > 0;
  // The shared header stats (refetched on index changes) carry the index's free pages.
  const reclaimable = useQuery({ queryKey: ["db_stats"], queryFn: api.dbStats }).data?.reclaimable;
  // A snapshot is a copy of the index as it was: threads deleted here stay in the ones
  // taken earlier (and come back with a restore), so the confirmation says so.
  const snapshotsKept =
    (useQuery({ queryKey: ["index_snapshots"], queryFn: api.indexSnapshots, staleTime: 60_000 })
      .data?.snapshots.length ?? 0) > 0;
  // Unknown (an older backend): leave it enabled, the backend applies the same rule.
  const nothingToReclaim = reclaimable != null && !reclaimable.worthwhile;
  // A rewrite must not overlap another rewrite, or a reindex / semantic build / distill
  // writing to the same file: it would wait out the lock and fail, or redo the work.
  const rewriteRunning = useIsMutating({ mutationKey: REWRITE_KEY }) > 0;
  const jobBusy = anyJobRunning(useJobsStatus().data);
  const rewriting = rewriteRunning || jobBusy;

  /** Start a rewrite after a fresh job check. The status is polled only while a job runs,
   *  so one that started while every flag was idle shows up here: the fetch refreshes the
   *  shared query, which disables the buttons, and nothing starts. */
  async function startRewrite(run: () => void) {
    // A failed check (an older backend without jobs_status) doesn't block: the backend
    // refuses an overlapping rewrite itself.
    const busy = await fetchJobsStatus(queryClient).then(anyJobRunning, () => false);
    // A second click while the check was in flight finds the first rewrite started.
    if (!busy && queryClient.isMutating({ mutationKey: REWRITE_KEY }) === 0) run();
  }

  /** Toggle selection for the rows on the current page only. */
  function setPageSelected(value: boolean) {
    form.setFieldValue(
      "rows",
      form.state.values.rows.map((r, i) =>
        i >= start && i < start + PAGE_SIZE ? { ...r, selected: value } : r,
      ),
    );
    setConfirming(false);
  }

  function onDeleteClick() {
    const ids = form.state.values.rows.filter((r) => r.selected).map((r) => r.id);
    if (ids.length === 0 || del.isPending) return;
    if (!confirming) {
      setConfirming(true);
      return;
    }
    del.mutate(ids);
  }

  return (
    <SettingsGroup title="Storage">
      <SettingRow
        title="Shrink index"
        description="Trim tool output stored by older versions to the size new indexing keeps."
        control={
          <Button
            size="sm"
            variant="outline"
            onClick={() => void startRewrite(() => compact.mutate())}
            disabled={rewriting}
          >
            {shrinking && <Spinner data-icon="inline-start" />}
            {shrinking ? "Shrinking…" : "Shrink index"}
          </Button>
        }
      >
        {jobBusy && !rewriteRunning && (
          <Note>
            Shrink index and Reclaim space are available again once the background job that is
            running now (a reindex, semantic build, distill or tool output trim) finishes.
          </Note>
        )}
        {(shrinking || lastCompact?.data) && (
          <Note>
            {shrinking
              ? compactProgress && compactProgress.rows > 0
                ? `Trimmed ${compactProgress.rows.toLocaleString()} tool outputs so far…`
                : "Looking for oversized tool output…"
              : lastCompact?.data && lastCompact.data.rows > 0
                ? `Trimmed ${lastCompact.data.rows.toLocaleString()} tool outputs. ${savedNote(lastCompact.data)}`
                : "Nothing to trim: the index is already compact."}
          </Note>
        )}
      </SettingRow>

      <SnapshotsRow />

      <SettingRow
        title="Reclaim disk space"
        description="Rewrite the database to return freed space to disk. Available once at least 50 MB and 1% of the index are free."
        control={
          <Button
            size="sm"
            variant="outline"
            onClick={() => void startRewrite(() => vacuum.mutate())}
            disabled={rewriting || nothingToReclaim}
          >
            {reclaiming && <Spinner data-icon="inline-start" />}
            {reclaiming ? "Reclaiming…" : "Reclaim space"}
          </Button>
        }
      >
        {reclaimable != null && <Note>Reclaimable: {fmtBytes(reclaimable.bytes)}</Note>}
      </SettingRow>

      <SettingRow
        title="Delete old threads"
        description="Least-recently-active first. Deleting cascades to messages, search and embeddings."
      >
        <div>
          <label htmlFor={ageId} className="sr-only">
            Thread age
          </label>
          <Select
            value={age}
            onValueChange={(v) => setAge(v ?? "all")}
            items={AGES.map((a) => ({ value: a.value, label: a.label }))}
          >
            <SelectTrigger id={ageId} size="sm" className="w-52">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {AGES.map((a) => (
                <SelectItem key={a.value} value={a.value}>
                  {a.label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>

        {/* Full-bleed (cancels the row's px-4) so the hairlines meet the group's dividers. */}
        <div className="-mx-4 border-t">
          {/* Edge cells pad to the row's 16px so text lines up with the titles above. */}
          <Table className="table-fixed [&_tr>*:first-child]:pl-4 [&_tr>*:last-child]:pr-4">
            <TableHeader className="[&_th]:h-8 [&_th]:text-xs [&_th]:font-medium [&_th]:normal-case [&_th]:tracking-normal [&_th]:text-muted-foreground">
              <TableRow className="hover:bg-transparent">
                <TableHead className="w-11">
                  <form.Subscribe selector={(s) => s.values.rows}>
                    {(formRows) => {
                      const pageRows = formRows.slice(start, start + PAGE_SIZE);
                      const allSel = pageRows.length > 0 && pageRows.every((r) => r.selected);
                      return (
                        <Checkbox
                          checked={allSel}
                          onCheckedChange={(c) => setPageSelected(c === true)}
                          disabled={pageRows.length === 0}
                          aria-label="Select all on this page"
                        />
                      );
                    }}
                  </form.Subscribe>
                </TableHead>
                <TableHead>Thread</TableHead>
                <TableHead className="w-24">Source</TableHead>
                <TableHead className="w-16 text-right">Size</TableHead>
                <TableHead className="w-28 text-right">Last active</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {rows.isLoading && (
                <TableRow className="hover:bg-transparent">
                  <TableCell colSpan={5} className="text-muted-foreground">
                    <span className="inline-flex items-center gap-2">
                      <Spinner className="size-3.5" /> Loading…
                    </span>
                  </TableCell>
                </TableRow>
              )}
              {!rows.isLoading && total === 0 && (
                <TableRow className="hover:bg-transparent">
                  <TableCell colSpan={5} className="text-muted-foreground">
                    No threads in this range.
                  </TableCell>
                </TableRow>
              )}
              {/* Subscribed to the rows array itself, not an array-mode Field: in
                  @tanstack/react-form 1.33 an array-mode field re-renders only on
                  push/remove (`_arrayVersion`), so after `form.reset` seeded the list the
                  table stayed empty. */}
              <form.Subscribe selector={(s) => s.values.rows}>
                {(formRows) =>
                  formRows.slice(start, start + PAGE_SIZE).map((row, j) => {
                    const i = start + j;
                    const title = row.title?.trim() || "(untitled)";
                    return (
                      <TableRow key={row.id}>
                        <TableCell>
                          {/* Not the form's CheckboxField: each row box needs its own label. */}
                          <form.Field name={`rows[${i}].selected`}>
                            {(f) => (
                              <Checkbox
                                checked={f.state.value}
                                onCheckedChange={(c) => f.handleChange(c === true)}
                                aria-label={`Select ${title}`}
                              />
                            )}
                          </form.Field>
                        </TableCell>
                        <TableCell className="truncate text-foreground">{title}</TableCell>
                        <TableCell className="truncate text-xs text-muted-foreground">
                          {SOURCE_LABELS[row.source] ?? row.source}
                        </TableCell>
                        <TableCell className="text-right text-xs tabular-nums text-muted-foreground">
                          {fmtBytes(row.bytes)}
                        </TableCell>
                        <TableCell className="text-right text-xs text-muted-foreground">
                          {formatTime(row.updatedAt)}
                        </TableCell>
                      </TableRow>
                    );
                  })
                }
              </form.Subscribe>
            </TableBody>
          </Table>
        </div>

        {total > 0 && (
          <div className="flex items-center justify-between gap-2 text-xs text-muted-foreground">
            <span>
              {total} thread{total === 1 ? "" : "s"}
              {total >= FETCH_LIMIT ? "+" : ""}
            </span>
            <div className="flex items-center gap-2">
              <Button
                size="xs"
                variant="outline"
                onClick={() => setPage(safePage - 1)}
                disabled={safePage <= 0}
              >
                <ChevronLeft data-icon="inline-start" />
                Prev
              </Button>
              <span>
                Page {safePage + 1} of {pageCount}
              </span>
              <Button
                size="xs"
                variant="outline"
                onClick={() => setPage(safePage + 1)}
                disabled={safePage >= pageCount - 1}
              >
                Next
                <ChevronRight data-icon="inline-end" />
              </Button>
            </div>
          </div>
        )}

        {err && <InlineError>{err}</InlineError>}

        <form.Subscribe selector={(s) => s.values.rows}>
          {(formRows) => {
            const sel = formRows.filter((r) => r.selected);
            const bytes = sel.reduce((n, r) => n + r.bytes, 0);
            return (
              <div className="flex items-center justify-between gap-2">
                <Note>
                  {confirming
                    ? `Delete ${sel.length} permanently? This can't be undone.${
                        snapshotsKept
                          ? " Index snapshots taken earlier still hold them until they are replaced; Reclaim space removes the automatic ones."
                          : ""
                      }`
                    : sel.length > 0
                      ? `${sel.length} selected · ${fmtBytes(bytes)}`
                      : "Nothing selected"}
                </Note>
                <div className="flex shrink-0 gap-2">
                  {confirming && !del.isPending && (
                    <Button size="sm" variant="ghost" onClick={() => setConfirming(false)}>
                      Cancel
                    </Button>
                  )}
                  <Button
                    size="sm"
                    variant="destructive"
                    onClick={onDeleteClick}
                    disabled={sel.length === 0 || del.isPending}
                  >
                    {del.isPending && <Spinner data-icon="inline-start" />}
                    {del.isPending
                      ? "Deleting…"
                      : confirming
                        ? `Confirm delete (${sel.length})`
                        : `Delete selected${sel.length > 0 ? ` (${sel.length})` : ""}`}
                  </Button>
                </div>
              </div>
            );
          }}
        </form.Subscribe>
      </SettingRow>
    </SettingsGroup>
  );
}
