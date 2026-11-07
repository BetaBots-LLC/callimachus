import { useEffect, useRef, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { ArrowUpRight, FileText } from "lucide-react";
import { api, type Snapshot } from "../../lib/api";
import { snapshotEventText } from "../../lib/format";
import { errorText, notify } from "../../lib/toast";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetHeader,
  SheetTitle,
} from "@/components/ui/sheet";
import { Spinner } from "@/components/ui/spinner";
import { Markdown } from "../Markdown";
import { RowList, SectionNote, SectionSkeleton } from "./ProjectSection";
import {
  AgentMenu,
  CopyButton,
  focusSuccessor,
  formatChars,
  fullDate,
  relativeAge,
  useCanLaunchCli,
  useOpenThread,
} from "./shared";

/** How many checkpoints the section lists. Auto snapshots keep one slot per thread and
 *  event, so a busy repo has about one per recent session. */
const SNAPSHOT_LIMIT = 20;

/** The display name of a snapshot: a named one's label, else its number; an auto one's
 *  event in words ("Before compaction"), since its label ("auto · PreCompact") only repeats
 *  the event. */
function snapshotName(s: Snapshot): string {
  if (s.event === "manual") return s.label.trim() || `Snapshot #${s.id}`;
  const text = snapshotEventText(s.event);
  return text.charAt(0).toUpperCase() + text.slice(1);
}

/** Checkpoints of this repo's sessions (the PreCompact hook, `cal snapshot`): read one in a
 *  drawer, hand it to any agent CLI, or delete it. */
export function SnapshotsSection({ project }: { project: string }) {
  const snapshots = useQuery({
    queryKey: ["snapshots", project],
    queryFn: () => api.listSnapshots(project, SNAPSHOT_LIMIT),
  });
  // The drawer's snapshot outlives its open state, so the sheet keeps its content through
  // the slide-out instead of emptying the moment it starts closing.
  const [reading, setReading] = useState<Snapshot | null>(null);
  const [drawerOpen, setDrawerOpen] = useState(false);
  const canLaunch = useCanLaunchCli();

  if (snapshots.isPending) return <SectionSkeleton rows={2} />;
  if (snapshots.isError) {
    return (
      <SectionNote tone="error">
        Couldn't load the snapshots: {errorText(snapshots.error)}
      </SectionNote>
    );
  }
  if (snapshots.data.length === 0) {
    return (
      <SectionNote>
        No snapshots for this repo. The PreCompact hook saves one before Claude Code compacts a
        session, and <code className="font-mono text-xs whitespace-nowrap">cal snapshot</code> saves
        one by hand.
      </SectionNote>
    );
  }
  return (
    <>
      <RowList label="Snapshots">
        {snapshots.data.map((s) => (
          <SnapshotRow
            key={s.id}
            snapshot={s}
            project={project}
            canLaunch={canLaunch}
            onRead={() => {
              setReading(s);
              setDrawerOpen(true);
            }}
          />
        ))}
      </RowList>
      <SnapshotDrawer
        snapshot={reading}
        open={drawerOpen}
        canLaunch={canLaunch}
        onClose={() => setDrawerOpen(false)}
      />
    </>
  );
}

function useResumeSnapshot(snapshot: Snapshot | null) {
  return useMutation({
    mutationFn: (agent: string) => api.resumeSnapshot((snapshot as Snapshot).id, agent),
  });
}

