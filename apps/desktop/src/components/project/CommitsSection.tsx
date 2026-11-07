import { useState } from "react";
import { useMutation, useQueries, useQuery, useQueryClient } from "@tanstack/react-query";
import { Check, GitCommitHorizontal } from "lucide-react";
import { api, type CommitLink, type ThreadSummary } from "../../lib/api";
import { copyText } from "../../lib/clipboard";
import { errorText } from "../../lib/toast";
import { Button } from "@/components/ui/button";
import { Spinner } from "@/components/ui/spinner";
import { SectionNote, SectionSkeleton } from "./ProjectSection";
import { fullDate, recentSessionsQuery, relativeAge, useOpenThread } from "./shared";

/** The commits the recent sessions likely produced (file-overlap links from `git log`),
 *  grouped under the session that wrote them. */
export function CommitsSection({ project }: { project: string }) {
  const qc = useQueryClient();
  const recent = useQuery(recentSessionsQuery(project));
  const threads = recent.data ?? [];
  // Stored links only (one indexed read per thread); Scan git recomputes them.
  const commits = useQueries({
    queries: threads.map((t) => ({
      queryKey: ["thread_commits", t.id],
      queryFn: () => api.threadCommits(t.id),
    })),
  });
  // One scan links the whole repo (gitlink::link_project, which runs `git log` in the given
  // session's own folder), so it runs once and every row then rereads its stored links. It
  // goes through the newest session that ran in this page's repo itself: the newest one
  // overall may have run in a worktree, whose HEAD misses this checkout's branch, or in a
  // worktree since removed. Only when every recent session ran in a worktree does it fall
  // back to the newest.
  const scanThread = threads.find((t) => t.projectPath === project) ?? threads[0];
  const scan = useMutation({
    mutationFn: () => api.linkThreadCommits(scanThread.id),
    onSuccess: () => {
      for (const t of threads) void qc.invalidateQueries({ queryKey: ["thread_commits", t.id] });
    },
  });

  if (recent.isPending || commits.some((c) => c.isPending)) return <SectionSkeleton rows={2} />;
  if (recent.isError) {
    return (
      <SectionNote tone="error">
        Couldn't load the recent sessions: {errorText(recent.error)}
      </SectionNote>
    );
  }
  if (threads.length === 0) {
    return <SectionNote>No sessions in this repo yet, so no commits to link.</SectionNote>;
  }

  // A failed read must not pass for "no commits": the empty state offers a scan that would
  // not fix it.
  const failed = commits.find((c) => c.isError);
  if (failed) {
    return (
      <SectionNote tone="error">
        Couldn't load the linked commits: {errorText(failed.error)}
      </SectionNote>
    );
  }

  const groups = threads
    .map((t, i) => ({ thread: t, commits: commits[i]?.data ?? [] }))
    .filter((g) => g.commits.length > 0);

  const scanButton = (
    <Button size="xs" variant="outline" onClick={() => scan.mutate()} disabled={scan.isPending}>
      {scan.isPending ? (
        <>
          <Spinner data-icon="inline-start" className="size-3" />
          Reading git log…
        </>
      ) : (
        <>
          <GitCommitHorizontal data-icon="inline-start" />
          {groups.length ? "Rescan git history" : "Scan git history"}
        </>
      )}
    </Button>
  );

  return (
    <div className="space-y-3">
      {groups.length === 0 ? (
        <div className="flex flex-wrap items-center gap-x-3 gap-y-2 px-1">
          <p className="text-sm text-muted-foreground">
            {scan.isSuccess
              ? "No commits in this repo's git history touch the files these sessions edited."
              : `No commits linked to the last ${threads.length} sessions yet. Scanning reads this repo's git log and matches commits to the files each session touched.`}
          </p>
          {scanButton}
        </div>
      ) : (
        <>
          <ul className="space-y-3" aria-label="Produced commits">
            {groups.map((g) => (
              <CommitGroup key={g.thread.id} thread={g.thread} commits={g.commits} />
            ))}
          </ul>
          <div className="flex items-center gap-2 px-1">
            {scanButton}
            <span className="text-xs text-muted-foreground">
              Linked by files touched, so a commit can show under more than one session.
            </span>
          </div>
        </>
      )}
      {scan.isError && (
        <SectionNote tone="error">
          Couldn't read git: {errorText(scan.error)}. Check that the repo folder still exists and
          git is on your PATH.
        </SectionNote>
      )}
    </div>
  );
}

function CommitGroup({ thread, commits }: { thread: ThreadSummary; commits: CommitLink[] }) {
  const openThread = useOpenThread();
  return (
    <li>
      <button
        type="button"
        onClick={() => openThread(thread.id)}
        className="mb-1 flex max-w-full cursor-pointer items-baseline gap-1.5 rounded-md px-1 text-left text-[13px] outline-none hover:text-foreground focus-visible:ring-[3px] focus-visible:ring-ring/50"
        title="Open the session"
      >
        <span className="truncate font-medium text-foreground">
          {thread.title || "Untitled session"}
        </span>
        <span className="shrink-0 tabular-nums text-muted-foreground">
          · {relativeAge(thread.updatedAt)}
        </span>
      </button>
      <ul className="divide-y divide-border overflow-hidden rounded-xl border bg-card">
        {commits.map((c) => (
          <CommitRow key={c.sha} commit={c} />
        ))}
      </ul>
    </li>
  );
}

function CommitRow({ commit }: { commit: CommitLink }) {
  const [copied, setCopied] = useState(false);
  return (
    <li className="flex items-center gap-2.5 px-3 py-2 text-sm">
      <button
        type="button"
        onClick={() =>
          copyText(commit.sha).then(() => {
            setCopied(true);
            setTimeout(() => setCopied(false), 1200);
          })
        }
        title={`Copy ${commit.sha}`}
        aria-label={`Copy commit ${commit.shortSha}`}
        className="flex shrink-0 cursor-pointer items-center gap-1 rounded-md bg-muted px-1.5 py-0.5 font-mono text-xs text-foreground outline-none transition-colors hover:bg-accent focus-visible:ring-2 focus-visible:ring-ring/50"
      >
        {copied && <Check aria-hidden="true" className="size-3" />}
        {copied ? "Copied" : commit.shortSha}
      </button>
      <span className="min-w-0 flex-1 truncate" title={commit.subject ?? undefined}>
        {commit.subject || "(no subject)"}
      </span>
      <span
        className="shrink-0 text-xs tabular-nums text-muted-foreground"
        title={fullDate(commit.committedAt)}
      >
        {relativeAge(commit.committedAt)}
      </span>
    </li>
  );
}