function SnapshotRow({
  snapshot,
  project,
  canLaunch,
  onRead,
}: {
  snapshot: Snapshot;
  project: string;
  canLaunch: boolean;
  onRead: () => void;
}) {
  const qc = useQueryClient();
  const resume = useResumeSnapshot(snapshot);
  const [confirming, setConfirming] = useState(false);
  const rowRef = useRef<HTMLLIElement>(null);
  const deleteRef = useRef<HTMLButtonElement>(null);
  // Set by a cancel: the confirm's buttons unmount, so focus goes back to Delete.
  const refocusDelete = useRef(false);
  useEffect(() => {
    if (!confirming && refocusDelete.current) {
      refocusDelete.current = false;
      deleteRef.current?.focus();
    }
  }, [confirming]);
  const cancelConfirm = () => {
    refocusDelete.current = true;
    setConfirming(false);
  };
  const remove = useMutation({
    mutationFn: () => api.deleteSnapshot(snapshot.id),
    onSuccess: () => {
      // The row (and the focused confirm button in it) unmounts below.
      focusSuccessor(rowRef.current);
      qc.setQueryData<Snapshot[]>(["snapshots", project], (prev) =>
        prev?.filter((s) => s.id !== snapshot.id),
      );
      // Today lists the snapshots captured since the last open.
      void qc.invalidateQueries({ queryKey: ["snapshots"] });
      void qc.invalidateQueries({ queryKey: ["today"] });
      notify.success(`Deleted the snapshot "${snapshotName(snapshot)}"`);
    },
    // A failed delete keeps the row: focus goes back to its Delete button.
    onError: () => {
      refocusDelete.current = true;
    },
    onSettled: () => setConfirming(false),
  });
  const failure = resume.isError
    ? `Couldn't open the agent: ${errorText(resume.error)}`
    : remove.isError
      ? `Couldn't delete the snapshot: ${errorText(remove.error)}`
      : null;

  return (
    <li
      ref={rowRef}
      className="flex items-center gap-2 px-1.5 py-1 transition-colors hover:bg-muted/40 has-[>button:focus-visible]:bg-muted/40"
    >
      <button
        type="button"
        onClick={onRead}
        title="Read the checkpoint"
        className="min-w-0 flex-1 cursor-pointer rounded-md px-1.5 py-1.5 text-left outline-none focus-visible:ring-[3px] focus-visible:ring-ring/50"
      >
        <span className="flex min-w-0 items-center gap-2">
          <span className="truncate text-sm text-foreground">{snapshotName(snapshot)}</span>
          {/* A named snapshot's event is "manual"; the hook's own get a badge. */}
          {snapshot.event !== "manual" && (
            <Badge variant="outline" className="shrink-0 text-[0.65rem]">
              Auto
            </Badge>
          )}
        </span>
        <span className="mt-0.5 flex min-w-0 gap-1.5 text-[13px] text-muted-foreground">
          <span className="min-w-0 truncate">{snapshot.threadTitle || "Untitled session"}</span>
          <span className="shrink-0 tabular-nums" title={fullDate(snapshot.createdAt)}>
            · {relativeAge(snapshot.createdAt)}
          </span>
          <span className="shrink-0 tabular-nums">· {formatChars(snapshot.chars)}</span>
        </span>
        {failure && (
          <span role="alert" className="mt-1 block text-xs text-destructive">
            {failure}
          </span>
        )}
      </button>
      <div className="flex shrink-0 items-center gap-1 pr-1.5">
        {confirming ? (
          // Inline, not a modal: the row stays in view, Cancel is the default focus, and
          // Escape cancels as it would in a dialog.
          <fieldset
            aria-label="Confirm delete"
            className="m-0 flex min-w-0 items-center gap-1 border-0 p-0"
            onKeyDown={(e) => {
              if (e.key === "Escape" && !remove.isPending) {
                e.stopPropagation();
                cancelConfirm();
              }
            }}
          >
            <span className="text-xs text-muted-foreground">Delete it?</span>
            <Button
              size="xs"
              variant="destructive"
              onClick={() => remove.mutate()}
              disabled={remove.isPending}
            >
              {remove.isPending ? "Deleting…" : "Delete"}
            </Button>
            <Button
              size="xs"
              variant="ghost"
              autoFocus
              onClick={cancelConfirm}
              disabled={remove.isPending}
            >
              Cancel
            </Button>
          </fieldset>
        ) : (
          <>
            {canLaunch && (
              <AgentMenu
                label="Resume this checkpoint in…"
                onPick={(agent) => resume.mutate(agent)}
                trigger={
                  <Button size="xs" variant="outline" disabled={resume.isPending}>
                    {resume.isPending ? (
                      <>
                        <Spinner data-icon="inline-start" className="size-3" />
                        Opening…
                      </>
                    ) : (
                      <>
                        Resume in
                        <ArrowUpRight data-icon="inline-end" />
                      </>
                    )}
                  </Button>
                }
              />
            )}
            <Button ref={deleteRef} size="xs" variant="ghost" onClick={() => setConfirming(true)}>
              Delete
            </Button>
          </>
        )}
      </div>
    </li>
  );
}

/** The checkpoint body, in a right-side drawer over the page so the list keeps its place. */
function SnapshotDrawer({
  snapshot,
  open,
  canLaunch,
  onClose,
}: {
  snapshot: Snapshot | null;
  open: boolean;
  canLaunch: boolean;
  onClose: () => void;
}) {
  const openThread = useOpenThread();
  const resume = useResumeSnapshot(snapshot);
  const detail = useQuery({
    queryKey: ["snapshot", snapshot?.id],
    queryFn: () => api.loadSnapshot((snapshot as Snapshot).id),
    enabled: snapshot !== null,
    // A checkpoint never changes once written.
    staleTime: Number.POSITIVE_INFINITY,
  });

  return (
    <Sheet open={open && snapshot !== null} onOpenChange={(next) => !next && onClose()}>
      <SheetContent side="right" className="gap-0 data-[side=right]:sm:max-w-xl">
        {snapshot && (
          <>
            <SheetHeader className="gap-1 border-b pr-12">
              <SheetTitle className="truncate">{snapshotName(snapshot)}</SheetTitle>
              <SheetDescription className="truncate text-[13px]">
                {[
                  snapshot.threadTitle || "Untitled session",
                  relativeAge(snapshot.createdAt),
                  formatChars(snapshot.chars),
                ].join(" · ")}
              </SheetDescription>
              <div className="flex flex-wrap items-center gap-1.5 pt-2">
                {canLaunch && (
                  <AgentMenu
                    label="Resume this checkpoint in…"
                    onPick={(agent) => resume.mutate(agent)}
                    trigger={
                      <Button size="xs" disabled={resume.isPending}>
                        {resume.isPending ? "Opening…" : "Resume in"}
                        <ArrowUpRight data-icon="inline-end" />
                      </Button>
                    }
                  />
                )}
                <CopyButton text={detail.data?.body ?? ""} disabled={!detail.data} />
                {snapshot.threadId != null && (
                  <Button
                    size="xs"
                    variant="ghost"
                    onClick={() => {
                      onClose();
                      openThread(snapshot.threadId as number);
                    }}
                  >
                    <FileText data-icon="inline-start" />
                    Open session
                  </Button>
                )}
              </div>
              {resume.isError && (
                <p role="alert" className="text-xs text-destructive">
                  Couldn't open the agent: {errorText(resume.error)}
                </p>
              )}
            </SheetHeader>
            <div className="min-h-0 flex-1 overflow-y-auto px-4 py-3">
              {detail.isPending ? (
                <div className="flex items-center gap-2 text-sm text-muted-foreground">
                  <Spinner /> Loading the checkpoint…
                </div>
              ) : detail.isError ? (
                <SectionNote tone="error">
                  Couldn't load the checkpoint: {errorText(detail.error)}
                </SectionNote>
              ) : (
                // A checkpoint's headings are transcript structure, not page titles: keep them
                // at body scale so the drawer reads as a document, not a poster.
                <div className="[&_h1]:text-lg [&_h1]:leading-snug [&_h2]:text-base [&_h3]:text-sm">
                  <Markdown>{detail.data.body}</Markdown>
                </div>
              )}
            </div>
          </>
        )}
      </SheetContent>
    </Sheet>
  );
}
